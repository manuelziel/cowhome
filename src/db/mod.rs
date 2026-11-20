//! Modular SQLite `WAL` persistence and time-series analytics engine (`Layer 3`).
//!
//! Replaces Python's 9-class mixin hierarchy with cohesive, cycle-free sub-modules
//! sharing a pooled `WAL` connection handle (`Database`), Wear-Leveling PRAGMAs,
//! in-memory zero-write entity/LWW/deadband caches, and `tokio::task::spawn_blocking`.

pub mod automation;
mod automation_sql;
pub mod efficiency;
mod efficiency_math;
pub mod energy;
pub mod entities_sync;
pub mod groups;
pub mod history;
pub mod ramp_analytics;
mod ramp_math;
pub mod recorder;
mod schema;
pub mod settings;
pub mod storage;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::Duration;

use rusqlite::{Connection, Transaction, TransactionBehavior};

use crate::core::constants::database_defaults::{
    BUSY_TIMEOUT_MS, CACHE_SIZE_KIB, DEFAULT_POOL_SIZE, FOREIGN_KEYS, JOURNAL_MODE,
    JOURNAL_SIZE_LIMIT_BYTES, MMAP_SIZE_BYTES, POOL_CHECKOUT_TIMEOUT_SECS, SYNCHRONOUS, TEMP_STORE,
};
use crate::core::domain_constants::automation::weather_defaults::TIMEZONE;
use crate::core::error::{AppError, AppResult};
use crate::core::logger::Logger;
use crate::core::security::ensure_not_symlink;
use recorder::RecorderCaches;
use schema::{ensure_database_ownership, initialize_and_verify_schema, seed_lww_timestamps};

// =============================================================================
/// Shared internal state of [`Database`] across threads and async tasks.
#[derive(Debug)]
struct DatabaseInner {
    /// Filesystem path to `cowhome.db` (or `":memory:"` in isolated tests).
    db_path: PathBuf,
    /// Idle `WAL` SQLite connections guarded by a mutex and condition variable.
    pool: Mutex<PoolState>,
    /// Condition variable notified whenever a connection returns to the pool.
    pool_cv: Condvar,
    /// Zero-write entity cache (`ain -> (entity_id, name, device_type)`).
    pub(super) entity_cache: RwLock<HashMap<String, (i64, String, String)>>,
    /// In-memory Last-Write-Wins timestamps (`lww_key -> effective_epoch_secs`).
    pub(super) lww_timestamps: RwLock<HashMap<String, f64>>,
    /// In-memory deadband, heartbeat, and pre-edge anchor state for history & presence.
    pub(super) recorder_caches: Mutex<RecorderCaches>,
    /// Configured home IANA timezone (`"Europe/Berlin"`).
    default_timezone: RwLock<String>,
    /// Application logger.
    pub(super) logger: Logger,
}

// =============================================================================
/// Bounded connection pool state tracking open vs idle `rusqlite` connections.
#[derive(Debug)]
struct PoolState {
    /// Idle ready-to-use connections.
    idle: Vec<Connection>,
    /// Whether `close()` has been invoked for graceful shutdown.
    closed: bool,
}

// =============================================================================
/// Thread-safe, `Clone`-cheap SQLite manager with `WAL` wear-leveling and async bridge.
#[derive(Debug, Clone)]
pub struct Database {
    /// Reference-counted shared database state.
    inner: Arc<DatabaseInner>,
}

