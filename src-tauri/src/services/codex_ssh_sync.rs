//! Sync local Codex gateway config + auth to remote SSH hosts.
//!
//! Used by:
//! - Auto-sync after local live config writes (provider switch / proxy takeover)
//! - Manual "Sync Now" from settings
//! - SSH `LocalCommand` hooks so every Codex/Cursor SSH connect pushes latest files first

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::codex_config::{
    get_codex_auth_path, get_codex_config_path, get_codex_model_catalog_path,
};
use crate::config::get_app_config_dir;
use crate::error::AppError;
use crate::settings::{
    self, CodexSshHost, CodexSshSyncSettings, update_codex_ssh_host_status,
};

const SSH_INCLUDE_FILENAME: &str = "cc-switch-codex-sync.conf";
const DEFAULT_PROXY_PORT: u16 = 15721;

static AUTO_SYNC_RUNNING: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexSshSyncHostResult {
    pub host_id: String,
    pub host: String,
    pub success: bool,
    pub message: String,
    pub synced_files: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexSshSyncResult {
    pub success: bool,
    pub results: Vec<CodexSshSyncHostResult>,
}

fn now_epoch_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn resolve_proxy_listen_port() -> u16 {
    // Best-effort: prefer live proxy config via env override, else default.
    if let Ok(raw) = std::env::var("CC_SWITCH_PROXY_PORT") {
        if let Ok(port) = raw.parse::<u16>() {
            if port > 0 {
                return port;
            }
        }
    }
    DEFAULT_PROXY_PORT
}

fn ssh_home_dir() -> Result<PathBuf, AppError> {
    dirs::home_dir().ok_or_else(|| {
        AppError::localized(
            "codex_ssh_sync.home_missing",
            "无法确定用户主目录",
            "Unable to resolve home directory.",
        )
    })
}

fn sync_scripts_dir() -> PathBuf {
    get_app_config_dir().join("codex-ssh-sync")
}

fn which_cmd(name: &str) -> Option<PathBuf> {
    #[cfg(windows)]
    let candidates = [
        format!("{name}.exe"),
        name.to_string(),
        format!("{name}.cmd"),
    ];
    #[cfg(not(windows))]
    let candidates = [name.to_string()];

    if let Ok(path_env) = std::env::var("PATH") {
        #[cfg(windows)]
        let sep = ';';
        #[cfg(not(windows))]
        let sep = ':';
        for dir in path_env.split(sep) {
            for candidate in &candidates {
                let full = Path::new(dir).join(candidate);
                if full.is_file() {
                    return Some(full);
                }
            }
        }
    }
    None
}

fn require_ssh_tools() -> Result<(PathBuf, PathBuf), AppError> {
    let ssh = which_cmd("ssh").ok_or_else(|| {
        AppError::localized(
            "codex_ssh_sync.ssh_missing",
            "未找到 ssh 命令，请先安装 OpenSSH 客户端",
            "ssh command not found. Install OpenSSH client first.",
        )
    })?;
    let scp = which_cmd("scp").ok_or_else(|| {
        AppError::localized(
            "codex_ssh_sync.scp_missing",
            "未找到 scp 命令，请先安装 OpenSSH 客户端",
            "scp command not found. Install OpenSSH client first.",
        )
    })?;
    Ok((ssh, scp))
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':' | '@' | '~'))
    {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn host_identity(host: &CodexSshHost) -> Option<&str> {
    host.identity_file
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
}

fn host_password(host: &CodexSshHost) -> Option<&str> {
    // 密钥优先：有私钥时不用密码
    if host_identity(host).is_some() {
        return None;
    }
    host.password
        .as_ref()
        .map(|s| s.as_str())
        .filter(|s| !s.is_empty())
}

fn uses_password_auth(host: &CodexSshHost) -> bool {
    host_password(host).is_some()
}

/// Ephemeral SSH_ASKPASS helper so password auth works without a console prompt.
struct AskPassGuard {
    script_path: PathBuf,
    pass_path: PathBuf,
}

impl Drop for AskPassGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.pass_path);
        let _ = fs::remove_file(&self.script_path);
    }
}

