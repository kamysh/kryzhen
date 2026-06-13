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
//! 1. Validate and topologically sort `migrations` (pre-loaded by the caller via
//!    [`file::load_dir`]).
//! 2. Ensure the `mallard` schema/table exists on the supplied connection.
//! 3. [`validation::check_checksums`] aborts if any already-applied migration's file
//!    has changed since it was applied (tamper detection).
//! 4. Each pending migration is applied in its own transaction together with its
//!    bookkeeping row, in dependency order. Already-applied migrations are skipped,
//!    so running twice is safe.
//!
//! `#!test` blocks (which mallard supports) are **not** supported and are a parse
//! error — kryzhen handles migrations only.
//!
//! # Example
//!
//! ```no_run
//! use kryzhen::{file, migrate, Report};
//! use std::path::PathBuf;
//!
//! # async fn run() -> kryzhen::Result<()> {
//! // The caller owns the connection.
//! let (mut client, conn) = tokio_postgres::connect(
//!     "host=127.0.0.1 user=postgres dbname=mydb",
//!     tokio_postgres::NoTls,
//! ).await?;
//! tokio::spawn(async move { let _ = conn.await; });
//!
//! let migrations = file::load_dir(std::path::Path::new("migrations"))?;
//! let report: Report = migrate(&mut client, &migrations, false).await?;
//! println!("applied {} migration(s)", report.applied.len());
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
pub mod sqlx_import;
pub mod types;
pub mod validation;

pub use types::{checksum, Error, Migration, MigrationName};

/// Library result type — `Result<T, `[`Error`]`>`.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors specific to [`hack_add`] and [`hack_delete`].
#[derive(Debug, thiserror::Error)]
pub enum HackError {
    #[error("migration {0:?} not found in the supplied migration list")]
    NotFound(String),
    #[error("cannot add {name:?}: required migration {missing:?} is not yet applied")]
    UnsatisfiedRequires { name: String, missing: String },
    #[error("cannot delete {name:?}: it is required by applied migration(s): {dependents:?}")]
    HasDependents {
        name: String,
        dependents: Vec<String>,
    },
    #[error(transparent)]
    Core(#[from] Error),
}

/// Summary of a [`migrate`] run.
///
/// Both lists are in topological (apply) order.
#[derive(Clone, Debug, Default)]
pub struct Report {
    /// Migration names applied this run — or, when `dry_run` is set, the names
    /// that *would* be applied.
    pub applied: Vec<String>,
    /// Migration names that were already present in the database before this run.
    pub already_applied: Vec<String>,
}

use std::collections::HashMap;
use tokio_postgres::GenericClient;

/// Run all pending migrations against the supplied database connection.
///
/// `migrations` must be pre-loaded by the caller (e.g. via [`file::load_dir`]).
/// The function validates names, topologically sorts by `requires`, ensures the
/// `mallard.applied_migrations` table exists, verifies checksums of already-applied
/// migrations, then applies each pending migration in dependency order inside its own
/// transaction.
///
/// When `dry_run` is `true`, nothing is applied; the function still connects and
/// verifies checksums.
pub async fn migrate(
    client: &mut impl GenericClient,
    migrations: &[Migration],
    dry_run: bool,
) -> Result<Report> {
    validation::check_duplicate_names(migrations)?;
    let ordered = graph::topo_sort(migrations.to_vec())?;

    postgres::ensure_schema(client).await?;
    let applied: HashMap<MigrationName, [u8; 32]> = postgres::load_applied(client).await?;
    validation::check_checksums(&ordered, &applied)?;

    let mut report = Report::default();
    for m in &ordered {
        if applied.contains_key(&m.name) {
            report.already_applied.push(m.name.0.clone());
            continue;
        }
        if !dry_run {
            postgres::apply_one(client, m).await?;
        }
        report.applied.push(m.name.0.clone());
    }
    Ok(report)
}

/// Record a migration as applied without running its SQL.
///
/// Looks up `name` in `migrations`, checks that all its `requires` are already
/// present in `mallard.applied_migrations`, then inserts the record.
///
/// Returns [`HackError::UnsatisfiedRequires`] if any dependency is not yet applied,
/// or [`HackError::NotFound`] if `name` is not in `migrations`.
pub async fn hack_add(
    client: &impl GenericClient,
    migrations: &[Migration],
    name: &str,
) -> std::result::Result<(), HackError> {
    let m = migrations
        .iter()
        .find(|m| m.name.0 == name)
        .ok_or_else(|| HackError::NotFound(name.to_string()))?;

    postgres::ensure_schema(client).await?;
    let applied = postgres::load_applied(client).await?;

    for req in &m.requires {
        if !applied.contains_key(req) {
            return Err(HackError::UnsatisfiedRequires {
                name: name.to_string(),
                missing: req.0.clone(),
            });
        }
    }

    postgres::record_applied(client, m).await?;
    Ok(())
}

/// Update the stored checksum and script_text for an already-applied migration to match
/// the current on-disk content.
///
/// Looks up `name` in `migrations` (pre-loaded from disk) and overwrites the stored
/// checksum and script_text in `mallard.applied_migrations`. Returns
/// [`HackError::NotFound`] if the name is absent from the supplied migration list or
/// not yet recorded in the database.
pub async fn hack_fix_checksum(
    client: &impl GenericClient,
    migrations: &[Migration],
    name: &str,
) -> std::result::Result<(), HackError> {
    let m = migrations
        .iter()
        .find(|m| m.name.0 == name)
        .ok_or_else(|| HackError::NotFound(name.to_string()))?;

    let applied = postgres::load_applied(client).await?;
    if !applied.contains_key(&m.name) {
        return Err(HackError::NotFound(name.to_string()));
    }

    postgres::update_checksum(client, m).await?;
    Ok(())
}

/// Remove a migration record without running any SQL.
///
/// Checks that no currently-applied migration lists `name` in its `requires`.
/// Returns [`HackError::HasDependents`] if any dependents exist.
pub async fn hack_delete(
    client: &impl GenericClient,
    name: &str,
) -> std::result::Result<(), HackError> {
    postgres::ensure_schema(client).await?;

    let applied_with_requires = postgres::load_applied_with_requires(client).await?;
    let target = MigrationName(name.to_string());

    let dependents: Vec<String> = applied_with_requires
        .iter()
        .filter(|(n, reqs)| *n != &target && reqs.contains(&target))
        .map(|(n, _)| n.0.clone())
        .collect();

    if !dependents.is_empty() {
        return Err(HackError::HasDependents {
            name: name.to_string(),
            dependents,
        });
    }

    postgres::remove_applied(client, &target).await?;
    Ok(())
}
