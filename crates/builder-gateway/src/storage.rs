use super::security::is_hex_secret;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::OpenOptions,
    io::{Read, Write},
    path::{Path, PathBuf},
};
use uuid::Uuid;

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct DeviceRecord {
    pub(super) version: u16,
    pub(super) id: String,
    pub(super) name: String,
    pub(super) owner: String,
    pub(super) token_hash: String,
}

pub(super) fn create_private_directory(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)?;
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(
        metadata.file_type().is_dir(),
        "Gateway data path must be a directory"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub(super) fn load_device(path: &Path) -> Result<Option<DeviceRecord>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        metadata.file_type().is_file(),
        "device.json must be a regular file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            metadata.permissions().mode() & 0o077 == 0,
            "device.json must be private"
        );
    }
    let mut contents = String::new();
    std::fs::File::open(path)?
        .take(16 * 1024 + 1)
        .read_to_string(&mut contents)?;
    ensure!(contents.len() <= 16 * 1024, "device.json is too large");
    let device: DeviceRecord = serde_json::from_str(&contents).context("Invalid device.json")?;
    ensure!(device.version == 1, "Unsupported device.json version");
    validate_device(&device)?;
    Ok(Some(device))
}

fn validate_device(device: &DeviceRecord) -> Result<()> {
    ensure!(Uuid::parse_str(&device.id).is_ok(), "Invalid device ID");
    ensure!(
        !device.name.is_empty() && device.name.len() <= 128,
        "Invalid device name"
    );
    ensure!(
        !device.owner.is_empty() && device.owner.len() <= 256,
        "Invalid device owner"
    );
    ensure!(
        is_hex_secret(&device.token_hash),
        "Invalid device token hash"
    );
    Ok(())
}

struct TemporaryFile(Option<PathBuf>);
impl Drop for TemporaryFile {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

pub(super) fn save_device(path: &Path, device: &DeviceRecord) -> Result<()> {
    validate_device(device)?;
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
    serde_json::to_writer(&mut file, device)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    std::fs::rename(&temporary_path, path)?;
    cleanup.0 = None;
    if let Some(parent) = path.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}
