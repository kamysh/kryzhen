//! kryzhen — forward-only, dependency-resolved SQL migrations for PostgreSQL.
//!
//! A Rust port of the Haskell [`mallard`](https://hackage.haskell.org/package/mallard)
//! tool. Migrations are plain `.sql` files carrying a `#!migration` header in SQL
//! comments; kryzhen parses them, resolves their dependency graph into a topological
//! order, and applies the pending ones inside individual transactions. It is
//! **forward-only** (accretive — there are no down-migrations) and records applied
//! migrations in a `mallard.applied_migrations` table that is byte-compatible with
//! mallard's own, so the two tools can share a database.
//!
//! # What it does
//!
//! [`migrate`] runs the whole pipeline:
//!
//! 1. [`file::load_dir`] walks the root directory and parses every `.sql` file into
//!    its `#!migration` blocks (multiple blocks per file are allowed; each block after
//!    the first implicitly requires its in-file predecessor).
//! 2. [`validation::check_duplicate_names`] rejects duplicate migration names.
//! 3. [`graph::topo_sort`] orders migrations so each runs after its `requires`,
//!    rejecting cycles and dangling dependencies.
//! 4. It connects to PostgreSQL, ensures the `mallard` schema/table exist, and loads
//!    the set of already-applied migrations.
//! 5. [`validation::check_checksums`] aborts if any already-applied migration's file
//!    has changed since it was applied (tamper detection).
//! 6. Each pending migration is applied in its own transaction together with its
//!    bookkeeping row, in dependency order. Already-applied migrations are skipped,
//!    so running twice is safe.
//!
//! `#!test` blocks (which mallard supports) are **not** supported and are a parse
//! error — kryzhen handles migrations only.
//!
//! # Example
//!
//! ```no_run
//! use kryzhen::{migrate, Config};
//! use std::path::PathBuf;
//!
//! # async fn run() -> kryzhen::Result<()> {
//! let report = migrate(Config {
//!     root: PathBuf::from("migrations"),
//!     host: "127.0.0.1".into(),
//!     port: 5432,
//!     user: "postgres".into(),
//!     password: "secret".into(),
//!     database: "mydb".into(),
//!     sslmode: kryzhen::SslMode::Prefer,
//!     dry_run: false,
//! })
//! .await?;
//!
//! println!("applied {} migration(s): {:?}", report.applied.len(), report.applied);
//! # Ok(())
//! # }
//! ```
//!
//! # Migration file format
//!
//! ```sql
//! -- #!migration
//! -- name: "tables/phone",
//! -- description: "Phone numbers attached to a person.",
//! -- requires: ["tables/person"];
//! CREATE TABLE phone (id bigint);
//! ```
//!
//! Header fields are comma-separated, the last terminated with `;`. `name` and
//! `description` are required strings; `requires` is optional and may be a single
//! `"name"` or a list `["a", "b"]`. The SQL body follows the header.

pub mod file;
pub mod graph;
pub mod parser;
pub mod postgres;
pub mod types;
pub mod validation;

pub use types::{checksum, Error, Migration, MigrationName};

/// Library result type — `Result<T, `[`Error`]`>`.
pub type Result<T> = std::result::Result<T, Error>;

use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;

/// How kryzhen negotiates TLS when connecting to PostgreSQL.
///
/// The semantics mirror libpq's `sslmode` (the subset kryzhen supports). In
/// [`Prefer`](SslMode::Prefer) and [`Require`](SslMode::Require) the server
/// certificate is **not** verified against a CA (encryption without
/// authentication) — matching libpq's behaviour for those modes. Certificate
/// verification (`verify-ca`/`verify-full`) is not yet supported.
///
/// ```
/// use kryzhen::SslMode;
/// assert_eq!(SslMode::default(), SslMode::Prefer);
/// assert_eq!("require".parse::<SslMode>().unwrap(), SslMode::Require);
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SslMode {
    /// Never use TLS; connect in plaintext only.
    Disable,
    /// Try TLS first, fall back to plaintext if the server does not offer it.
    /// This is the default, matching libpq.
    #[default]
    Prefer,
    /// Require TLS; fail if the server does not offer it.
    Require,
}

impl std::fmt::Display for SslMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            SslMode::Disable => "disable",
            SslMode::Prefer => "prefer",
            SslMode::Require => "require",
        };
        f.write_str(s)
    }
}

impl FromStr for SslMode {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "disable" => Ok(SslMode::Disable),
            "prefer" => Ok(SslMode::Prefer),
            "require" => Ok(SslMode::Require),
            other => Err(format!(
                "invalid sslmode {other:?} (expected disable, prefer, or require)"
            )),
        }
    }
}

