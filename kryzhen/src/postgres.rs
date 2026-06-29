//! PostgreSQL applier: creates the mallard-compatible `mallard.applied_migrations`
//! tracking table plus the per-schema association table `mallard.applied_migration_schemas`
//! ([`ensure_schema`]), reads the per-schema applied set ([`load_applied_for_schema`]) and
//! the canonical checksums ([`load_applied`]), and applies a migration to a target schema
//! with its bookkeeping rows in one transaction ([`apply_one`]).
//!
//! ## Multi-schema model
//!
//! The same migration set can be applied to many schemas (schema-per-customer / RAG).
//! Migrations are **templates**: unqualified DDL lands wherever `search_path` points, so
//! [`apply_one`] runs each body under `SET LOCAL search_path TO <schema>`. Explicitly
//! schema-qualified DDL still goes where it names.
//!
//! > Because the body runs with `search_path` set to the target schema *only*, a migration
//! > that references an object in another schema (e.g. an extension function) by an
//! > unqualified name will not resolve it. Schema-qualify such references.
//!
//! Two tables track state inside the `mallard` schema:
//! - `mallard.applied_migrations` — one row per migration *name*; holds the canonical
//!   body + checksum (schema-independent, so verified once). Upstream-mallard-shaped.
//! - `mallard.applied_migration_schemas` — one row per (migration, schema); the per-schema
//!   skip-set. Sparse: different schemas may sit at different points in the chain.
//!
//! A `mallard.migrator_version` table records the ledger-format version and drives a
//! one-time soft upgrade of pre-multi-schema databases (see [`ensure_schema`]).

use crate::types::{Migration, MigrationName};
use crate::Result;
use std::collections::{HashMap, HashSet};
use tokio_postgres::GenericClient;

/// The schema migrations target when none is specified (the CLI's `--schema` default).
/// Matches plain libpq behaviour where unqualified objects land in `public`.
pub const DEFAULT_SCHEMA: &str = "public";

/// Ledger-format version stamped once the per-schema association table exists and has
/// been backfilled. Pre-multi-schema databases report 0 (no `migrator_version` row).
const MULTI_SCHEMA_VERSION: i64 = 1;

const ENSURE_SCHEMA: &str = "CREATE SCHEMA IF NOT EXISTS mallard";

const ENSURE_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS mallard.applied_migrations( \
    id           bigserial    NOT NULL, \
    name         text         NOT NULL, \
    description  text         NOT NULL, \
    requires     text[]       NOT NULL, \
    checksum     bytea        NOT NULL, \
    script_text  text         NOT NULL, \
    applied_on   timestamptz  NOT NULL DEFAULT now(), \
    PRIMARY KEY (id) \
)";

// The unique constraint on `name` is created as a separate index rather than inline in
// ENSURE_TABLE: `CREATE TABLE IF NOT EXISTS` is a no-op on a pre-existing table, so an
// inline `UNIQUE (name)` would never be added to databases created by an earlier kryzhen
// (which had only `PRIMARY KEY (id)`). `ON CONFLICT (name)` requires this index to exist,
// so it must be retrofitted on every run. `CREATE UNIQUE INDEX IF NOT EXISTS` is idempotent
// and applies equally to new and pre-existing tables.
const ENSURE_NAME_UNIQUE: &str = "CREATE UNIQUE INDEX IF NOT EXISTS applied_migrations_name_key \
     ON mallard.applied_migrations (name)";

const ENSURE_SCHEMAS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS mallard.applied_migration_schemas( \
    id              uuid         NOT NULL DEFAULT gen_random_uuid(), \
    migration_name  text         NOT NULL, \
    schema          text         NOT NULL, \
    applied_on      timestamptz  NOT NULL DEFAULT now(), \
    PRIMARY KEY (id), \
    UNIQUE (migration_name, schema) \
)";

// Single-row version table: the `singleton` boolean is a fixed primary key constrained to
// `true`, so at most one row can ever exist. This makes the version stamp an idempotent
// upsert (`ON CONFLICT (singleton)`), safe under concurrent first-run upgrades — no
// duplicate rows, no separate read-then-write race.
const ENSURE_VERSION_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS mallard.migrator_version( \
    singleton  boolean  NOT NULL DEFAULT true, \
    version    bigint   NOT NULL, \
    PRIMARY KEY (singleton), \
    CHECK (singleton) \
)";

