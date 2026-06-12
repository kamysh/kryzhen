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

/// Connection and run configuration for [`migrate`].
///
/// The connection string is built from the individual `host`/`port`/`user`/
/// `password`/`database` fields (kryzhen connects with `NoTls`).
///
/// ```
/// use kryzhen::Config;
/// use std::path::PathBuf;
///
/// let config = Config {
///     root: PathBuf::from("migrations"),
///     host: "127.0.0.1".into(),
///     port: 5432,
///     user: "postgres".into(),
///     password: String::new(),
///     database: "mydb".into(),
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
    use tokio_postgres::NoTls;

    let migrations = file::load_dir(&config.root)?;
    validation::check_duplicate_names(&migrations)?;
    let ordered = graph::topo_sort(migrations)?;

    let conn_str = format!(
        "host={} port={} user={} password={} dbname={}",
        config.host, config.port, config.user, config.password, config.database
    );
    let (mut client, connection) = tokio_postgres::connect(&conn_str, NoTls).await?;
    tokio::spawn(async move {
        let _ = connection.await;
    });

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
