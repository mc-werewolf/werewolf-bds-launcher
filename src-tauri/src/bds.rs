use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Cursor, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Mutex,
};

const DOWNLOAD_LINKS_URL: &str =
    "https://net-secondary.web.minecraft-services.net/api/v1.0/download/links";
const WORLD_NAME: &str = "Werewolf";
const MAX_BDS_EXPANDED_SIZE: u64 = 2 * 1024 * 1024 * 1024;

pub struct ServerProcess(pub Mutex<Option<Child>>);

impl Default for ServerProcess {
    fn default() -> Self {
        Self(Mutex::new(None))
    }
}

#[derive(Debug, Deserialize)]
struct DownloadLinksEnvelope {
    result: DownloadLinks,
}
#[derive(Debug, Deserialize)]
struct DownloadLinks {
    links: Vec<DownloadLink>,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DownloadLink {
    download_type: String,
    download_url: String,
}
#[derive(Debug, Serialize, Deserialize)]
struct InstalledBds {
    download_url: String,
}
#[derive(Debug, Deserialize)]
struct PackManifest {
    header: PackHeader,
}
#[derive(Debug, Deserialize)]
struct PackHeader {
    uuid: String,
    version: Vec<u32>,
}
#[derive(Debug, Clone, Serialize)]
struct WorldPack {
    pack_id: String,
    version: Vec<u32>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BdsStatus {
    pub version: String,
    pub updated: bool,
    pub world_name: String,
    pub behavior_packs: usize,
    pub resource_packs: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchResult {
    pub pid: u32,
    pub address: String,
    pub port: u16,
    pub world_name: String,
}

pub async fn prepare_bds(install_root: &Path, addon_ids: &[String]) -> Result<BdsStatus, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .map_err(|error| error.to_string())?;
    let links = client
        .get(DOWNLOAD_LINKS_URL)
        .send()
        .await
        .map_err(|error| format!("BDSダウンロード情報を取得できませんでした: {error}"))?
        .error_for_status()
        .map_err(|error| format!("BDSダウンロードAPIがエラーを返しました: {error}"))?
        .json::<DownloadLinksEnvelope>()
        .await
        .map_err(|error| format!("BDSダウンロード情報を解析できませんでした: {error}"))?;
    let download_url = links
        .result
        .links
        .into_iter()
        .find(|link| link.download_type == "serverBedrockWindows")
        .map(|link| link.download_url)
        .ok_or_else(|| "Windows用BDSダウンロードが見つかりませんでした".to_owned())?;
    let bds_root = install_root.join("bds");
    fs::create_dir_all(&bds_root).map_err(|error| error.to_string())?;
    let current = bds_root.join("current");
    let installed = fs::read(current.join(".werewolf-bds.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<InstalledBds>(&bytes).ok());
    let updated = !current.join("bedrock_server.exe").is_file()
        || installed
            .as_ref()
            .is_none_or(|value| value.download_url != download_url);
    if updated {
        let bytes = client
            .get(&download_url)
            .send()
            .await
            .map_err(|error| format!("BDSをダウンロードできませんでした: {error}"))?
            .error_for_status()
            .map_err(|error| format!("BDSダウンロードが拒否されました: {error}"))?
            .bytes()
            .await
            .map_err(|error| format!("BDS ZIPを読み込めませんでした: {error}"))?;
        install_bds(&bytes, &current, &download_url)
            .map_err(|error| format!("BDSをインストールできませんでした: {error}"))?;
    }
    let (behavior_packs, resource_packs) = apply_addons(install_root, &current, addon_ids)
        .map_err(|error| format!("アドオンをBDSへ適用できませんでした: {error}"))?;
    ensure_server_properties(&current).map_err(|error| error.to_string())?;
    Ok(BdsStatus {
        version: version_from_url(&download_url),
        updated,
        world_name: WORLD_NAME.to_owned(),
        behavior_packs,
        resource_packs,
    })
}

fn install_bds(bytes: &[u8], current: &Path, download_url: &str) -> io::Result<()> {
    let parent = current
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "BDS target has no parent"))?;
    let staging = parent.join(".current.staging");
    let backup = parent.join(".current.backup");
    remove_dir(&staging)?;
    remove_dir(&backup)?;
    fs::create_dir_all(&staging)?;
    extract_zip(bytes, &staging, MAX_BDS_EXPANDED_SIZE)?;
    if !staging.join("bedrock_server.exe").is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "BDS ZIPにbedrock_server.exeがありません",
        ));
    }
    fs::write(
        staging.join(".werewolf-bds.json"),
        serde_json::to_vec_pretty(&InstalledBds {
            download_url: download_url.to_owned(),
        })?,
    )?;
    if current.exists() {
        preserve_path(current, &staging, "worlds")?;
        for file in ["server.properties", "allowlist.json", "permissions.json"] {
            preserve_path(current, &staging, file)?;
        }
        fs::rename(current, &backup)?;
    }
    if let Err(error) = fs::rename(&staging, current) {
        if backup.exists() {
            let _ = fs::rename(&backup, current);
        }
        return Err(error);
    }
    remove_dir(&backup)
}

fn preserve_path(current: &Path, staging: &Path, name: &str) -> io::Result<()> {
    let source = current.join(name);
    if !source.exists() {
        return Ok(());
    }
    let destination = staging.join(name);
    if destination.exists() {
        if destination.is_dir() {
            fs::remove_dir_all(&destination)?;
        } else {
            fs::remove_file(&destination)?;
        }
    }
    copy_recursively(&source, &destination)
}