fn write_askpass_files(password: &str, script_path: &Path, pass_path: &Path) -> Result<(), AppError> {
    if let Some(parent) = script_path.parent() {
        fs::create_dir_all(parent).map_err(|e| AppError::io(parent, e))?;
    }
    fs::write(pass_path, password.as_bytes()).map_err(|e| AppError::io(pass_path, e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(pass_path)
            .map_err(|e| AppError::io(pass_path, e))?
            .permissions();
        perms.set_mode(0o600);
        fs::set_permissions(pass_path, perms).map_err(|e| AppError::io(pass_path, e))?;
    }

    #[cfg(windows)]
    {
        let content = format!(
            "@echo off\r\nsetlocal\r\ntype \"{pass}\"\r\n",
            pass = pass_path.display()
        );
        fs::write(script_path, content).map_err(|e| AppError::io(script_path, e))?;
    }
    #[cfg(not(windows))]
    {
        let content = format!("#!/bin/sh\ncat {}\n", shell_quote(&pass_path.display().to_string()));
        fs::write(script_path, content).map_err(|e| AppError::io(script_path, e))?;
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(script_path)
            .map_err(|e| AppError::io(script_path, e))?
            .permissions();
        perms.set_mode(0o700);
        fs::set_permissions(script_path, perms).map_err(|e| AppError::io(script_path, e))?;
    }
    Ok(())
}

fn ephemeral_askpass(password: &str) -> Result<AskPassGuard, AppError> {
    let dir = sync_scripts_dir().join("askpass-tmp");
    fs::create_dir_all(&dir).map_err(|e| AppError::io(&dir, e))?;
    let stamp = now_epoch_ms();
    #[cfg(windows)]
    let script_path = dir.join(format!("askpass-{stamp}.cmd"));
    #[cfg(not(windows))]
    let script_path = dir.join(format!("askpass-{stamp}.sh"));
    let pass_path = dir.join(format!("askpass-{stamp}.pass"));
    write_askpass_files(password, &script_path, &pass_path)?;
    Ok(AskPassGuard {
        script_path,
        pass_path,
    })
}

fn persistent_askpass_paths(host: &CodexSshHost) -> (PathBuf, PathBuf) {
    let dir = sync_scripts_dir();
    #[cfg(windows)]
    let script = dir.join(format!("{}.askpass.cmd", host.id));
    #[cfg(not(windows))]
    let script = dir.join(format!("{}.askpass.sh", host.id));
    let pass = dir.join(format!("{}.pass", host.id));
    (script, pass)
}

fn ensure_persistent_askpass(host: &CodexSshHost) -> Result<Option<(PathBuf, PathBuf)>, AppError> {
    let Some(password) = host_password(host) else {
        let (script, pass) = persistent_askpass_paths(host);
        let _ = fs::remove_file(script);
        let _ = fs::remove_file(pass);
        return Ok(None);
    };
    let (script, pass) = persistent_askpass_paths(host);
    write_askpass_files(password, &script, &pass)?;
    Ok(Some((script, pass)))
}

fn build_ssh_base_args(host: &CodexSshHost) -> Vec<String> {
    let mut args = vec![
        "-o".to_string(),
        "StrictHostKeyChecking=accept-new".to_string(),
        "-o".to_string(),
        "ConnectTimeout=20".to_string(),
        "-p".to_string(),
        host.port.to_string(),
    ];
    if uses_password_auth(host) {
        args.extend([
            "-o".to_string(),
            "BatchMode=no".to_string(),
            "-o".to_string(),
            "PreferredAuthentications=password,keyboard-interactive".to_string(),
            "-o".to_string(),
            "PubkeyAuthentication=no".to_string(),
            "-o".to_string(),
            "NumberOfPasswordPrompts=1".to_string(),
        ]);
    } else {
        args.extend(["-o".to_string(), "BatchMode=yes".to_string()]);
        if let Some(identity) = host_identity(host) {
            args.push("-i".to_string());
            args.push(identity.to_string());
            args.extend(["-o".to_string(), "IdentitiesOnly=yes".to_string()]);
        }
    }
    args
}

fn apply_windows_no_window(cmd: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let _ = cmd;
}

fn run_command(program: &Path, args: &[String], host: &CodexSshHost) -> Result<String, AppError> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    apply_windows_no_window(&mut cmd);

    let _askpass_guard;
    if let Some(password) = host_password(host) {
        let guard = ephemeral_askpass(password)?;
        cmd.env("SSH_ASKPASS", &guard.script_path);
        cmd.env("SSH_ASKPASS_REQUIRE", "force");
        // OpenSSH only invokes ASKPASS when it believes a display is available.
        cmd.env("DISPLAY", "cc-switch:0");
        cmd.env_remove("SSH_AUTH_SOCK");
        _askpass_guard = Some(guard);
    } else {
        _askpass_guard = None;
    }

    let output = cmd.output().map_err(|e| {
        AppError::localized(
            "codex_ssh_sync.command_failed",
            format!("执行 {} 失败: {e}", program.display()),
            format!("Failed to run {}: {e}", program.display()),
        )
    })?;

    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).trim().to_string());
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let detail = if !stderr.is_empty() {
        stderr
    } else if !stdout.is_empty() {
        stdout
    } else {
        format!("exit {}", output.status)
    };
    Err(AppError::localized(
        "codex_ssh_sync.remote_failed",
        format!("远程操作失败: {detail}"),
        format!("Remote operation failed: {detail}"),
    ))
}

fn local_files_to_sync() -> Result<Vec<(PathBuf, String)>, AppError> {
    let config = get_codex_config_path();
    let auth = get_codex_auth_path();
    let catalog = get_codex_model_catalog_path();

    let mut files = Vec::new();
    if config.is_file() {
        files.push((config, "config.toml".to_string()));
    }
    if auth.is_file() {
        files.push((auth.clone(), "auth.json".to_string()));
    }
    if catalog.is_file() {
        let name = catalog
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("cc-switch-model-catalog.json")
            .to_string();
        files.push((catalog, name));
    }

    if files.is_empty() {
        return Err(AppError::localized(
            "codex_ssh_sync.local_missing",
            format!(
                "本地没有可同步的 Codex 文件。请先在本机完成「登录 Codex」，或在 CC Switch 切换/接管 Codex 供应商以生成 {} / {}",
                get_codex_config_path().display(),
                get_codex_auth_path().display()
            ),
            format!(
                "No local Codex files to sync. Sign in to Codex locally first, or switch/take over a Codex provider in CC Switch so {} / {} exist.",
                get_codex_config_path().display(),
                get_codex_auth_path().display()
            ),
        ));
    }

    if !auth.is_file() {
        log::warn!(
            "Codex SSH sync: auth.json missing at {}; remote may still show login required",
            get_codex_auth_path().display()
        );
    }
    Ok(files)
}