const INSERT_CANONICAL: &str = "\
INSERT INTO mallard.applied_migrations \
    (name, description, requires, checksum, script_text) \
    VALUES ($1, $2, $3, $4, $5) \
    ON CONFLICT (name) DO NOTHING";

const INSERT_SCHEMA_ASSOC: &str = "\
INSERT INTO mallard.applied_migration_schemas (migration_name, schema) \
    VALUES ($1, $2) \
    ON CONFLICT (migration_name, schema) DO NOTHING";

/// Create the mallard schema, the canonical tracking table (with its retrofitted unique
/// index on `name`), the per-schema association table, and the version table — then run
/// the one-time soft upgrade if needed.
///
/// **Soft upgrade.** A database created by a pre-multi-schema kryzhen has rows in
/// `mallard.applied_migrations` but no association table. Without bookkeeping, the new
/// code would see an empty per-schema set, conclude every migration is unapplied, and
/// re-run them — failing with `relation already exists`. To prevent that, when the
/// stored ledger version is below [`MULTI_SCHEMA_VERSION`] we backfill an association row
/// for every existing canonical migration against the schema this connection resolves to
/// (`current_schema()` — where those objects actually live for this connection), then
/// stamp the version. The backfill is idempotent and the version stamp runs at most once.
pub async fn ensure_schema(client: &impl GenericClient) -> Result<()> {
    client.batch_execute(ENSURE_SCHEMA).await?;
    client.batch_execute(ENSURE_TABLE).await?;
    client.batch_execute(ENSURE_NAME_UNIQUE).await?;
    client.batch_execute(ENSURE_SCHEMAS_TABLE).await?;
    client.batch_execute(ENSURE_VERSION_TABLE).await?;

    let version: i64 = client
        .query_one(
            "SELECT coalesce(max(version), 0) FROM mallard.migrator_version",
            &[],
        )
        .await?
        .get(0);

    if version < MULTI_SCHEMA_VERSION {
        // Existing migrations were applied to whatever schema this connection resolves to;
        // record that schema (current_schema()), not a hardcoded literal, so the backfill
        // is accurate for a non-`public` default search_path too.
        client
            .execute(
                "INSERT INTO mallard.applied_migration_schemas (migration_name, schema) \
                 SELECT name, current_schema() FROM mallard.applied_migrations \
                 ON CONFLICT (migration_name, schema) DO NOTHING",
                &[],
            )
            .await?;
        client
            .execute(
                "INSERT INTO mallard.migrator_version (version) VALUES ($1) \
                 ON CONFLICT (singleton) DO UPDATE SET version = excluded.version \
                 WHERE mallard.migrator_version.version < excluded.version",
                &[&MULTI_SCHEMA_VERSION],
            )
            .await?;
    }

    Ok(())
}

/// Load every migration name's stored checksum from the canonical table.
///
/// The body is schema-independent, so one canonical checksum per name covers all schemas;
/// this is what tamper detection ([`crate::validation::check_checksums`]) verifies.
pub async fn load_applied(client: &impl GenericClient) -> Result<HashMap<MigrationName, [u8; 32]>> {
    let rows = client
        .query("SELECT name, checksum FROM mallard.applied_migrations", &[])
        .await?;
    let mut out = HashMap::new();
    for row in rows {
        let name: String = row.get(0);
        let bytes: Vec<u8> = row.get(1);
        if bytes.len() != 32 {
            return Err(crate::Error::CorruptChecksum {
                name: MigrationName(name),
                len: bytes.len(),
            });
        }
        let mut cs = [0u8; 32];
        cs.copy_from_slice(&bytes);
        out.insert(MigrationName(name), cs);
    }
    Ok(out)
}

/// Load the set of migration names already applied to `schema` (the per-schema skip-set).
pub async fn load_applied_for_schema(
    client: &impl GenericClient,
    schema: &str,
) -> Result<HashSet<MigrationName>> {
    let rows = client
        .query(
            "SELECT migration_name FROM mallard.applied_migration_schemas WHERE schema = $1",
            &[&schema],
        )
        .await?;
    Ok(rows
        .into_iter()
        .map(|row| MigrationName(row.get(0)))
        .collect())
}