impl Database {
    /// Opens (or creates) the SQLite database at `db_path`, applying `WAL` PRAGMAs and schema `v1`.
    ///
    /// # Errors
    /// Returns [`AppError`] on symlink detection, directory/connection failure, or schema mismatch.
    pub fn open(db_path: &Path, default_tz: &str, logger: Logger) -> AppResult<Self> {
        let is_mem = db_path.as_os_str() == ":memory:";
        if !is_mem {
            ensure_not_symlink(db_path)?;
            if let Some(parent) = db_path.parent().filter(|p| !p.as_os_str().is_empty()) {
                ensure_not_symlink(parent)?;
                fs::create_dir_all(parent).map_err(|err| {
                    AppError::io(
                        format!("Failed creating DB dir '{}'", parent.display()),
                        err,
                    )
                })?;
            }
        }
        let pool_size = if is_mem { 1 } else { DEFAULT_POOL_SIZE };
        let mut connections = Vec::with_capacity(pool_size);
        for _ in 0..pool_size {
            connections.push(open_configured_connection(db_path)?);
        }
        let first_conn = connections
            .first()
            .ok_or_else(|| AppError::Database("Empty SQLite pool".into()))?;
        initialize_and_verify_schema(first_conn, &logger)?;
        let seeded_lww = seed_lww_timestamps(first_conn);
        ensure_database_ownership(db_path, &logger);

        Ok(Self {
            inner: Arc::new(DatabaseInner {
                db_path: db_path.to_path_buf(),
                pool: Mutex::new(PoolState {
                    idle: connections,
                    closed: false,
                }),
                pool_cv: Condvar::new(),
                entity_cache: RwLock::new(HashMap::new()),
                lww_timestamps: RwLock::new(seeded_lww),
                recorder_caches: Mutex::new(RecorderCaches::default()),
                default_timezone: RwLock::new(default_tz.to_owned()),
                logger,
            }),
        })
    }

    /// Returns the configured filesystem path of the SQLite database.
    #[must_use]
    pub fn db_path(&self) -> &Path {
        &self.inner.db_path
    }

    /// Updates the fallback home IANA timezone on live `SIGHUP` reload.
    pub fn set_default_timezone(&self, tz: &str) {
        if let Ok(mut guard) = self.inner.default_timezone.write() {
            *guard = tz.to_owned();
        }
    }

    /// Reads the configured fallback home IANA timezone.
    #[must_use]
    pub fn default_timezone(&self) -> String {
        self.inner
            .default_timezone
            .read()
            .map(|g| g.clone())
            .unwrap_or_else(|_| String::from(TIMEZONE))
    }

    /// Checks out a pooled `WAL` connection for synchronous read or single-statement write work.
    ///
    /// # Errors
    /// Returns [`AppError::Database`] if the pool is closed, exhausted, or `work` fails.
    pub fn with_conn<T, F>(&self, work: F) -> AppResult<T>
    where
        F: FnOnce(&Connection) -> AppResult<T>,
    {
        let conn = self.checkout_connection()?;
        let result = work(&conn);
        self.return_connection(conn);
        result
    }

    /// Executes `work` inside an atomic `BEGIN IMMEDIATE` transaction, rolling back on error.
    ///
    /// # Errors
    /// Returns [`AppError`] if starting, executing, or committing the transaction fails.
    pub fn with_tx<T, F>(&self, work: F) -> AppResult<T>
    where
        F: FnOnce(&Transaction<'_>) -> AppResult<T>,
    {
        let mut conn = self.checkout_connection()?;
        let result = (|| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let out = work(&tx)?;
            tx.commit()?;
            Ok(out)
        })();
        if result.is_err() {
            if let Ok(mut cache) = self.inner.entity_cache.write() {
                cache.clear();
            }
        }
        self.return_connection(conn);
        result
    }

    /// Offloads synchronous SQLite work onto `tokio`'s blocking thread pool for `axum` handlers.
    ///
    /// # Errors
    /// Returns [`AppError`] if the blocking task panics or the database operation fails.
    pub async fn run_blocking<T, F>(&self, work: F) -> AppResult<T>
    where
        T: Send + 'static,
        F: FnOnce(Self) -> AppResult<T> + Send + 'static,
    {
        let db = self.clone();
        tokio::task::spawn_blocking(move || work(db))
            .await
            .map_err(|err| AppError::Database(format!("Blocking database task failed: {err}")))?
    }