fn build_scp_base_args(host: &CodexSshHost) -> Vec<String> {
    let mut args = vec![
        "-o".to_string(),
        "StrictHostKeyChecking=accept-new".to_string(),
        "-o".to_string(),
        "ConnectTimeout=20".to_string(),
        "-P".to_string(),
        host.port.to_string(),
    ];
    if uses_password_auth(host) {
        args.extend([
            "-o".to_string(),
            "BatchMode=no".to_string(),
            "-o".to_string(),
            "PreferredAuthentications=password,keyboard-interactive".to_string(),
            "-o".to_string(),
            "PubkeyAuthentication=no".to_string(),
            "-o".to_string(),
            "NumberOfPasswordPrompts=1".to_string(),
        ]);
    } else {
        args.extend(["-o".to_string(), "BatchMode=yes".to_string()]);
        if let Some(identity) = host_identity(host) {
            args.push("-i".to_string());
            args.push(identity.to_string());
            args.extend(["-o".to_string(), "IdentitiesOnly=yes".to_string()]);
        }
    }
    args
}

fn remote_target(host: &CodexSshHost) -> String {
    format!("{}@{}", host.user, host.host)
}

/// Ensure Codex CLI exists on the remote host (npm/pnpm global install).
fn ensure_remote_codex_cli(ssh: &Path, host: &CodexSshHost) -> Result<String, AppError> {
    let target = remote_target(host);
    let install_script = r#"
set -e
export PATH="$HOME/.local/bin:/usr/local/bin:/opt/homebrew/bin:$PATH"
# Load common Node version managers when present.
[ -s "$HOME/.nvm/nvm.sh" ] && . "$HOME/.nvm/nvm.sh"
[ -s "$HOME/.local/share/fnm/fnm" ] && eval "$("$HOME/.local/share/fnm/fnm" env)"
if command -v codex >/dev/null 2>&1; then
  echo "ALREADY:$(command -v codex)"
  exit 0
fi
if command -v npm >/dev/null 2>&1; then
  npm install -g @openai/codex >/tmp/cc-switch-codex-install.log 2>&1
  command -v codex >/dev/null 2>&1 && echo "INSTALLED_NPM:$(command -v codex)" && exit 0
fi
if command -v pnpm >/dev/null 2>&1; then
  pnpm add -g @openai/codex >/tmp/cc-switch-codex-install.log 2>&1
  command -v codex >/dev/null 2>&1 && echo "INSTALLED_PNPM:$(command -v codex)" && exit 0
fi
if command -v bun >/dev/null 2>&1; then
  bun add -g @openai/codex >/tmp/cc-switch-codex-install.log 2>&1
  command -v codex >/dev/null 2>&1 && echo "INSTALLED_BUN:$(command -v codex)" && exit 0
fi
echo "MISSING_NODE_OR_INSTALL_FAILED"
exit 2
"#;
    let mut args = build_ssh_base_args(host);
    args.push(target);
    args.push(format!("bash -lc {}", shell_quote(install_script.trim())));
    let out = run_command(ssh, &args, host)?;
    if out.contains("ALREADY:") || out.contains("INSTALLED_") {
        Ok(out)
    } else {
        Err(AppError::localized(
            "codex_ssh_sync.cli_install_failed",
            format!(
                "远程未安装 Codex CLI，且自动安装失败（需要 npm/pnpm）。输出: {out}"
            ),
            format!(
                "Remote Codex CLI missing and auto-install failed (npm/pnpm required). Output: {out}"
            ),
        ))
    }
}

/// Enable Codex remote-control features on the remote config (device control / phone).
fn ensure_remote_control_features(
    ssh: &Path,
    host: &CodexSshHost,
    remote_dir: &str,
) -> Result<(), AppError> {
    let target = remote_target(host);
    // Keep this shell-only so remotes without python3 still work.
    let patch = format!(
        r#"
set -e
CFG={cfg}
mkdir -p "$(dirname "$CFG")"
touch "$CFG"
if ! grep -q '^\[features\]' "$CFG" 2>/dev/null; then
  printf '\n[features]\nremote_connections = true\nremote_control = true\n' >> "$CFG"
else
  grep -q '^remote_connections' "$CFG" || sed -i '/^\[features\]/a remote_connections = true' "$CFG"
  grep -q '^remote_control' "$CFG" || sed -i '/^\[features\]/a remote_control = true' "$CFG"
  sed -i 's/^remote_connections.*/remote_connections = true/' "$CFG"
  sed -i 's/^remote_control.*/remote_control = true/' "$CFG"
fi
echo FEATURES_OK
"#,
        cfg = shell_quote(&format!("{remote_dir}/config.toml")),
    );
    let mut args = build_ssh_base_args(host);
    args.push(target);
    args.push(format!("bash -lc {}", shell_quote(patch.trim())));
    let _ = run_command(ssh, &args, host);
    Ok(())
}

pub fn remote_sessions_cache_dir(host_id: &str) -> PathBuf {
    get_app_config_dir()
        .join("remote-codex-sessions")
        .join(host_id)
}

