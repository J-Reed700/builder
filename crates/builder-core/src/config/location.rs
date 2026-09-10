//! Human-edited configuration is separate from durable application data.
use super::{Config, ensure_home};
use anyhow::{Context, Result, ensure};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
};

pub fn directory(override_home: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = override_home {
        return Ok(path.to_owned());
    }
    let base =
        directories::BaseDirs::new().context("Cannot find user directory; set BUILDER_HOME")?;
    directory_from(
        base.home_dir(),
        std::env::var_os("XDG_CONFIG_HOME")
            .as_deref()
            .map(Path::new),
    )
}
fn directory_from(user: &Path, xdg: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = xdg.filter(|p| !p.as_os_str().is_empty()) {
        ensure!(
            path.is_absolute(),
            "XDG_CONFIG_HOME must be an absolute directory"
        );
        Ok(path.join("builder"))
    } else {
        Ok(user.join(".config").join("builder"))
    }
}

pub(super) fn read_source(path: &Path) -> Result<String> {
    let mut source = String::new();
    std::fs::File::open(path)?
        .take(1024 * 1024 + 1)
        .read_to_string(&mut source)?;
    ensure!(
        source.len() <= 1024 * 1024,
        "Config exceeds the 1 MiB limit: {}",
        path.display()
    );
    Ok(source)
}

/// Copy only the legacy config, preserving comments, credentials, and all data paths.
/// Atomic publication never replaces a config another process/user already created.
pub fn migrate_legacy(data: &Path, config: &Path) -> Result<bool> {
    let old = data.join("config.toml");
    let destination = config.join("config.toml");
    if old == destination || destination.try_exists()? || !old.try_exists()? {
        return Ok(false);
    }
    let source = read_source(&old)?;
    let _: Config = toml::from_str(&source).map_err(|_| {
        anyhow::anyhow!(
            "Legacy config is invalid; source hidden to protect credentials: {}",
            old.display()
        )
    })?;
    ensure_home(config)?;
    let tmp = config.join(format!("config-migration-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<bool> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(source.as_bytes())?;
        file.sync_all()?;
        drop(file);
        match std::fs::hard_link(&tmp, &destination) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
            Err(e) => return Err(e.into()),
        }
        #[cfg(unix)]
        std::fs::File::open(config)?.sync_all()?;
        Ok(true)
    })();
    let _ = std::fs::remove_file(tmp);
    result.context("Could not migrate config; the original and application data remain in place")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn normal_directory_and_absolute_xdg_override() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            directory_from(root.path(), None).unwrap(),
            root.path().join(".config/builder")
        );
        assert_eq!(
            directory_from(root.path(), Some(root.path())).unwrap(),
            root.path().join("builder")
        );
        assert!(directory_from(root.path(), Some(Path::new("relative"))).is_err());
        assert_eq!(directory(Some(root.path())).unwrap(), root.path());
    }
    #[test]
    fn migration_preserves_raw_config_and_data_and_never_overwrites() {
        let old = tempfile::tempdir().unwrap();
        let new = tempfile::tempdir().unwrap();
        let source = "# Keep my comments\ndefault_profile='private'\n[profiles.private]\nbase_url='https://example.com/v1'\nmodel='model'\n[profiles.private.headers]\nAuthorization='Basic fixture'\n";
        std::fs::write(old.path().join("config.toml"), source).unwrap();
        std::fs::write(old.path().join("builder.sqlite3"), b"untouched fixture").unwrap();
        assert!(migrate_legacy(old.path(), new.path()).unwrap());
        assert_eq!(
            std::fs::read_to_string(new.path().join("config.toml")).unwrap(),
            source
        );
        assert_eq!(
            std::fs::read_to_string(old.path().join("config.toml")).unwrap(),
            source
        );
        assert_eq!(
            std::fs::read(old.path().join("builder.sqlite3")).unwrap(),
            b"untouched fixture"
        );
        std::fs::write(new.path().join("config.toml"), "new user config").unwrap();
        assert!(!migrate_legacy(old.path(), new.path()).unwrap());
        assert_eq!(
            std::fs::read_to_string(new.path().join("config.toml")).unwrap(),
            "new user config"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(new.path().join("config.toml"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
    }
    #[test]
    fn invalid_legacy_config_does_not_publish_partial_file_or_expose_secret() {
        let old = tempfile::tempdir().unwrap();
        let new = tempfile::tempdir().unwrap();
        std::fs::write(
            old.path().join("config.toml"),
            "password='never-print-this'\ninvalid{",
        )
        .unwrap();
        let error = migrate_legacy(old.path(), new.path()).unwrap_err();
        assert!(!format!("{error:#}").contains("never-print-this"));
        assert!(!new.path().join("config.toml").exists());
    }
}

#[cfg(test)]
mod limits {
    use super::*;
    #[test]
    fn oversized_config_fails_without_publishing_or_loading() {
        let old = tempfile::tempdir().unwrap();
        let new = tempfile::tempdir().unwrap();
        std::fs::write(old.path().join("config.toml"), "#".repeat(1024 * 1024 + 1)).unwrap();
        assert!(Config::load(old.path()).is_err());
        assert!(migrate_legacy(old.path(), new.path()).is_err());
        assert!(!new.path().join("config.toml").exists());
    }
}
