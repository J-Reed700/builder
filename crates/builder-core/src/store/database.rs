//! Private database creation, connection lifetimes, and per-checkout index locks.
use super::{CodeIndexGuard, Store, private_lock_options};
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use rusqlite::{Connection, TransactionBehavior};
use sha2::{Digest, Sha256};
use std::{
    cell::{RefCell, RefMut},
    fs::File,
    path::{Path, PathBuf},
    time::Duration,
};

const JOURNAL_BUSY_TIMEOUT: Duration = Duration::from_secs(5);
// Each checkout index has exactly one writer (its CodeIndexGuard holder), so
// a busy wait only covers first-open initialization races between processes.
const INDEX_BUSY_TIMEOUT: Duration = Duration::from_secs(2);
// A full generation publish is one transaction, so the WAL briefly reaches the
// size of that generation. Truncate it back after each checkpoint reset rather
// than retaining the high-water mark indefinitely.
const INDEX_WAL_LIMIT_BYTES: i64 = 64 * 1024 * 1024;
const INDEX_DIR: &str = "code-index";
const LEGACY_SHARED_INDEX: &str = "builder-index.sqlite3";

fn private_database_options() -> std::fs::OpenOptions {
    let mut options = File::options();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

fn index_identity(scope: &str) -> Result<String> {
    ensure!(
        !scope.is_empty() && scope.len() <= 16 * 1024,
        "Invalid code index scope"
    );
    Ok(format!("{:x}", Sha256::digest(scope.as_bytes())))
}

fn open_index(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        crate::config::ensure_home(parent)?;
    }
    let options = private_database_options();
    match options.open(path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let mut conn = Connection::open(path)?;
    conn.busy_timeout(INDEX_BUSY_TIMEOUT)?;
    conn.execute_batch(&format!(
        "PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON; PRAGMA journal_size_limit={INDEX_WAL_LIMIT_BYTES};"
    ))?;
    let mut version: u32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    ensure!(
        version <= 1,
        "Code index database was created by a newer Builder version; upgrade Builder"
    );
    if version == 0 {
        // journal_mode cannot change inside a transaction. It is established
        // once before any index rows exist; later opens only read the version
        // and therefore do not contend with an active background writer.
        conn.execute_batch("PRAGMA journal_mode=WAL;")?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        version = tx.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        ensure!(
            version <= 1,
            "Code index database was created by a newer Builder version; upgrade Builder"
        );
        if version == 0 {
            tx.execute_batch(super::code_index::SCHEMA)?;
            tx.execute_batch(super::code_index::TELEMETRY_SCHEMA)?;
            tx.execute_batch(super::code_index::HISTORY_SCHEMA)?;
            tx.execute_batch(super::code_index::GRAPH_SCHEMA)?;
            tx.execute_batch("PRAGMA user_version=1;")?;
        }
        tx.commit()?;
    }
    let journal_mode: String =
        conn.query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))?;
    ensure!(
        journal_mode.eq_ignore_ascii_case("wal"),
        "Code index database is not in WAL mode"
    );
    Ok(conn)
}

impl Store {
    pub fn open(home: &Path) -> Result<Self> {
        crate::config::ensure_home(home)?;
        let path = home.join("builder.sqlite3");
        let options = private_database_options();
        // Seed a new database with private permissions; SQLite copies them to
        // its WAL and shm files. Never open an existing database here: POSIX
        // drops every advisory lock this process holds on a file whenever any
        // descriptor for it is closed, so opening and closing the file beside
        // a live SQLite connection released that connection's WAL-mode shared
        // lock. A second Builder process could then checkpoint and truncate the
        // WAL underneath it, and the next read failed with SQLITE_IOERR_SHORT_READ
        // ("disk I/O error: Error code 522 ... file truncated?").
        match options.open(&path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        let mut conn = Connection::open(&path)?;
        conn.busy_timeout(JOURNAL_BUSY_TIMEOUT)?;
        conn.execute_batch("PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;")?;
        super::migrations::migrate(&mut conn)?;
        let journal_mode: String =
            conn.query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))?;
        ensure!(
            journal_mode.eq_ignore_ascii_case("wal"),
            "Authoritative journal is not in WAL mode"
        );
        // Releases before per-checkout indexes shared one database across
        // every repository and process, so one checkout's bulk publish locked
        // out another's foreground reads and writes. The data is derived and
        // rebuilt per checkout on first use. Removal is best-effort: an older
        // process keeps its open descriptors until it exits.
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(home.join(format!("{LEGACY_SHARED_INDEX}{suffix}")));
        }
        Ok(Self {
            conn,
            indexes: RefCell::default(),
            home: home.into(),
        })
    }

    fn index_path(&self, scope: &str, extension: &str) -> Result<PathBuf> {
        Ok(self
            .home
            .join(INDEX_DIR)
            .join(format!("{}.{extension}", index_identity(scope)?)))
    }

    /// The checkout's derived index connection. A rebuildable sidecar must
    /// never make the authoritative journal unavailable: failure to open is
    /// reported to the caller, which degrades, and the next use retries.
    pub(super) fn index(&self, scope: &str) -> Result<RefMut<'_, Connection>> {
        let mut indexes = self
            .indexes
            .try_borrow_mut()
            .context("Code index connection is already in use")?;
        if !indexes.contains_key(scope) {
            let path = self.index_path(scope, "sqlite3")?;
            let connection = open_index(&path).context("Derived code index unavailable")?;
            indexes.insert(scope.to_owned(), connection);
        }
        Ok(RefMut::map(indexes, |indexes| {
            indexes
                .get_mut(scope)
                .expect("index connection opened above")
        }))
    }

    /// Nonblocking: `None` means another holder is writing this checkout's
    /// index, and callers continue on the last published generation.
    pub fn try_code_index_lock(&self, scope: &str) -> Result<Option<CodeIndexGuard>> {
        let path = self.index_path(scope, "lock")?;
        if let Some(parent) = path.parent() {
            crate::config::ensure_home(parent)?;
        }
        let file = private_lock_options().open(path)?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(CodeIndexGuard {
                file,
                scope: scope.to_owned(),
            })),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(error).context("Could not acquire code index maintenance lock"),
        }
    }
}