/// Pull remote ~/.codex/sessions into a local cache for usage accounting.
pub fn pull_remote_sessions(host: &CodexSshHost) -> Result<usize, AppError> {
    let (ssh, scp) = require_ssh_tools()?;
    let target = remote_target(host);
    let remote_dir = if host.remote_codex_dir.trim().is_empty() {
        "~/.codex"
    } else {
        host.remote_codex_dir.trim()
    };
    let cache = remote_sessions_cache_dir(&host.id);
    let sessions_cache = cache.join("sessions");
    fs::create_dir_all(&sessions_cache).map_err(|e| AppError::io(&sessions_cache, e))?;

    // Ensure remote sessions dir exists (empty is fine).
    let mut check_args = build_ssh_base_args(host);
    check_args.push(target.clone());
    check_args.push(format!(
        "mkdir -p {d}/sessions; if [ -d {d}/sessions ]; then echo HAS_SESSIONS; else echo NO_SESSIONS; fi",
        d = shell_quote(remote_dir)
    ));
    let check = run_command(&ssh, &check_args, host)?;
    if !check.contains("HAS_SESSIONS") {
        return Ok(0);
    }

    // Prefer rsync when available (incremental); fall back to scp -r.
    // Skip rsync for password auth — SSH_ASKPASS is awkward via rsync -e.
    let used_rsync = if !uses_password_auth(host) {
        if let Some(rsync) = which_cmd("rsync") {
            let mut ssh_cmd = format!(
                "ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new -p {}",
                host.port
            );
            if let Some(identity) = host_identity(host) {
                ssh_cmd.push_str(&format!(" -i {}", shell_quote(identity)));
            }
            let rsync_args = vec![
                "-az".into(),
                "-e".into(),
                ssh_cmd,
                format!("{target}:{remote_dir}/sessions/"),
                format!("{}/", sessions_cache.display()),
            ];
            run_command(&rsync, &rsync_args, host).is_ok()
        } else {
            false
        }
    } else {
        false
    };

    if !used_rsync {
        // scp -r copies the folder itself; stage then move.
        let staging = cache.join("_staging");
        let _ = fs::remove_dir_all(&staging);
        fs::create_dir_all(&staging).map_err(|e| AppError::io(&staging, e))?;
        let mut args = build_scp_base_args(host);
        args.insert(0, "-r".to_string());
        args.push(format!("{target}:{remote_dir}/sessions"));
        args.push(staging.display().to_string());
        run_command(&scp, &args, host)?;
        let copied = staging.join("sessions");
        if copied.is_dir() {
            let _ = fs::remove_dir_all(&sessions_cache);
            fs::rename(&copied, &sessions_cache).map_err(|e| AppError::io(&sessions_cache, e))?;
        }
        let _ = fs::remove_dir_all(&staging);
    }

    // Count jsonl files
    let mut count = 0usize;
    let mut stack = vec![sessions_cache];
    while let Some(dir) = stack.pop() {
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                    count += 1;
                }
            }
        }
    }
    Ok(count)
}

pub fn pull_remote_sessions_for_usage() -> usize {
    let Some(settings) = settings::get_codex_ssh_sync_settings() else {
        return 0;
    };
    if !settings.enabled {
        return 0;
    }
    let mut total = 0usize;
    for host in settings.hosts.iter().filter(|h| h.enabled) {
        match pull_remote_sessions(host) {
            Ok(n) => {
                total += n;
                log::info!(
                    "Pulled {n} remote Codex session files from {}",
                    host.host
                );
            }
            Err(e) => log::warn!(
                "Pull remote Codex sessions from {} failed: {e}",
                host.host
            ),
        }
    }
    total
}

pub fn list_remote_session_cache_roots() -> Vec<PathBuf> {
    let root = get_app_config_dir().join("remote-codex-sessions");
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(root) {
        for entry in entries.flatten() {
            let sessions = entry.path().join("sessions");
            if sessions.is_dir() {
                out.push(entry.path());
            }
        }
    }
    out
}

pub fn sync_host(host: &CodexSshHost) -> Result<CodexSshSyncHostResult, AppError> {
    let (ssh, scp) = require_ssh_tools()?;
    let files = local_files_to_sync()?;
    let target = remote_target(host);
    let remote_dir = host.remote_codex_dir.trim();
    let remote_dir = if remote_dir.is_empty() {
        "~/.codex"
    } else {
        remote_dir
    };

    // 1) Install Codex CLI when missing (fixes "未安装 Codex CLI")
    let cli_status = ensure_remote_codex_cli(&ssh, host)?;

    let mut ssh_args = build_ssh_base_args(host);
    ssh_args.push(target.clone());
    ssh_args.push(format!(
        "mkdir -p {} && chmod 700 {}",
        shell_quote(remote_dir),
        shell_quote(remote_dir)
    ));
    run_command(&ssh, &ssh_args, host)?;

    // 2) Replace gateway + auth files
    let mut synced_files = Vec::new();
    for (local_path, remote_name) in &files {
        let mut args = build_scp_base_args(host);
        args.push(local_path.display().to_string());
        args.push(format!("{target}:{remote_dir}/{remote_name}"));
        run_command(&scp, &args, host)?;
        synced_files.push(remote_name.clone());
    }

    let mut chmod_args = build_ssh_base_args(host);
    chmod_args.push(target.clone());
    chmod_args.push(format!(
        "chmod 600 {dir}/config.toml 2>/dev/null; \
         [ -f {dir}/auth.json ] && chmod 600 {dir}/auth.json; \
         true",
        dir = shell_quote(remote_dir)
    ));
    let _ = run_command(&ssh, &chmod_args, host);

    // 3) Enable remote-control features for phone/device control
    let _ = ensure_remote_control_features(&ssh, host, remote_dir);

    // 4) Pull sessions for usage stats (best effort)
    let pulled = pull_remote_sessions(host).unwrap_or(0);

    let message = format!(
        "CLI:{cli_status}; 已同步 {} 个文件到 {target}:{remote_dir}; 拉取会话 {pulled} 个",
        synced_files.len(),
    );
    update_codex_ssh_host_status(&host.id, Some(now_epoch_ms()), None)?;

    Ok(CodexSshSyncHostResult {
        host_id: host.id.clone(),
        host: host.host.clone(),
        success: true,
        message,
        synced_files,
    })
}