    /// Executes a passive `WAL` checkpoint (`PRAGMA wal_checkpoint(PASSIVE)`).
    pub fn wal_checkpoint(&self) {
        if let Err(err) = self.with_conn(|conn| {
            conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE)")?;
            Ok(())
        }) {
            self.inner
                .logger
                .warning(&format!("Passive WAL checkpoint failed: {err}"));
        }
    }

    /// Executes `VACUUM` to defragment and compact the SQLite database file.
    ///
    /// # Errors
    /// Returns [`AppError::Database`] if `VACUUM` fails.
    pub fn vacuum(&self) -> AppResult<()> {
        self.with_conn(|conn| {
            conn.execute_batch("VACUUM")?;
            Ok(())
        })?;
        self.inner.logger.info("Database VACUUM completed.");
        Ok(())
    }

    /// Hands off ownership of `.db`, `-wal`, and `-shm` to `cowhome:cowhome` when `root`.
    pub fn sync_ownership(&self) {
        ensure_database_ownership(&self.inner.db_path, &self.inner.logger);
    }

    /// Closes all idle connections in the pool during graceful daemon shutdown.
    pub fn close(&self) {
        if let Ok(mut guard) = self.inner.pool.lock() {
            guard.closed = true;
            guard.idle.clear();
            self.inner.pool_cv.notify_all();
        }
        self.sync_ownership();
    }

    /// Acquires a connection from `pool`, waiting up to [`POOL_CHECKOUT_TIMEOUT_SECS`].
    fn checkout_connection(&self) -> AppResult<Connection> {
        let mut guard = self
            .inner
            .pool
            .lock()
            .map_err(|_| AppError::Database("SQLite pool mutex poisoned".into()))?;
        let deadline = std::time::Instant::now() + Duration::from_secs(POOL_CHECKOUT_TIMEOUT_SECS);
        loop {
            if guard.closed {
                return Err(AppError::Database("Database pool is closed".into()));
            }
            if let Some(conn) = guard.idle.pop() {
                return Ok(conn);
            }
            let Some(rem) = deadline.checked_duration_since(std::time::Instant::now()) else {
                return Err(AppError::Database(
                    "SQLite connection pool checkout timed out".into(),
                ));
            };
            let (next_guard, _) = self
                .inner
                .pool_cv
                .wait_timeout(guard, rem)
                .map_err(|_| AppError::Database("SQLite pool condvar poisoned".into()))?;
            guard = next_guard;
        }
    }

    /// Returns a checked-out connection back to `pool` unless closed.
    fn return_connection(&self, conn: Connection) {
        if let Ok(mut guard) = self.inner.pool.lock() {
            if !guard.closed {
                guard.idle.push(conn);
                self.inner.pool_cv.notify_one();
            }
        }
    }
}

// =============================================================================
/// Opens a single SQLite connection and applies all `WAL` and flash wear-leveling PRAGMAs.
///
/// # Errors
/// Returns [`AppError::Database`] if opening the file or setting PRAGMAs fails.
fn open_configured_connection(db_path: &Path) -> AppResult<Connection> {
    let conn = if db_path.as_os_str() == ":memory:" {
        Connection::open_in_memory()?
    } else {
        Connection::open(db_path)?
    };
    conn.busy_timeout(Duration::from_millis(u64::from(BUSY_TIMEOUT_MS)))?;
    let fk_val = if FOREIGN_KEYS { "ON" } else { "OFF" };
    let pragma_batch = format!(
        "PRAGMA journal_mode = {JOURNAL_MODE};
         PRAGMA synchronous = {SYNCHRONOUS};
         PRAGMA journal_size_limit = {JOURNAL_SIZE_LIMIT_BYTES};
         PRAGMA cache_size = {CACHE_SIZE_KIB};
         PRAGMA temp_store = {TEMP_STORE};
         PRAGMA mmap_size = {MMAP_SIZE_BYTES};
         PRAGMA foreign_keys = {fk_val};"
    );
    conn.execute_batch(&pragma_batch)?;
    Ok(conn)
}
