//! Tauri commands of the remote access feature. Each one runs its blocking
//! work (files, sockets, locks) off the IPC thread.

use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tauri::{AppHandle, Manager};

use super::emit::Status;
use super::hub::PublishPayload;
use super::manager::{PairStart, RemoteManager};
use super::net::{self, Iface};
use super::settings::RemoteSettings;

/// Run `work` against the managed [`RemoteManager`] on a blocking thread.
async fn with_manager<T: Send + 'static>(
    app: AppHandle,
    work: impl FnOnce(&RemoteManager) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let manager = app
        .try_state::<Arc<RemoteManager>>()
        .map(|state| Arc::clone(&state))
        .ok_or("remote access needs the app config directory")?;
    tauri::async_runtime::spawn_blocking(move || work(&manager))
        .await
        .map_err(|error| format!("join error: {error}"))?
}

#[tauri::command]
pub async fn remote_status(app: AppHandle) -> Result<Status, String> {
    with_manager(app, |manager| Ok(manager.status())).await
}

#[tauri::command]
pub async fn remote_interfaces() -> Result<Vec<Iface>, String> {
    tauri::async_runtime::spawn_blocking(net::interfaces)
        .await
        .map_err(|error| format!("join error: {error}"))?
}

#[tauri::command]
pub async fn remote_apply_settings(
    settings: RemoteSettings,
    app: AppHandle,
) -> Result<Status, String> {
    with_manager(app, move |manager| manager.apply(settings)).await
}

#[tauri::command]
pub async fn remote_pair_start(app: AppHandle) -> Result<PairStart, String> {
    with_manager(app, RemoteManager::pair_start).await
}

#[tauri::command]
pub async fn remote_pair_cancel(app: AppHandle) -> Result<(), String> {
    with_manager(app, |manager| {
        manager.pair_cancel();
        Ok(())
    })
    .await
}

#[tauri::command]
pub async fn remote_device_rename(
    id: String,
    name: String,
    app: AppHandle,
) -> Result<Status, String> {
    with_manager(app, move |manager| manager.device_rename(&id, &name)).await
}

#[tauri::command]
pub async fn remote_device_revoke(id: String, app: AppHandle) -> Result<Status, String> {
    with_manager(app, move |manager| manager.device_revoke(&id)).await
}

#[tauri::command]
pub async fn remote_regenerate_cert(app: AppHandle) -> Result<Status, String> {
    with_manager(app, RemoteManager::regenerate_cert).await
}

#[tauri::command]
pub async fn remote_publish(payload: PublishPayload, app: AppHandle) -> Result<(), String> {
    with_manager(app, move |manager| {
        manager.hub().publish(payload);
        Ok(())
    })
    .await
}

#[tauri::command]
pub async fn remote_action_result(
    req_id: String,
    ok: bool,
    error: Option<String>,
    app: AppHandle,
) -> Result<(), String> {
    with_manager(app, move |manager| {
        manager.hub().action_result(&req_id, ok, error.as_deref());
        Ok(())
    })
    .await
}

#[tauri::command]
pub async fn remote_ready(ready: bool, app: AppHandle) -> Result<(), String> {
    with_manager(app, move |manager| {
        manager.hub().set_ready(ready);
        Ok(())
    })
    .await
}

#[tauri::command]
pub async fn remote_push_test(app: AppHandle) -> Result<(), String> {
    with_manager(app, RemoteManager::push_test).await
}

/// `git config user.email`, to prefill the push contact; empty when git or
/// the setting is missing.
#[tauri::command]
pub async fn remote_default_contact() -> String {
    tauri::async_runtime::spawn_blocking(git_user_email)
        .await
        .unwrap_or_default()
}

const GIT_TIMEOUT: Duration = Duration::from_secs(2);

fn git_user_email() -> String {
    let mut command = Command::new("git");
    command
        .args(["config", "user.email"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    crate::child_path::apply_child_path(&mut command);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(crate::agent::spawn::CREATE_NO_WINDOW);
    }
    let Ok(mut child) = crate::spawn_gate::spawn(&mut command) else {
        return String::new();
    };
    let deadline = Instant::now() + GIT_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(Some(_)) | Err(_) => return String::new(),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return String::new();
            }
        }
    }
    let mut email = String::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = std::io::Read::read_to_string(&mut out, &mut email);
    }
    email.trim().to_owned()
}