pub fn sync_hosts(hosts: &[CodexSshHost]) -> CodexSshSyncResult {
    let mut results = Vec::new();
    let mut all_ok = true;
    for host in hosts {
        if !host.enabled {
            continue;
        }
        match sync_host(host) {
            Ok(result) => results.push(result),
            Err(err) => {
                all_ok = false;
                let msg = err.to_string();
                let _ = update_codex_ssh_host_status(&host.id, None, Some(msg.clone()));
                results.push(CodexSshSyncHostResult {
                    host_id: host.id.clone(),
                    host: host.host.clone(),
                    success: false,
                    message: msg,
                    synced_files: vec![],
                });
            }
        }
    }
    CodexSshSyncResult {
        success: all_ok && !results.is_empty(),
        results,
    }
}

pub fn sync_enabled_hosts() -> Result<CodexSshSyncResult, AppError> {
    let settings = settings::get_codex_ssh_sync_settings().ok_or_else(|| {
        AppError::localized(
            "codex_ssh_sync.not_configured",
            "未配置 Codex SSH 同步",
            "Codex SSH sync is not configured.",
        )
    })?;
    if !settings.enabled {
        return Err(AppError::localized(
            "codex_ssh_sync.disabled",
            "Codex SSH 同步未启用",
            "Codex SSH sync is disabled.",
        ));
    }
    let hosts: Vec<_> = settings
        .hosts
        .into_iter()
        .filter(|h| h.enabled)
        .collect();
    if hosts.is_empty() {
        return Err(AppError::localized(
            "codex_ssh_sync.no_hosts",
            "没有启用的 SSH 主机",
            "No enabled SSH hosts.",
        ));
    }
    Ok(sync_hosts(&hosts))
}

pub fn sync_auto_hosts() -> CodexSshSyncResult {
    let Some(settings) = settings::get_codex_ssh_sync_settings() else {
        return CodexSshSyncResult {
            success: true,
            results: vec![],
        };
    };
    if !settings.enabled {
        return CodexSshSyncResult {
            success: true,
            results: vec![],
        };
    }
    let hosts: Vec<_> = settings
        .hosts
        .into_iter()
        .filter(|h| h.enabled && h.auto_sync)
        .collect();
    if hosts.is_empty() {
        return CodexSshSyncResult {
            success: true,
            results: vec![],
        };
    }
    sync_hosts(&hosts)
}

/// Fire-and-forget auto sync after local Codex live writes.
pub fn schedule_auto_sync_after_live_write() {
    if !settings::get_codex_ssh_sync_settings()
        .map(|s| s.enabled && s.hosts.iter().any(|h| h.enabled && h.auto_sync))
        .unwrap_or(false)
    {
        return;
    }
    if AUTO_SYNC_RUNNING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    std::thread::spawn(|| {
        let result = sync_auto_hosts();
        for item in &result.results {
            if item.success {
                log::info!(
                    "Codex SSH sync ok host={} files={:?}",
                    item.host,
                    item.synced_files
                );
            } else {
                log::warn!("Codex SSH sync failed host={}: {}", item.host, item.message);
            }
        }
        AUTO_SYNC_RUNNING.store(false, Ordering::SeqCst);
    });
}

pub fn test_host_connection(host: &CodexSshHost) -> Result<String, AppError> {
    let (ssh, _) = require_ssh_tools()?;
    let mut args = build_ssh_base_args(host);
    args.push(remote_target(host));
    args.push("echo cc-switch-codex-ssh-ok".to_string());
    let out = run_command(&ssh, &args, host)?;
    if out.contains("cc-switch-codex-ssh-ok") {
        Ok("SSH connection ok".to_string())
    } else {
        Err(AppError::localized(
            "codex_ssh_sync.test_unexpected",
            format!("SSH 测试返回异常: {out}"),
            format!("Unexpected SSH test output: {out}"),
        ))
    }
}

fn helper_script_path(host: &CodexSshHost) -> PathBuf {
    #[cfg(windows)]
    {
        sync_scripts_dir().join(format!("{}.ps1", host.id))
    }
    #[cfg(not(windows))]
    {
        sync_scripts_dir().join(format!("{}.sh", host.id))
    }
}

