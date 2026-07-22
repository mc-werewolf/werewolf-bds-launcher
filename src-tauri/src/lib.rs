mod bds;
mod network;
mod updater;

use tauri::Manager;

const LAUNCHER_CONFIG_URL: &str = "https://mc-werewolf.com/api/launcher/v1/config";

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
async fn publish_server() -> Result<network::PublishResult, String> {
    network::publish().await
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(bds::ServerProcess::default())
        .invoke_handler(tauri::generate_handler![
            prepare_server,
            start_server,
            publish_server
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
