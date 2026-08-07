#![allow(non_snake_case)]

use serde_json::{json, Value};

use crate::error::AppError;
use crate::services::codex_ssh_sync as sync_service;
use crate::settings::{self, CodexSshHost, CodexSshSyncSettings};

fn not_configured() -> String {
    AppError::localized(
        "codex_ssh_sync.not_configured",
        "未配置 Codex SSH 同步",
        "Codex SSH sync is not configured.",
    )
    .to_string()
}

#[tauri::command]
pub async fn codex_ssh_sync_get_settings() -> Result<Option<CodexSshSyncSettings>, String> {
    Ok(settings::get_codex_ssh_sync_settings())
}

#[tauri::command]
pub async fn codex_ssh_sync_save_settings(
    settings: CodexSshSyncSettings,
) -> Result<Value, String> {
    let saved = sync_service::save_settings_and_hooks(settings).map_err(|e| e.to_string())?;
    Ok(json!({
        "success": true,
        "settings": saved,
    }))
}

#[tauri::command]
pub async fn codex_ssh_sync_now(
    hostId: Option<String>,
) -> Result<sync_service::CodexSshSyncResult, String> {
    let settings = settings::get_codex_ssh_sync_settings().ok_or_else(not_configured)?;
    if !settings.enabled {
        return Err(AppError::localized(
            "codex_ssh_sync.disabled",
            "Codex SSH 同步未启用",
            "Codex SSH sync is disabled.",
        )
        .to_string());
    }

    // Blocking SSH/SCP — run off the async runtime.
    let result = tokio::task::spawn_blocking(move || {
        if let Some(id) = hostId {
            let hosts: Vec<CodexSshHost> = settings
                .hosts
                .into_iter()
                .filter(|h| h.id == id)
                .collect();
            if hosts.is_empty() {
                return Err(AppError::localized(
                    "codex_ssh_sync.no_hosts",
                    "没有可同步的 SSH 主机",
                    "No SSH hosts to sync.",
                ));
            }
            Ok(sync_service::sync_hosts(&hosts))
        } else {
            sync_service::sync_enabled_hosts()
        }
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;
    Ok(result)
}

#[tauri::command]
pub async fn codex_ssh_sync_test_host(host: CodexSshHost) -> Result<Value, String> {
    let message = tokio::task::spawn_blocking(move || sync_service::test_host_connection(&host))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    Ok(json!({ "success": true, "message": message }))
}

#[tauri::command]
pub async fn codex_ssh_sync_install_hooks() -> Result<Value, String> {
    let settings = settings::get_codex_ssh_sync_settings().unwrap_or_default();
    let path = sync_service::install_connect_hooks(&settings).map_err(|e| e.to_string())?;
    Ok(json!({
        "success": true,
        "sshConfigInclude": path.display().to_string(),
    }))
}
