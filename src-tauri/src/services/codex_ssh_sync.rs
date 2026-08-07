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

fn build_ssh_base_args(host: &CodexSshHost) -> Vec<String> {
    let mut args = vec![
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        "StrictHostKeyChecking=accept-new".to_string(),
        "-o".to_string(),
        "ConnectTimeout=20".to_string(),
        "-p".to_string(),
        host.port.to_string(),
    ];
    if let Some(identity) = host.identity_file.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty())
    {
        args.push("-i".to_string());
        args.push(identity.to_string());
    }
    args
}

fn run_command(program: &Path, args: &[String]) -> Result<String, AppError> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| {
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
    if !config.is_file() {
        return Err(AppError::localized(
            "codex_ssh_sync.config_missing",
            format!("本地缺少 Codex 网关配置: {}", config.display()),
            format!("Local Codex config missing: {}", config.display()),
        ));
    }

    let mut files = vec![(config, "config.toml".to_string())];
    let auth = get_codex_auth_path();
    if auth.is_file() {
        files.push((auth, "auth.json".to_string()));
    }
    let catalog = get_codex_model_catalog_path();
    if catalog.is_file() {
        let name = catalog
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("cc-switch-model-catalog.json")
            .to_string();
        files.push((catalog, name));
    }
    Ok(files)
}

fn build_scp_base_args(host: &CodexSshHost) -> Vec<String> {
    let mut args = vec![
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        "StrictHostKeyChecking=accept-new".to_string(),
        "-o".to_string(),
        "ConnectTimeout=20".to_string(),
        "-P".to_string(),
        host.port.to_string(),
    ];
    if let Some(identity) = host
        .identity_file
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        args.push("-i".to_string());
        args.push(identity.to_string());
    }
    args
}