fn write_helper_script(host: &CodexSshHost) -> Result<PathBuf, AppError> {
    let dir = sync_scripts_dir();
    fs::create_dir_all(&dir).map_err(|e| AppError::io(&dir, e))?;
    let path = helper_script_path(host);
    let askpass = ensure_persistent_askpass(host)?;

    let auth = get_codex_auth_path();
    let config = get_codex_config_path();
    let catalog = get_codex_model_catalog_path();
    let remote_dir = if host.remote_codex_dir.trim().is_empty() {
        "~/.codex".to_string()
    } else {
        host.remote_codex_dir.clone()
    };
    let target = remote_target(host);
    let identity = host_identity(host);

    #[cfg(windows)]
    {
        // PowerShell helper — LocalCommand launches it with -WindowStyle Hidden.
        let mut content = String::from("$ErrorActionPreference = 'Continue'\n$err = 0\n");
        if let Some((ask_script, _)) = &askpass {
            content.push_str(&format!(
                "$env:SSH_ASKPASS = '{}'\n$env:SSH_ASKPASS_REQUIRE = 'force'\n$env:DISPLAY = 'cc-switch:0'\nRemove-Item Env:SSH_AUTH_SOCK -ErrorAction SilentlyContinue\n",
                ask_script.display().to_string().replace('\'', "''")
            ));
            content.push_str(
                "$sshOpts = @('-o','BatchMode=no','-o','StrictHostKeyChecking=accept-new','-o','PreferredAuthentications=password,keyboard-interactive','-o','PubkeyAuthentication=no','-o','NumberOfPasswordPrompts=1','-p',",
            );
            content.push_str(&format!("'{}')\n", host.port));
            content.push_str(
                "$scpOpts = @('-o','BatchMode=no','-o','StrictHostKeyChecking=accept-new','-o','PreferredAuthentications=password,keyboard-interactive','-o','PubkeyAuthentication=no','-o','NumberOfPasswordPrompts=1','-P',",
            );
            content.push_str(&format!("'{}')\n", host.port));
        } else {
            content.push_str(
                "$sshOpts = @('-o','BatchMode=yes','-o','StrictHostKeyChecking=accept-new','-p',",
            );
            content.push_str(&format!("'{}')\n", host.port));
            content.push_str(
                "$scpOpts = @('-o','BatchMode=yes','-o','StrictHostKeyChecking=accept-new','-P',",
            );
            content.push_str(&format!("'{}')\n", host.port));
            if let Some(id) = identity {
                let id_esc = id.replace('\'', "''");
                content.push_str(&format!(
                    "$sshOpts += @('-i','{id_esc}','-o','IdentitiesOnly=yes')\n$scpOpts += @('-i','{id_esc}','-o','IdentitiesOnly=yes')\n"
                ));
            }
        }
        let target_esc = target.replace('\'', "''");
        let remote_esc = remote_dir.replace('\'', "''");
        content.push_str(&format!(
            "& ssh @sshOpts '{target_esc}' \"mkdir -p {remote_esc} && chmod 700 {remote_esc}\"; if ($LASTEXITCODE -ne 0) {{ $err = 1 }}\n"
        ));
        content.push_str(&format!(
            "& scp @scpOpts '{config}' '{target_esc}:{remote_esc}/config.toml'; if ($LASTEXITCODE -ne 0) {{ $err = 1 }}\n",
            config = config.display().to_string().replace('\'', "''"),
        ));
        if auth.is_file() {
            content.push_str(&format!(
                "if (Test-Path -LiteralPath '{auth}') {{ & scp @scpOpts '{auth}' '{target_esc}:{remote_esc}/auth.json'; if ($LASTEXITCODE -ne 0) {{ $err = 1 }} }}\n",
                auth = auth.display().to_string().replace('\'', "''"),
            ));
        }
        if catalog.is_file() {
            let name = catalog
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("cc-switch-model-catalog.json");
            content.push_str(&format!(
                "if (Test-Path -LiteralPath '{catalog}') {{ & scp @scpOpts '{catalog}' '{target_esc}:{remote_esc}/{name}'; if ($LASTEXITCODE -ne 0) {{ $err = 1 }} }}\n",
                catalog = catalog.display().to_string().replace('\'', "''"),
                name = name,
            ));
        }
        content.push_str("exit $err\n");
        fs::write(&path, content).map_err(|e| AppError::io(&path, e))?;
    }

    #[cfg(not(windows))]
    {
        let mut content = String::from("#!/usr/bin/env bash\nset -euo pipefail\n");
        if let Some((ask_script, _)) = &askpass {
            content.push_str(&format!(
                "export SSH_ASKPASS={}\nexport SSH_ASKPASS_REQUIRE=force\nexport DISPLAY=cc-switch:0\nunset SSH_AUTH_SOCK || true\n",
                shell_quote(&ask_script.display().to_string())
            ));
            content.push_str(&format!(
                "SSH_OPTS=(-o BatchMode=no -o StrictHostKeyChecking=accept-new -o PreferredAuthentications=password,keyboard-interactive -o PubkeyAuthentication=no -o NumberOfPasswordPrompts=1 -p {port})\n",
                port = host.port,
            ));
            content.push_str(&format!(
                "SCP_OPTS=(-o BatchMode=no -o StrictHostKeyChecking=accept-new -o PreferredAuthentications=password,keyboard-interactive -o PubkeyAuthentication=no -o NumberOfPasswordPrompts=1 -P {port})\n",
                port = host.port,
            ));
        } else {
            let id_args = identity
                .map(|i| format!(" -i {} -o IdentitiesOnly=yes", shell_quote(i)))
                .unwrap_or_default();
            content.push_str(&format!(
                "SSH_OPTS=(-o BatchMode=yes -o StrictHostKeyChecking=accept-new -p {port}{id_args})\n",
                port = host.port,
                id_args = id_args,
            ));
            content.push_str(&format!(
                "SCP_OPTS=(-o BatchMode=yes -o StrictHostKeyChecking=accept-new -P {port}{id_args})\n",
                port = host.port,
                id_args = id_args,
            ));
        }
        content.push_str(&format!(
            "ssh \"${{SSH_OPTS[@]}}\" {target} {mkdir_cmd}\n",
            target = shell_quote(&target),
            mkdir_cmd = shell_quote(&format!(
                "mkdir -p {} && chmod 700 {}",
                remote_dir, remote_dir
            )),
        ));
        content.push_str(&format!(
            "scp \"${{SCP_OPTS[@]}}\" {config} {target}:{remote}/config.toml\n",
            config = shell_quote(&config.display().to_string()),
            target = shell_quote(&target),
            remote = remote_dir,
        ));
        content.push_str(&format!(
            "if [[ -f {auth} ]]; then scp \"${{SCP_OPTS[@]}}\" {auth} {target}:{remote}/auth.json; fi\n",
            auth = shell_quote(&auth.display().to_string()),
            target = shell_quote(&target),
            remote = remote_dir,
        ));
        if catalog.is_file() {
            let name = catalog
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("cc-switch-model-catalog.json");
            content.push_str(&format!(
                "if [[ -f {catalog} ]]; then scp \"${{SCP_OPTS[@]}}\" {catalog} {target}:{remote}/{name}; fi\n",
                catalog = shell_quote(&catalog.display().to_string()),
                target = shell_quote(&target),
                remote = remote_dir,
                name = name,
            ));
        }
        content.push_str(&format!(
            "ssh \"${{SSH_OPTS[@]}}\" {target} {chmod_cmd}\n",
            target = shell_quote(&target),
            chmod_cmd = shell_quote(&format!(
                "chmod 600 {d}/config.toml 2>/dev/null; [ -f {d}/auth.json ] && chmod 600 {d}/auth.json; true",
                d = remote_dir
            )),
        ));
        let mut file = fs::File::create(&path).map_err(|e| AppError::io(&path, e))?;
        file.write_all(content.as_bytes())
            .map_err(|e| AppError::io(&path, e))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&path)
                .map_err(|e| AppError::io(&path, e))?
                .permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&path, perms).map_err(|e| AppError::io(&path, e))?;
        }
    }

    Ok(path)
}

