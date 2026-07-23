mod bds;
mod network;
mod updater;

use tauri::Manager;
use tauri_plugin_updater::UpdaterExt;

const LAUNCHER_CONFIG_URL: &str = "https://mc-werewolf.com/api/launcher/v1/config";

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct AppUpdate {
    current_version: String,
    version: String,
    notes: Option<String>,
}

#[tauri::command]
async fn check_app_update(app: tauri::AppHandle) -> Result<Option<AppUpdate>, String> {
    let current_version = app.package_info().version.to_string();
    let update = app
        .updater()
        .map_err(|error| format!("更新機能を初期化できませんでした: {error}"))?
        .check()
        .await
        .map_err(|error| format!("更新情報を確認できませんでした: {error}"))?;

    Ok(update.map(|update| AppUpdate {
        current_version,
        version: update.version.to_string(),
        notes: update.body,
    }))
}

#[tauri::command]
async fn install_app_update(app: tauri::AppHandle) -> Result<(), String> {
    let update = app
        .updater()
        .map_err(|error| format!("更新機能を初期化できませんでした: {error}"))?
        .check()
        .await
        .map_err(|error| format!("更新情報を確認できませんでした: {error}"))?
        .ok_or_else(|| "利用可能な更新はありません。".to_owned())?;

    update
        .download_and_install(|_, _| {}, || {})
        .await
        .map_err(|error| format!("更新をインストールできませんでした: {error}"))?;

    app.restart();
}

#[tauri::command]
async fn prepare_server(app: tauri::AppHandle) -> Result<PrepareResult, String> {
    let install_root = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("アプリデータディレクトリを取得できませんでした: {error}"))?;
    let addons = updater::update_addons(LAUNCHER_CONFIG_URL, &install_root).await?;
    let addon_ids = addons
        .iter()
        .map(|result| result.addon_id().to_owned())
        .collect::<Vec<_>>();
    let bds = bds::prepare_bds(&install_root, &addon_ids).await?;
    Ok(PrepareResult { addons, bds })
}

#[derive(serde::Serialize)]
struct PrepareResult {
    addons: Vec<updater::UpdateResult>,
    bds: bds::BdsStatus,
}

#[tauri::command]
fn start_server(
    app: tauri::AppHandle,
    process: tauri::State<'_, bds::ServerProcess>,
) -> Result<bds::LaunchResult, String> {
    let install_root = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("アプリデータディレクトリを取得できませんでした: {error}"))?;
    bds::start_bds(&install_root, &process)
}

#[tauri::command]
async fn publish_server(
    state: tauri::State<'_, network::NetworkState>,
) -> Result<network::PublishResult, String> {
    network::publish(state.inner().clone()).await
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_opener::init())
        .manage(bds::ServerProcess::default())
        .manage(network::NetworkState::default())
        .invoke_handler(tauri::generate_handler![
            check_app_update,
            install_app_update,
            prepare_server,
            start_server,
            publish_server
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
