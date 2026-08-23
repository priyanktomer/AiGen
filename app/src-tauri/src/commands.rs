//! Tauri commands.
//!
//! Every one of these is a thin wrapper: validate what the WebView sent, call the engine, map
//! the error. Nothing here decides anything. That is deliberate — logic in this layer cannot be
//! tested without a window, so the layer that has to exist is kept as close to empty as it can
//! be, and `swiftload-core::manager` carries the behaviour and the tests.

use std::{path::PathBuf, sync::Arc};
use swiftload_core::{
    manager::{
        AddRequest, DownloadDetails, DownloadSummary, Manager, ManagerError, ProbePreview,
        RefreshReport, UiSettings, UrlHistoryRow,
    },
    scheduler::Priority,
    store::models::{DownloadStatus, UrlSource},
};
use tauri::State;
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_opener::OpenerExt;

type Res<T> = Result<T, ManagerError>;
type Mgr<'a> = State<'a, Arc<Manager>>;

#[tauri::command]
pub async fn probe_url(m: Mgr<'_>, url: String) -> Res<ProbePreview> {
    m.probe_url(&url).await
}

#[tauri::command]
pub async fn add_download(m: Mgr<'_>, request: AddRequest) -> Res<String> {
    let manager = m.inner().clone();
    manager.add(request).await
}

#[tauri::command]
pub fn pause(m: Mgr<'_>, id: String) -> Res<()> {
    m.pause(&id)
}

#[tauri::command]
pub fn resume(m: Mgr<'_>, id: String) -> Res<()> {
    m.inner().clone().resume(&id)
}

#[tauri::command]
pub fn retry(m: Mgr<'_>, id: String) -> Res<()> {
    m.inner().clone().retry(&id)
}

#[tauri::command]
pub fn cancel(m: Mgr<'_>, id: String) -> Res<()> {
    m.cancel(&id)
}

#[tauri::command]
pub fn remove(m: Mgr<'_>, id: String, delete_file: bool) -> Res<()> {
    m.remove(&id, delete_file)
}

#[tauri::command]
pub fn set_priority(m: Mgr<'_>, id: String, priority: Priority) -> bool {
    m.set_priority(&id, priority)
}

#[tauri::command]
pub fn start_now(m: Mgr<'_>, id: String) -> bool {
    m.inner().clone().start_now(&id)
}

#[tauri::command]
pub fn list_downloads(m: Mgr<'_>, status: Option<DownloadStatus>) -> Res<Vec<DownloadSummary>> {
    m.list(status)
}

#[tauri::command]
pub fn get_details(m: Mgr<'_>, id: String) -> Res<DownloadDetails> {
    m.details(&id)
}

#[tauri::command]
pub fn find_existing_for(m: Mgr<'_>, identity_hint: String) -> Res<Vec<DownloadSummary>> {
    m.find_existing_for(&identity_hint)
}

#[tauri::command]
pub fn get_url_history(m: Mgr<'_>, id: String) -> Res<Vec<UrlHistoryRow>> {
    m.url_history(&id)
}

/// Hand back the unredacted link. A deliberate user action, and recorded as one.
#[tauri::command]
pub fn reveal_full_url(m: Mgr<'_>, id: String) -> Res<String> {
    m.reveal_full_url(&id)
}

#[tauri::command]
pub fn subscribe_connections(m: Mgr<'_>, id: String) {
    m.subscribe_connections(&id);
}

#[tauri::command]
pub fn unsubscribe_connections(m: Mgr<'_>, id: String) {
    m.unsubscribe_connections(&id);
}

#[tauri::command]
pub fn get_settings(m: Mgr<'_>) -> UiSettings {
    m.ui_settings()
}

#[tauri::command]
pub fn set_settings(m: Mgr<'_>, settings: UiSettings) -> Res<()> {
    m.set_ui_settings(&settings)
}

#[tauri::command]
pub async fn validate_replacement_url(m: Mgr<'_>, id: String, url: String) -> Res<RefreshReport> {
    m.validate_replacement_url(&id, &url).await
}

/// Swap in a replacement link and resume.
///
/// `accept_confirm` is the user having clicked through the "looks like the same file" card. It
/// cannot turn a rejection into a resume — the engine re-checks and refuses either way.
#[tauri::command]
pub async fn commit_replacement_url(
    m: Mgr<'_>,
    id: String,
    url: String,
    accept_confirm: bool,
) -> Res<()> {
    let manager = m.inner().clone();
    manager
        .commit_replacement_url(&id, &url, UrlSource::User, accept_confirm)
        .await
}

#[tauri::command]
pub async fn choose_folder(app: tauri::AppHandle) -> Option<String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog().file().pick_folder(move |path| {
        let _ = tx.send(path);
    });
    rx.await.ok().flatten().map(|p| p.to_string())
}

/// Show the finished file in the file manager.
#[tauri::command]
pub fn reveal_in_explorer(m: Mgr<'_>, app: tauri::AppHandle, id: String) -> Res<()> {
    let path = target_path(&m, &id)?;
    app.opener()
        .reveal_item_in_dir(&path)
        .map_err(|e| ManagerError::Io(e.to_string()))
}

/// Open a finished file.
///
/// Only ever reached by a deliberate click in the overflow menu — nothing is auto-opened on
/// completion, and that is not a setting we offer. Going through the OS opener means the
/// Mark-of-the-Web and SmartScreen engage exactly as they would for a browser download.
#[tauri::command]
pub fn open_file(m: Mgr<'_>, app: tauri::AppHandle, id: String) -> Res<()> {
    let path = target_path(&m, &id)?;
    if !path.exists() {
        return Err(ManagerError::Invalid(
            "that file is not there any more".into(),
        ));
    }
    app.opener()
        .open_path(path.to_string_lossy(), None::<&str>)
        .map_err(|e| ManagerError::Io(e.to_string()))
}

fn target_path(m: &Mgr<'_>, id: &str) -> Res<PathBuf> {
    let d = m.details(id)?;
    Ok(PathBuf::from(&d.summary.dest_dir).join(&d.summary.filename))
}