/// Connection and run configuration for [`migrate`].
///
/// The connection string is built from the individual `host`/`port`/`user`/
/// `password`/`database` fields; TLS negotiation is controlled by
/// [`sslmode`](Config::sslmode).
///
/// ```
/// use kryzhen::{Config, SslMode};
/// use std::path::PathBuf;
///
/// let config = Config {
///     root: PathBuf::from("migrations"),
///     host: "127.0.0.1".into(),
///     port: 5432,
///     user: "postgres".into(),
///     password: String::new(),
///     database: "mydb".into(),
///     sslmode: SslMode::Prefer,
///     dry_run: true,
/// };
/// assert!(config.dry_run);
/// ```
#[derive(Clone, Debug)]
pub struct Config {
    /// Root directory of the migration tree (searched recursively for `*.sql`).
    pub root: PathBuf,
    /// PostgreSQL server host.
    pub host: String,
    /// PostgreSQL server port.
    pub port: u16,
    /// Database user.
    pub user: String,
    /// Database password (may be empty).
    pub password: String,
    /// Database name to connect to.
    pub database: String,
    /// TLS negotiation mode (see [`SslMode`]).
    pub sslmode: SslMode,
    /// If `true`, resolve and plan the migrations but apply nothing. kryzhen still
    /// connects to the database to load the applied set and verify checksums.
    pub dry_run: bool,
}

/// Summary of a [`migrate`] run.
///
/// Both lists are in topological (apply) order.
#[derive(Clone, Debug, Default)]
pub struct Report {
    /// Migration names applied this run — or, when [`Config::dry_run`] is set, the
    /// names that *would* be applied.
    pub applied: Vec<String>,
    /// Migration names that were already present in the database before this run.
    pub already_applied: Vec<String>,
}

/// Run all pending migrations under `config.root` against the configured database.
///
/// Performs the full pipeline described in the [crate-level docs](crate): load and
/// parse migrations, check for duplicate names, topologically sort by `requires`,
/// connect, ensure the `mallard.applied_migrations` table exists, verify the
/// checksums of already-applied migrations (tamper detection), then apply each
/// pending migration in dependency order inside its own transaction.
///
/// Returns a [`Report`] listing what was applied (or, under
/// [`dry_run`](Config::dry_run), what would be applied) and what was already present.
///
/// # Errors
///
/// Returns an [`Error`] if a file fails to parse ([`Error::Parse`]), names collide
/// ([`Error::DuplicateName`]), the dependency graph has a cycle ([`Error::Cycle`]) or
/// a dangling reference ([`Error::DanglingDependency`]), an already-applied migration
/// has been edited ([`Error::ChecksumMismatch`]), or a database operation fails
/// ([`Error::Db`]). A migration whose SQL fails rolls back and aborts the run;
/// migrations committed earlier in the run remain applied (forward-only).
pub async fn migrate(config: Config) -> Result<Report> {
    let migrations = file::load_dir(&config.root)?;
    validation::check_duplicate_names(&migrations)?;
    let ordered = graph::topo_sort(migrations)?;

    let mut client = connect_db(&config).await?;

    postgres::ensure_schema(&client).await?;
    let applied: HashMap<MigrationName, [u8; 32]> = postgres::load_applied(&client).await?;
    validation::check_checksums(&ordered, &applied)?;

    let mut report = Report::default();
    for m in &ordered {
        if applied.contains_key(&m.name) {
            report.already_applied.push(m.name.0.clone());
            continue;
        }
        if !config.dry_run {
            postgres::apply_one(&mut client, m).await?;
        }
        report.applied.push(m.name.0.clone());
    }
    Ok(report)
}

/// Connect to PostgreSQL per `config`, spawning the connection task and returning
/// the live client. `sslmode` is passed through the connection string so
/// tokio-postgres drives negotiation (including `prefer`'s plaintext fallback);
/// `disable` uses [`NoTls`], while the TLS modes use a native-tls connector that
/// encrypts without verifying the server certificate (matching libpq's
/// `prefer`/`require` semantics — `verify-ca`/`verify-full` are not yet supported).
async fn connect_db(config: &Config) -> Result<tokio_postgres::Client> {
    use tokio_postgres::NoTls;

    let conn_str = format!(
        "host={} port={} user={} password={} dbname={} sslmode={}",
        config.host, config.port, config.user, config.password, config.database, config.sslmode
    );

    // The two connectors yield different `Connection` types, so each arm spawns its
    // own connection-driver task and returns just the `Client`.
    match config.sslmode {
        SslMode::Disable => {
            let (client, conn) = tokio_postgres::connect(&conn_str, NoTls).await?;
            tokio::spawn(async move {
                let _ = conn.await;
            });
            Ok(client)
        }
        SslMode::Prefer | SslMode::Require => {
            let connector = native_tls::TlsConnector::builder()
                .danger_accept_invalid_certs(true)
                .danger_accept_invalid_hostnames(true)
                .build()?;
            let tls = postgres_native_tls::MakeTlsConnector::new(connector);
            let (client, conn) = tokio_postgres::connect(&conn_str, tls).await?;
            tokio::spawn(async move {
                let _ = conn.await;
            });
            Ok(client)
        }
    }
}