fn remote_target(host: &CodexSshHost) -> String {
    format!("{}@{}", host.user, host.host)
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

    let mut ssh_args = build_ssh_base_args(host);
    ssh_args.push(target.clone());
    ssh_args.push(format!(
        "mkdir -p {} && chmod 700 {}",
        shell_quote(remote_dir),
        shell_quote(remote_dir)
    ));
    run_command(&ssh, &ssh_args)?;

    let mut synced_files = Vec::new();
    for (local_path, remote_name) in &files {
        let mut args = build_scp_base_args(host);
        args.push(local_path.display().to_string());
        args.push(format!("{target}:{remote_dir}/{remote_name}"));
        run_command(&scp, &args)?;
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
    let _ = run_command(&ssh, &chmod_args);

    let message = format!(
        "已同步 {} 个文件到 {target}:{}",
        synced_files.len(),
        remote_dir
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
    let out = run_command(&ssh, &args)?;
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
        sync_scripts_dir().join(format!("{}.cmd", host.id))
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

    let auth = get_codex_auth_path();
    let config = get_codex_config_path();
    let catalog = get_codex_model_catalog_path();
    let remote_dir = if host.remote_codex_dir.trim().is_empty() {
        "~/.codex".to_string()
    } else {
        host.remote_codex_dir.clone()
    };
    let target = remote_target(host);
    let identity = host
        .identity_file
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty());

    #[cfg(windows)]
    {
        let mut content = String::from("@echo off\r\nsetlocal\r\n");
        content.push_str("set \"ERR=0\"\r\n");
        let id_arg = identity
            .map(|i| format!(" -i \"{i}\""))
            .unwrap_or_default();
        content.push_str(&format!(
            "ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new -p {port}{id_arg} {target} \"mkdir -p {remote} && chmod 700 {remote}\" || set ERR=1\r\n",
            port = host.port,
            remote = remote_dir,
        ));
        content.push_str(&format!(
            "scp -o BatchMode=yes -P {port}{id_arg} \"{config}\" {target}:{remote}/config.toml || set ERR=1\r\n",
            port = host.port,
            config = config.display(),
            remote = remote_dir,
        ));
        if auth.is_file() {
            content.push_str(&format!(
                "if exist \"{auth}\" scp -o BatchMode=yes -P {port}{id_arg} \"{auth}\" {target}:{remote}/auth.json || set ERR=1\r\n",
                auth = auth.display(),
                port = host.port,
                remote = remote_dir,
            ));
        }
        if catalog.is_file() {
            let name = catalog
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("cc-switch-model-catalog.json");
            content.push_str(&format!(
                "if exist \"{catalog}\" scp -o BatchMode=yes -P {port}{id_arg} \"{catalog}\" {target}:{remote}/{name} || set ERR=1\r\n",
                catalog = catalog.display(),
                port = host.port,
                remote = remote_dir,
                name = name,
            ));
        }
        content.push_str("exit /b %ERR%\r\n");
        fs::write(&path, content).map_err(|e| AppError::io(&path, e))?;
    }

    #[cfg(not(windows))]
    {
        let mut content = String::from("#!/usr/bin/env bash\nset -euo pipefail\n");
        let id_args = identity
            .map(|i| format!(" -i {}", shell_quote(i)))
            .unwrap_or_default();
        content.push_str(&format!(
            "SSH_OPTS=(-o BatchMode=yes -o StrictHostKeyChecking=accept-new -p {port}{id_args})\n",
            port = host.port,
            id_args = id_args,
        ));
        content.push_str(&format!(
            "ssh \"${{SSH_OPTS[@]}}\" {target} {mkdir_cmd}\n",
            target = shell_quote(&target),
            mkdir_cmd = shell_quote(&format!(
                "mkdir -p {} && chmod 700 {}",
                remote_dir, remote_dir
            )),
        ));
        content.push_str(&format!(
            "scp -o BatchMode=yes -P {port}{id_args} {config} {target}:{remote}/config.toml\n",
            port = host.port,
            id_args = id_args,
            config = shell_quote(&config.display().to_string()),
            target = shell_quote(&target),
            remote = remote_dir,
        ));
        content.push_str(&format!(
            "if [[ -f {auth} ]]; then scp -o BatchMode=yes -P {port}{id_args} {auth} {target}:{remote}/auth.json; fi\n",
            auth = shell_quote(&auth.display().to_string()),
            port = host.port,
            id_args = id_args,
            target = shell_quote(&target),
            remote = remote_dir,
        ));
        if catalog.is_file() {
            let name = catalog
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("cc-switch-model-catalog.json");
            content.push_str(&format!(
                "if [[ -f {catalog} ]]; then scp -o BatchMode=yes -P {port}{id_args} {catalog} {target}:{remote}/{name}; fi\n",
                catalog = shell_quote(&catalog.display().to_string()),
                port = host.port,
                id_args = id_args,
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

fn render_ssh_host_block(host: &CodexSshHost, script_path: &Path, proxy_port: u16) -> String {
    let alias = host.resolve_alias();
    let mut block = String::new();
    block.push_str(&format!("# BEGIN CC-SWITCH CODEX SSH SYNC {}\n", host.id));
    block.push_str(&format!("Host {alias}\n"));
    block.push_str(&format!("  HostName {}\n", host.host));
    block.push_str(&format!("  User {}\n", host.user));
    if host.port != 22 {
        block.push_str(&format!("  Port {}\n", host.port));
    }
    if let Some(identity) = host.identity_file.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty())
    {
        block.push_str(&format!("  IdentityFile {identity}\n"));
    }
    if host.sync_on_ssh_connect {
        block.push_str("  PermitLocalCommand yes\n");
        #[cfg(windows)]
        {
            block.push_str(&format!(
                "  LocalCommand \"{}\"\n",
                script_path.display().to_string().replace('\\', "/")
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
    settings.normalize();
    settings.validate()?;
    install_connect_hooks(&settings)?;
    settings::set_codex_ssh_sync_settings(Some(settings.clone()))?;
    Ok(settings)
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
