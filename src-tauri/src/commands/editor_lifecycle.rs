#![allow(non_snake_case)]

use std::process::Command;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EditorAppTarget {
    ClaudeCode,
    ClaudeDesktop,
    Codex,
}

fn map_target(raw: &str) -> Result<EditorAppTarget, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "claude" | "claude-code" | "claudecode" => Ok(EditorAppTarget::ClaudeCode),
        "claude-desktop" | "claudedesktop" | "claude_desktop" => Ok(EditorAppTarget::ClaudeDesktop),
        "codex" => Ok(EditorAppTarget::Codex),
        other => Err(format!("Unsupported editor target: {other}")),
    }
}

/// Restart Cursor / VS Code / Claude Desktop used by the selected app.
#[tauri::command]
pub async fn restart_editor_app(target: String) -> Result<String, String> {
    let target = map_target(&target)?;
    tokio::task::spawn_blocking(move || restart_editor_app_blocking(target))
        .await
        .map_err(|e| e.to_string())?
}

fn restart_editor_app_blocking(target: EditorAppTarget) -> Result<String, String> {
    #[cfg(target_os = "windows")]
    {
        restart_windows(target)
    }
    #[cfg(target_os = "macos")]
    {
        restart_macos(target)
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        restart_linux(target)
    }
}

#[cfg(target_os = "windows")]
fn restart_windows(target: EditorAppTarget) -> Result<String, String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let (kill_images, launch_candidates): (&[&str], Vec<String>) = match target {
        EditorAppTarget::ClaudeDesktop => {
            let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
            let candidates = vec![
                format!(r"{local}\AnthropicClaude\claude.exe"),
                format!(r"{local}\Programs\Claude\Claude.exe"),
                r"C:\Program Files\Claude\Claude.exe".to_string(),
            ];
            (&["Claude.exe", "claude.exe"], candidates)
        }
        EditorAppTarget::ClaudeCode | EditorAppTarget::Codex => {
            // Claude Code / Codex IDE sessions usually run inside Cursor or VS Code.
            let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
            let candidates = vec![
                format!(r"{local}\Programs\cursor\Cursor.exe"),
                format!(r"{local}\Programs\Cursor\Cursor.exe"),
                r"C:\Program Files\Cursor\Cursor.exe".to_string(),
                format!(r"{local}\Programs\Microsoft VS Code\Code.exe"),
                r"C:\Program Files\Microsoft VS Code\Code.exe".to_string(),
            ];
            (&["Cursor.exe", "Code.exe", "code.exe"], candidates)
        }
    };

    for image in kill_images {
        let _ = Command::new("taskkill")
            .creation_flags(CREATE_NO_WINDOW)
            .args(["/IM", image, "/F", "/T"])
            .output();
    }

    // Brief wait so file locks are released.
    std::thread::sleep(std::time::Duration::from_millis(800));

    for path in &launch_candidates {
        if std::path::Path::new(path).is_file() {
            Command::new(path)
                .creation_flags(CREATE_NO_WINDOW)
                .spawn()
                .map_err(|e| format!("Failed to launch {path}: {e}"))?;
            return Ok(format!("Restarted via {path}"));
        }
    }

    // Fallback: rely on PATH / App Paths
    let fallback = match target {
        EditorAppTarget::ClaudeDesktop => "Claude",
        EditorAppTarget::ClaudeCode | EditorAppTarget::Codex => "cursor",
    };
    Command::new("cmd")
        .creation_flags(CREATE_NO_WINDOW)
        .args(["/C", "start", "", fallback])
        .spawn()
        .map_err(|e| format!("Failed to launch {fallback}: {e}"))?;
    Ok(format!("Restart requested for {fallback}"))
}

#[cfg(target_os = "macos")]
fn restart_macos(target: EditorAppTarget) -> Result<String, String> {
    let apps: &[&str] = match target {
        EditorAppTarget::ClaudeDesktop => &["Claude"],
        EditorAppTarget::ClaudeCode | EditorAppTarget::Codex => {
            &["Cursor", "Code", "Visual Studio Code"]
        }
    };
    for app in apps {
        let _ = Command::new("osascript")
            .args(["-e", &format!("tell application \"{app}\" to quit")])
            .output();
    }
    std::thread::sleep(std::time::Duration::from_millis(800));
    for app in apps {
        let status = Command::new("open").args(["-a", app]).status();
        if matches!(status, Ok(s) if s.success()) {
            return Ok(format!("Restarted {app}"));
        }
    }
    Err("Failed to relaunch editor app on macOS".into())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn restart_linux(target: EditorAppTarget) -> Result<String, String> {
    let (patterns, binaries): (&[&str], &[&str]) = match target {
        EditorAppTarget::ClaudeDesktop => {
            (&["claude-desktop", "claude"], &["claude-desktop", "claude"])
        }
        EditorAppTarget::ClaudeCode | EditorAppTarget::Codex => {
            (&["cursor", "code"], &["cursor", "code"])
        }
    };
    for pat in patterns {
        let _ = Command::new("pkill").args(["-f", pat]).output();
    }
    std::thread::sleep(std::time::Duration::from_millis(500));
    for bin in binaries {
        if Command::new("sh")
            .args(["-lc", &format!("command -v {bin}")])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            Command::new("sh")
                .args(["-lc", &format!("nohup {bin} >/dev/null 2>&1 &")])
                .spawn()
                .map_err(|e| format!("Failed to launch {bin}: {e}"))?;
            return Ok(format!("Restarted {bin}"));
        }
    }
    Err("Failed to relaunch editor app on Linux".into())
}