fn apply_addons(
    install_root: &Path,
    bds_root: &Path,
    addon_ids: &[String],
) -> io::Result<(usize, usize)> {
    let mut behavior = Vec::new();
    let mut resources = Vec::new();
    for addon_id in addon_ids {
        let addon = install_root.join("addons").join(addon_id);
        install_pack(
            &addon.join("BP"),
            &bds_root.join("behavior_packs").join(addon_id),
            &mut behavior,
        )?;
        install_pack(
            &addon.join("RP"),
            &bds_root.join("resource_packs").join(addon_id),
            &mut resources,
        )?;
    }
    let world = bds_root.join("worlds").join(WORLD_NAME);
    fs::create_dir_all(&world)?;
    write_json_atomic(&world.join("world_behavior_packs.json"), &behavior)?;
    write_json_atomic(&world.join("world_resource_packs.json"), &resources)?;
    Ok((behavior.len(), resources.len()))
}

fn install_pack(source: &Path, target: &Path, packs: &mut Vec<WorldPack>) -> io::Result<()> {
    if !source.is_dir() {
        return Ok(());
    }
    let manifest: PackManifest = serde_json::from_slice(&fs::read(source.join("manifest.json"))?)?;
    if manifest.header.version.len() != 3 || manifest.header.uuid.trim().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "pack manifest header is invalid",
        ));
    }
    if target.exists() {
        fs::remove_dir_all(target)?;
    }
    copy_recursively(source, target)?;
    packs.push(WorldPack {
        pack_id: manifest.header.uuid,
        version: manifest.header.version,
    });
    Ok(())
}

fn ensure_server_properties(bds_root: &Path) -> io::Result<()> {
    let path = bds_root.join("server.properties");
    let content = fs::read_to_string(&path).unwrap_or_default();
    let mut found = false;
    let mut output = String::new();
    for line in content.lines() {
        if line.starts_with("level-name=") {
            output.push_str(&format!("level-name={WORLD_NAME}\n"));
            found = true;
        } else {
            output.push_str(line);
            output.push('\n');
        }
    }
    if !found {
        output.push_str(&format!("level-name={WORLD_NAME}\n"));
    }
    fs::write(path, output)
}

pub fn start_bds(install_root: &Path, process: &ServerProcess) -> Result<LaunchResult, String> {
    let mut guard = process
        .0
        .lock()
        .map_err(|_| "BDSプロセス状態を取得できませんでした")?;
    if let Some(child) = guard.as_mut() {
        if child
            .try_wait()
            .map_err(|error| error.to_string())?
            .is_none()
        {
            return Err("BDSは既に起動しています".to_owned());
        }
    }
    let bds_root = install_root.join("bds").join("current");
    let executable = bds_root.join("bedrock_server.exe");
    if !executable.is_file() {
        return Err("BDSが準備されていません".to_owned());
    }
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(bds_root.join("bedrock_server.log"))
        .map_err(|error| error.to_string())?;
    let error_log = log.try_clone().map_err(|error| error.to_string())?;
    let child = Command::new(executable)
        .current_dir(&bds_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(error_log))
        .spawn()
        .map_err(|error| format!("BDSを起動できませんでした: {error}"))?;
    let pid = child.id();
    *guard = Some(child);
    Ok(LaunchResult {
        pid,
        address: "127.0.0.1".to_owned(),
        port: server_port(&bds_root.join("server.properties")),
        world_name: WORLD_NAME.to_owned(),
    })
}

fn server_port(path: &Path) -> u16 {
    fs::read_to_string(path)
        .ok()
        .and_then(|content| {
            content.lines().find_map(|line| {
                line.strip_prefix("server-port=")?
                    .trim()
                    .parse::<u16>()
                    .ok()
            })
        })
        .unwrap_or(19132)
}
fn version_from_url(url: &str) -> String {
    url.rsplit('/')
        .next()
        .and_then(|name| name.strip_prefix("bedrock-server-"))
        .and_then(|name| name.strip_suffix(".zip"))
        .unwrap_or("unknown")
        .to_owned()
}
fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let temporary = path.with_extension("json.tmp");
    let mut file = File::create(&temporary)?;
    file.write_all(&serde_json::to_vec_pretty(value)?)?;
    file.sync_all()?;
    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(temporary, path)
}
fn extract_zip(bytes: &[u8], destination: &Path, limit: u64) -> io::Result<()> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))?;
    let mut expanded = 0_u64;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        let relative = entry
            .enclosed_name()
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "ZIP contains an unsafe path")
            })?
            .to_owned();
        expanded = expanded
            .checked_add(entry.size())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "ZIP size overflow"))?;
        if expanded > limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "ZIP is too large",
            ));
        }
        let output = destination.join(relative);
        if entry.is_dir() {
            fs::create_dir_all(output)?;
        } else {
            if let Some(parent) = output.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut file = File::create(output)?;
            io::copy(&mut entry, &mut file)?;
        }
    }
    Ok(())
}
fn copy_recursively(source: &Path, destination: &Path) -> io::Result<()> {
    if source.is_file() {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(source, destination)?;
        return Ok(());
    }
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        copy_recursively(&entry.path(), &destination.join(entry.file_name()))?;
    }
    Ok(())
}
fn remove_dir(path: &PathBuf) -> io::Result<()> {
    if path.exists() {
        fs::remove_dir_all(path)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reads_bds_version_from_download_url() {
        assert_eq!(
            version_from_url("https://example.test/bin-win/bedrock-server-1.26.33.2.zip"),
            "1.26.33.2"
        );
    }
}