fn ensure_ssh_config_include(ssh_dir: &Path) -> Result<(), AppError> {
    let config_path = ssh_dir.join("config");
    let include_line = format!("Include {SSH_INCLUDE_FILENAME}");
    if config_path.is_file() {
        let existing = fs::read_to_string(&config_path).map_err(|e| AppError::io(&config_path, e))?;
        if existing.lines().any(|l| {
            let t = l.trim();
            t == include_line || t == format!("Include ~/.ssh/{SSH_INCLUDE_FILENAME}")
        }) {
            return Ok(());
        }
        let mut next = String::new();
        next.push_str(&include_line);
        next.push('\n');
        if !existing.is_empty() && !existing.starts_with('\n') {
            next.push('\n');
        }
        next.push_str(&existing);
        fs::write(&config_path, next).map_err(|e| AppError::io(&config_path, e))?;
    } else {
        fs::create_dir_all(ssh_dir).map_err(|e| AppError::io(ssh_dir, e))?;
        fs::write(&config_path, format!("{include_line}\n")).map_err(|e| AppError::io(&config_path, e))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&config_path)
            .map_err(|e| AppError::io(&config_path, e))?
            .permissions();
        perms.set_mode(0o600);
        let _ = fs::set_permissions(&config_path, perms);
    }
    Ok(())
}

fn render_ssh_host_patterns(host: &CodexSshHost) -> String {
    let alias = host.resolve_alias();
    let raw_host = host.host.trim();
    // Match both the friendly alias and the raw IP/hostname Cursor/Codex often use.
    if alias == raw_host || raw_host.is_empty() {
        alias
    } else {
        format!("{alias} {raw_host}")
    }
}

