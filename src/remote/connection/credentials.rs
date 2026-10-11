//! Private, bounded, atomically replaced gateway credentials.
use super::gateway_urls;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::OpenOptions,
    io::{Read, Write},
    path::{Path, PathBuf},
};
use uuid::Uuid;

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct SavedCredential {
    pub(super) version: u16,
    pub(super) gateway: String,
    pub(super) device_id: String,
    pub(super) device_name: String,
    pub(super) token: String,
}

pub(super) fn credential_path(home: &Path, gateway: &str) -> PathBuf {
    let digest = format!("{:x}", Sha256::digest(gateway.as_bytes()));
    home.join("remote-gateways")
        .join(format!("{}.json", &digest[..24]))
}

pub(super) fn load_credential(path: &Path) -> Result<Option<SavedCredential>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        metadata.file_type().is_file(),
        "Gateway credential must be a regular file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            metadata.permissions().mode() & 0o077 == 0,
            "Gateway credential must be private"
        );
    }
    let mut contents = String::new();
    std::fs::File::open(path)?
        .take(16 * 1024 + 1)
        .read_to_string(&mut contents)?;
    ensure!(
        contents.len() <= 16 * 1024,
        "Gateway credential is too large"
    );
    let credential: SavedCredential =
        serde_json::from_str(&contents).context("Invalid gateway credential")?;
    ensure!(
        credential.version == 1,
        "Unsupported gateway credential version"
    );
    ensure!(
        Uuid::parse_str(&credential.device_id).is_ok(),
        "Invalid saved gateway device ID"
    );
    ensure!(
        !credential.device_name.is_empty() && credential.device_name.len() <= 128,
        "Invalid saved gateway device name"
    );
    gateway_urls(&credential.gateway).context("Invalid saved gateway origin")?;
    ensure!(
        is_hex_secret(&credential.token),
        "Invalid saved gateway device token"
    );
    Ok(Some(credential))
}

struct TemporaryFile(Option<PathBuf>);
impl Drop for TemporaryFile {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

pub(super) fn save_credential(path: &Path, credential: &SavedCredential) -> Result<()> {
    let parent = path
        .parent()
        .context("Gateway credential path has no parent")?;
    std::fs::create_dir_all(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    let temporary_path = path.with_extension(format!("{}.tmp", Uuid::new_v4().simple()));
    let mut cleanup = TemporaryFile(Some(temporary_path.clone()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary_path)?;
    serde_json::to_writer(&mut file, credential)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    std::fs::rename(&temporary_path, path)?;
    cleanup.0 = None;
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

pub(super) fn is_hex_secret(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}
