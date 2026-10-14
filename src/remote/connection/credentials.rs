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
    // Directory handles cannot be opened this way on Windows; the rename is
    // already atomic there.
    #[cfg(unix)]
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

pub(super) fn is_hex_secret(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credential(gateway: &str) -> SavedCredential {
        SavedCredential {
            version: 1,
            gateway: gateway.into(),
            device_id: Uuid::new_v4().to_string(),
            device_name: "Laptop".into(),
            token: "ab".repeat(32),
        }
    }

    fn write_private(path: &Path, text: &str) {
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options
            .open(path)
            .unwrap()
            .write_all(text.as_bytes())
            .unwrap();
    }

    fn json_with(mutate: impl FnOnce(&mut serde_json::Value)) -> String {
        let mut value = serde_json::to_value(credential("https://gateway.example.com")).unwrap();
        mutate(&mut value);
        value.to_string()
    }

    #[test]
    fn credential_path_is_a_private_digest_per_origin() {
        let home = Path::new("/home/user/.builder");
        let a = credential_path(home, "https://a.example.com");
        let b = credential_path(home, "https://b.example.com");
        assert_ne!(a, b);
        assert_eq!(a, credential_path(home, "https://a.example.com"));
        assert_eq!(a.parent().unwrap(), home.join("remote-gateways"));
        let name = a.file_name().unwrap().to_str().unwrap();
        assert_eq!(name.len(), 24 + ".json".len());
        assert!(name.ends_with(".json"));
        assert!(name[..24].bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(
            !name.contains("example"),
            "the origin is not in the file name"
        );
    }

    #[test]
    fn hex_secrets_are_exactly_sixty_four_hex_digits() {
        assert!(is_hex_secret(&"0f".repeat(32)));
        assert!(is_hex_secret(&"AB".repeat(32)));
        assert!(!is_hex_secret(&"0f".repeat(31)));
        assert!(!is_hex_secret(&"0f".repeat(33)));
        assert!(!is_hex_secret(&"zz".repeat(32)));
        assert!(!is_hex_secret(""));
    }

    #[test]
    fn save_then_load_round_trips_privately_and_leaves_no_temporary_file() {
        let home = tempfile::tempdir().unwrap();
        let path = credential_path(home.path(), "https://gateway.example.com");
        assert!(load_credential(&path).unwrap().is_none());

        let saved = credential("https://gateway.example.com");
        save_credential(&path, &saved).unwrap();
        let loaded = load_credential(&path).unwrap().unwrap();
        assert_eq!(loaded.version, 1);
        assert_eq!(loaded.gateway, saved.gateway);
        assert_eq!(loaded.device_id, saved.device_id);
        assert_eq!(loaded.device_name, saved.device_name);
        assert_eq!(loaded.token, saved.token);

        let parent = path.parent().unwrap();
        let entries = std::fs::read_dir(parent)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(entries.len(), 1, "no temporary file remains: {entries:?}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(parent).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }

        let replaced = credential("https://gateway.example.com");
        save_credential(&path, &replaced).unwrap();
        assert_eq!(
            load_credential(&path).unwrap().unwrap().device_id,
            replaced.device_id
        );
        assert_eq!(std::fs::read_dir(parent).unwrap().count(), 1);
    }

    #[test]
    fn saving_needs_a_parent_directory() {
        let error =
            save_credential(Path::new("/"), &credential("https://g.example.com")).unwrap_err();
        assert!(error.to_string().contains("no parent"), "{error:#}");
    }

    #[test]
    fn invalid_saved_credentials_are_rejected_without_being_used() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("credential.json");
        let cases = [
            ("not json".to_owned(), "Invalid gateway credential"),
            (
                json_with(|value| value["version"] = 2.into()),
                "Unsupported gateway credential version",
            ),
            (
                json_with(|value| value["device_id"] = "nope".into()),
                "Invalid saved gateway device ID",
            ),
            (
                json_with(|value| value["device_name"] = "".into()),
                "Invalid saved gateway device name",
            ),
            (
                json_with(|value| value["device_name"] = "x".repeat(129).into()),
                "Invalid saved gateway device name",
            ),
            (
                json_with(|value| value["gateway"] = "https://x.example.com/path".into()),
                "Invalid saved gateway origin",
            ),
            (
                json_with(|value| value["token"] = "short".into()),
                "Invalid saved gateway device token",
            ),
            (
                format!("{}{}", json_with(|_| {}), " ".repeat(16 * 1024)),
                "Gateway credential is too large",
            ),
        ];
        for (text, reason) in cases {
            write_private(&path, &text);
            let error = load_credential(&path).err().unwrap().to_string();
            assert!(error.contains(reason), "{reason}: {error}");
        }
        write_private(&path, &json_with(|_| {}));
        assert!(load_credential(&path).unwrap().is_some());
    }

    #[cfg(unix)]
    #[test]
    fn shared_or_symlinked_credentials_are_refused() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("credential.json");
        write_private(&path, &json_with(|_| {}));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let error = load_credential(&path).err().unwrap().to_string();
        assert!(error.contains("must be private"), "{error}");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(load_credential(&path).unwrap().is_some());

        let link = home.path().join("link.json");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        let error = load_credential(&link).err().unwrap().to_string();
        assert!(error.contains("regular file"), "{error}");
    }
}