fn render_ssh_host_block(host: &CodexSshHost, script_path: &Path, proxy_port: u16) -> String {
    let patterns = render_ssh_host_patterns(host);
    let mut block = String::new();
    block.push_str(&format!("# BEGIN CC-SWITCH CODEX SSH SYNC {}\n", host.id));
    block.push_str(&format!("Host {patterns}\n"));
    block.push_str(&format!("  HostName {}\n", host.host.trim()));
    block.push_str(&format!("  User {}\n", host.user));
    if host.port != 22 {
        block.push_str(&format!("  Port {}\n", host.port));
    }
    if let Some(identity) = host_identity(host) {
        block.push_str(&format!("  IdentityFile {identity}\n"));
        block.push_str("  IdentitiesOnly yes\n");
    }
    // Cursor/Codex may invoke OpenSSH without an interactive TTY; keep LocalCommand on.
    if host.sync_on_ssh_connect {
        block.push_str("  PermitLocalCommand yes\n");
        #[cfg(windows)]
        {
            // Hidden PowerShell — avoids flashing console windows during connect sync.
            let ps1 = script_path.display().to_string().replace('\\', "/");
            block.push_str(&format!(
                "  LocalCommand powershell.exe -NoProfile -WindowStyle Hidden -ExecutionPolicy Bypass -File \"{ps1}\"\n"
            ));
        }
        #[cfg(not(windows))]
        {
            block.push_str(&format!(
                "  LocalCommand {}\n",
                shell_quote(&script_path.display().to_string())
            ));
        }
    }
    if host.forward_proxy {
        block.push_str(&format!(
            "  RemoteForward 127.0.0.1:{proxy_port} 127.0.0.1:{proxy_port}\n"
        ));
    }
    block.push_str("  ServerAliveInterval 30\n");
    block.push_str(&format!("# END CC-SWITCH CODEX SSH SYNC {}\n\n", host.id));
    block
}

/// Regenerate helper scripts + SSH Include file for connect-time sync.
pub fn install_connect_hooks(settings: &CodexSshSyncSettings) -> Result<PathBuf, AppError> {
    let home = ssh_home_dir()?;
    let ssh_dir = home.join(".ssh");
    fs::create_dir_all(&ssh_dir).map_err(|e| AppError::io(&ssh_dir, e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&ssh_dir)
            .map_err(|e| AppError::io(&ssh_dir, e))?
            .permissions();
        perms.set_mode(0o700);
        let _ = fs::set_permissions(&ssh_dir, perms);
    }

    let include_path = ssh_dir.join(SSH_INCLUDE_FILENAME);
    let proxy_port = resolve_proxy_listen_port();

    // Clean old scripts for hosts no longer present
    let scripts_dir = sync_scripts_dir();
    if scripts_dir.is_dir() {
        if let Ok(entries) = fs::read_dir(&scripts_dir) {
            let keep: std::collections::HashSet<String> =
                settings.hosts.iter().map(|h| h.id.clone()).collect();
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                let stem = name
                    .trim_end_matches(".askpass.cmd")
                    .trim_end_matches(".askpass.sh")
                    .trim_end_matches(".pass")
                    .trim_end_matches(".ps1")
                    .trim_end_matches(".sh")
                    .trim_end_matches(".cmd")
                    .to_string();
                if !keep.contains(&stem) {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
    }

    let mut include_content = String::from(
        "# Managed by CC Switch — Codex SSH sync (gateway + auth). Do not edit manually.\n\n",
    );

    if settings.enabled {
        for host in &settings.hosts {
            if !host.enabled {
                continue;
            }
            let script = write_helper_script(host)?;
            include_content.push_str(&render_ssh_host_block(host, &script, proxy_port));
        }
    }

    fs::write(&include_path, include_content).map_err(|e| AppError::io(&include_path, e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&include_path)
            .map_err(|e| AppError::io(&include_path, e))?
            .permissions();
        perms.set_mode(0o600);
        let _ = fs::set_permissions(&include_path, perms);
    }

    ensure_ssh_config_include(&ssh_dir)?;
    Ok(include_path)
}

pub fn save_settings_and_hooks(
    mut settings: CodexSshSyncSettings,
) -> Result<CodexSshSyncSettings, AppError> {
    let existing = settings::get_codex_ssh_sync_settings();
    settings::merge_codex_ssh_passwords(&mut settings, existing.as_ref());
    settings.normalize();
    settings.validate()?;
    install_connect_hooks(&settings)?;
    settings::set_codex_ssh_sync_settings(Some(settings.clone()))?;
    // Saving should immediately push gateway + auth, instead of waiting for the
    // next SSH connect (Codex "重启连接" may not run OpenSSH LocalCommand).
    if settings.enabled {
        let hosts: Vec<_> = settings
            .hosts
            .iter()
            .filter(|h| h.enabled && h.auto_sync)
            .cloned()
            .collect();
        if !hosts.is_empty() {
            let result = sync_hosts(&hosts);
            for item in &result.results {
                if item.success {
                    log::info!(
                        "Codex SSH sync after save ok host={} files={:?}",
                        item.host,
                        item.synced_files
                    );
                } else {
                    log::warn!(
                        "Codex SSH sync after save failed host={}: {}",
                        item.host,
                        item.message
                    );
                }
            }
        }
    }
    // Reload settings so lastSyncAt / lastError from sync are visible to UI.
    Ok(settings::get_codex_ssh_sync_settings().unwrap_or(settings))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_leaves_safe_tokens() {
        assert_eq!(shell_quote("root@1.2.3.4"), "root@1.2.3.4");
        assert!(shell_quote("a b").starts_with('\''));
    }

    #[test]
    fn resolve_alias_sanitizes_host() {
        let host = CodexSshHost {
            id: "h1".into(),
            name: "srv".into(),
            host: "1.2.3.4".into(),
            port: 22,
            user: "root".into(),
            identity_file: None,
            password: None,
            has_password: None,
            ssh_alias: None,
            remote_codex_dir: "~/.codex".into(),
            enabled: true,
            auto_sync: true,
            sync_on_ssh_connect: true,
            forward_proxy: true,
            last_sync_at: None,
            last_error: None,
        };
        assert_eq!(host.resolve_alias(), "cc-switch-1-2-3-4");
    }
}
