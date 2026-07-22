use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{self, Cursor},
    path::{Path, PathBuf},
};

const MAX_EXPANDED_SIZE: u64 = 1024 * 1024 * 1024;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LauncherConfig {
    registry_url: String,
    addons: Vec<LauncherAddon>,
}

#[derive(Debug, Deserialize)]
struct LauncherAddon {
    id: String,
    required: bool,
    #[serde(rename = "latestVersionUrl")]
    latest_version_url: String,
}

#[derive(Debug, Deserialize)]
struct VersionEnvelope {
    version: RegistryVersion,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RegistryVersion {
    version: String,
    file_size: u64,
    sha256: String,
    download_url: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateResult {
    addon_id: String,
    version: String,
    required: bool,
    updated: bool,
}

impl UpdateResult {
    pub fn addon_id(&self) -> &str {
        &self.addon_id
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct InstalledVersion {
    version: String,
    sha256: String,
}

pub async fn update_addons(
    config_url: &str,
    install_root: &Path,
) -> Result<Vec<UpdateResult>, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|error| error.to_string())?;
    let config = client
        .get(config_url)
        .send()
        .await
        .map_err(|error| format!("ランチャー構成を取得できませんでした: {error}"))?
        .error_for_status()
        .map_err(|error| format!("ランチャー構成APIがエラーを返しました: {error}"))?
        .json::<LauncherConfig>()
        .await
        .map_err(|error| format!("ランチャー構成を解析できませんでした: {error}"))?;

    let addons_root = install_root.join("addons");
    fs::create_dir_all(&addons_root).map_err(|error| error.to_string())?;
    let mut results = Vec::with_capacity(config.addons.len());
    for addon in config.addons {
        validate_addon_id(&addon.id)?;
        let latest_url = absolute_url(&config.registry_url, &addon.latest_version_url);
        let release = client
            .get(latest_url)
            .send()
            .await
            .map_err(|error| format!("{}の更新情報を取得できませんでした: {error}", addon.id))?
            .error_for_status()
            .map_err(|error| format!("{}の更新情報APIがエラーを返しました: {error}", addon.id))?
            .json::<VersionEnvelope>()
            .await
            .map_err(|error| format!("{}の更新情報を解析できませんでした: {error}", addon.id))?
            .version;
        let target = addons_root.join(&addon.id);
        if installed_version(&target)
            .as_ref()
            .is_some_and(|installed| {
                installed.version == release.version && installed.sha256 == release.sha256
            })
        {
            results.push(UpdateResult {
                addon_id: addon.id,
                version: release.version,
                required: addon.required,
                updated: false,
            });
            continue;
        }
        let download_url = absolute_url(&config.registry_url, &release.download_url);
        let bytes = client
            .get(download_url)
            .send()
            .await
            .map_err(|error| format!("{}をダウンロードできませんでした: {error}", addon.id))?
            .error_for_status()
            .map_err(|error| format!("{}のダウンロードが拒否されました: {error}", addon.id))?
            .bytes()
            .await
            .map_err(|error| format!("{}を読み込めませんでした: {error}", addon.id))?;
        verify_archive(&bytes, &release)?;
        install_archive(&bytes, &target, &release)
            .map_err(|error| format!("{}をインストールできませんでした: {error}", addon.id))?;
        results.push(UpdateResult {
            addon_id: addon.id,
            version: release.version,
            required: addon.required,
            updated: true,
        });
    }
    Ok(results)
}

fn absolute_url(registry_url: &str, value: &str) -> String {
    if value.starts_with("http://") || value.starts_with("https://") {
        value.to_owned()
    } else {
        format!(
            "{}{}",
            registry_url.trim_end_matches('/'),
            if value.starts_with('/') {
                value.to_owned()
            } else {
                format!("/{value}")
            }
        )
    }
}

fn validate_addon_id(id: &str) -> Result<(), String> {
    if id.is_empty()
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(format!("不正なアドオンIDです: {id}"));
    }
    Ok(())
}

fn installed_version(target: &Path) -> Option<InstalledVersion> {
    serde_json::from_slice(&fs::read(target.join(".kairo-version.json")).ok()?).ok()
}

fn verify_archive(bytes: &[u8], release: &RegistryVersion) -> Result<(), String> {
    if bytes.len() as u64 != release.file_size {
        return Err(format!(
            "ファイルサイズが一致しません (expected {}, got {})",
            release.file_size,
            bytes.len()
        ));
    }
    let actual = format!("{:x}", Sha256::digest(bytes));
    if !actual.eq_ignore_ascii_case(&release.sha256) {
        return Err("SHA-256が一致しません".to_owned());
    }
    Ok(())
}

fn install_archive(bytes: &[u8], target: &Path, release: &RegistryVersion) -> io::Result<()> {
    let parent = target.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "install target has no parent")
    })?;
    let staging = parent.join(format!(
        ".{}.staging",
        target.file_name().unwrap_or_default().to_string_lossy()
    ));
    let backup = parent.join(format!(
        ".{}.backup",
        target.file_name().unwrap_or_default().to_string_lossy()
    ));
    remove_if_exists(&staging)?;
    remove_if_exists(&backup)?;
    fs::create_dir_all(&staging)?;
    if let Err(error) = extract_zip(bytes, &staging) {
        let _ = remove_if_exists(&staging);
        return Err(error);
    }
    fs::write(
        staging.join(".kairo-version.json"),
        serde_json::to_vec_pretty(&InstalledVersion {
            version: release.version.clone(),
            sha256: release.sha256.clone(),
        })?,
    )?;
    if target.exists() {
        fs::rename(target, &backup)?;
    }
    if let Err(error) = fs::rename(&staging, target) {
        if backup.exists() {
            let _ = fs::rename(&backup, target);
        }
        return Err(error);
    }
    remove_if_exists(&backup)
}

fn extract_zip(bytes: &[u8], destination: &Path) -> io::Result<()> {
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
        if entry
            .unix_mode()
            .is_some_and(|mode| mode & 0o170000 == 0o120000)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "ZIP symlinks are not allowed",
            ));
        }
        expanded = expanded
            .checked_add(entry.size())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "ZIP size overflow"))?;
        if expanded > MAX_EXPANDED_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "ZIP expands beyond 1 GiB",
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

fn remove_if_exists(path: &PathBuf) -> io::Result<()> {
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
    fn resolves_relative_registry_urls() {
        assert_eq!(
            absolute_url("https://kairojs.com/", "/api/v1/addons/kairo"),
            "https://kairojs.com/api/v1/addons/kairo"
        );
    }

    #[test]
    fn rejects_unsafe_addon_ids() {
        assert!(validate_addon_id("game-manager").is_ok());
        assert!(validate_addon_id("../escape").is_err());
    }
}