/// Quote a schema name as a SQL identifier for `SET search_path` (doubles embedded `"`).
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Upsert the canonical (schema-independent) `applied_migrations` row on first sighting.
async fn insert_canonical(client: &impl GenericClient, m: &Migration) -> Result<()> {
    let requires: Vec<String> = m.requires.iter().map(|r| r.0.clone()).collect();
    let checksum_bytes: &[u8] = &m.checksum;
    client
        .execute(
            INSERT_CANONICAL,
            &[
                &m.name.0,
                &m.description,
                &requires,
                &checksum_bytes,
                &m.script,
            ],
        )
        .await?;
    Ok(())
}

/// Record that migration `name` has been applied to `schema` (idempotent).
async fn insert_schema_assoc(client: &impl GenericClient, name: &str, schema: &str) -> Result<()> {
    client
        .execute(INSERT_SCHEMA_ASSOC, &[&name, &schema])
        .await?;
    Ok(())
}

/// Apply one migration to `schema` and record it, atomically.
///
/// In a single transaction: set `search_path` to `schema` (so unqualified DDL templates
/// into it), run the body, upsert the canonical `applied_migrations` row on first sighting,
/// and record the `(name, schema)` association. Body and bookkeeping commit together, so a
/// failure leaves no partial state.
pub async fn apply_one(client: &mut impl GenericClient, m: &Migration, schema: &str) -> Result<()> {
    let tx = client.transaction().await?;
    // SET LOCAL is scoped to this transaction; the connection's default search_path is
    // restored on commit. Schema name is quoted as an identifier (not a bind parameter —
    // SET does not accept parameters).
    tx.batch_execute(&format!("SET LOCAL search_path TO {}", quote_ident(schema)))
        .await?;
    tx.batch_execute(&m.script).await?;
    insert_canonical(&tx, m).await?;
    insert_schema_assoc(&tx, &m.name.0, schema).await?;
    tx.commit().await?;
    Ok(())
}

/// Record a migration as applied to `schema` without running its SQL (used by `hack_add`
/// and `migrate_from_sqlx`). Upserts the canonical row and records the association in one
/// transaction, so the two never tear apart on a dropped connection.
pub async fn record_applied(
    client: &mut impl GenericClient,
    m: &Migration,
    schema: &str,
) -> Result<()> {
    let tx = client.transaction().await?;
    insert_canonical(&tx, m).await?;
    insert_schema_assoc(&tx, &m.name.0, schema).await?;
    tx.commit().await?;
    Ok(())
}

/// Remove a migration's association with `schema` (used by `hack_delete`). When that was
/// the migration's last remaining schema, also delete the canonical `applied_migrations`
/// row so the migration is fully forgotten. Both steps run in one transaction.
pub async fn remove_applied(
    client: &mut impl GenericClient,
    name: &MigrationName,
    schema: &str,
) -> Result<()> {
    let tx = client.transaction().await?;
    tx.execute(
        "DELETE FROM mallard.applied_migration_schemas \
         WHERE migration_name = $1 AND schema = $2",
        &[&name.0, &schema],
    )
    .await?;
    // Drop the canonical row only if no schema still references this migration.
    tx.execute(
        "DELETE FROM mallard.applied_migrations a \
         WHERE a.name = $1 \
           AND NOT EXISTS ( \
               SELECT 1 FROM mallard.applied_migration_schemas s \
               WHERE s.migration_name = $1 \
           )",
        &[&name.0],
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Update checksum and script_text for an already-applied migration.
pub async fn update_checksum(client: &impl GenericClient, m: &Migration) -> Result<()> {
    let checksum_bytes: &[u8] = &m.checksum;
    client
        .execute(
            "UPDATE mallard.applied_migrations \
             SET checksum = $1, script_text = $2 \
             WHERE name = $3",
            &[&checksum_bytes, &m.script, &m.name.0],
        )
        .await?;
    Ok(())
}

/// Load, for `schema`, each applied migration name and its `requires` array — restricted
/// to migrations actually applied to that schema.
pub async fn load_applied_with_requires(
    client: &impl GenericClient,
    schema: &str,
) -> Result<HashMap<MigrationName, Vec<MigrationName>>> {
    let rows = client
        .query(
            "SELECT a.name, a.requires \
             FROM mallard.applied_migrations a \
             JOIN mallard.applied_migration_schemas s ON s.migration_name = a.name \
             WHERE s.schema = $1",
            &[&schema],
        )
        .await?;
    let mut out = HashMap::new();
    for row in rows {
        let name: String = row.get(0);
        let requires: Vec<String> = row.get(1);
        out.insert(
            MigrationName(name),
            requires.into_iter().map(MigrationName).collect(),
        );
    }
    Ok(out)
}
