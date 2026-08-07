//! PostgreSQL applier: creates the mallard-compatible `mallard.applied_migrations`
//! tracking table plus the per-schema association table `mallard.applied_migration_schemas`
//! ([`ensure_schema`]), reads the per-schema applied set ([`load_applied_for_schema`]) and
//! the canonical checksums ([`load_applied`]), and applies a migration to a target schema
//! with its bookkeeping rows in one transaction ([`apply_one`]).
//!
//! ## Multi-schema model
//!
//! The same migration set can be applied to many schemas (schema-per-customer / RAG).
//! [`apply_one`] creates the target schema on demand (`CREATE SCHEMA IF NOT EXISTS`, in
//! the same transaction as the migration it's about to run) — unlike mallard, no
//! dedicated `CREATE SCHEMA` migration is required first. Migrations are **templates**:
//! unqualified DDL lands wherever `search_path` points, so [`apply_one`] runs each body
//! under `SET LOCAL search_path TO <schema>`. Explicitly schema-qualified DDL still goes
//! where it names.
//!
//! > Because the body runs with `search_path` set to the target schema *only*, a migration
//! > that references an object in another schema (e.g. an extension function) by an
//! > unqualified name will not resolve it. Schema-qualify such references.
//!
//! Two tables track state inside the `mallard` schema:
//! - `mallard.applied_migrations` — one row per migration *name*; holds the canonical
//!   body + checksum (schema-independent, so verified once). Byte-for-byte mallard's own
//!   shape (no kryzhen-only columns).
//! - `mallard.applied_migration_schemas` — one row per (migration, schema); the per-schema
//!   skip-set. Sparse: different schemas may sit at different points in the chain.
//!
//! ## Self-migration (mirrors mallard's own bootstrap)
//!
//! `mallard.migrator_version` is not kryzhen's table to redefine — it is mallard's *own*
//! self-migration ledger, and stock mallard already uses it to bring itself up via two
//! embedded scripts, versions 0 and 1 (`sql/mallard/0000-setup.sql` creates the `mallard`
//! schema and `migrator_version` itself; `0001-applied-migrations.sql` creates
//! `applied_migrations`, exactly as shipped upstream — see [`SELF_MIGRATIONS`]). kryzhen
//! applies those two verbatim, then layers its own version 2
//! (`0002-multi-schema-tracking.sql`) on top, which retrofits the `UNIQUE(name)` index and
//! creates `applied_migration_schemas`. [`ensure_schema`] runs whichever of the three are
//! still missing, recorded as ordinary rows in `mallard.migrator_version` — the same table
//! and the same mechanism real mallard uses for its own two, so kryzhen can extend a
//! database a stock mallard already manages without touching mallard's own rows.
//!
//! An earlier kryzhen (pre-dating this scheme) stamped a meaningless one-row
//! `(singleton boolean, version bigint)` table under this same name; [`ensure_schema`]
//! detects and replaces it (see the module-private legacy-migration helper) before
//! computing the current version.

use crate::types::{Migration, MigrationName};
use crate::Result;
use std::collections::{HashMap, HashSet};
use tokio_postgres::GenericClient;

/// The schema migrations target when none is specified (the CLI's `--schema` default).
/// Matches plain libpq behaviour where unqualified objects land in `public`.
pub const DEFAULT_SCHEMA: &str = "public";

/// kryzhen's self-migrations, applied in order by [`ensure_schema`] and recorded one row
/// per version in `mallard.migrator_version` — the same ledger and mechanism stock
/// mallard uses for its own bootstrap.
///
/// Versions 0 and 1 are mallard's own scripts, embedded byte-for-byte from upstream:
/// pointed at a database a real mallard already manages, both are already satisfied (the
/// ledger already has those rows) and only version 2 — kryzhen's own addition — applies.
const SELF_MIGRATIONS: [(i64, &str); 3] = [
    (0, include_str!("../sql/mallard/0000-setup.sql")),
    (
        1,
        include_str!("../sql/mallard/0001-applied-migrations.sql"),
    ),
    (
        2,
        include_str!("../sql/mallard/0002-multi-schema-tracking.sql"),
    ),
];

/// mallard's real `migrator_version` shape: one row per version reached, no upsert —
/// matches upstream's `sql/mallard/0000-setup.sql` exactly.
const CREATE_MIGRATOR_VERSION_TABLE: &str = "\
CREATE TABLE mallard.migrator_version( \
    version     bigint       NOT NULL, \
    applied_on  timestamptz  NOT NULL DEFAULT now(), \
    PRIMARY KEY (version) \
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

/// True if `schema.table` exists (safe to call before `schema` itself exists).
async fn table_exists(client: &impl GenericClient, schema: &str, table: &str) -> Result<bool> {
    let row = client
        .query_opt(
            "SELECT 1 FROM information_schema.tables \
             WHERE table_schema = $1 AND table_name = $2",
            &[&schema, &table],
        )
        .await?;
    Ok(row.is_some())
}

/// True if `schema.table` has a column named `column`.
async fn column_exists(
    client: &impl GenericClient,
    schema: &str,
    table: &str,
    column: &str,
) -> Result<bool> {
    let row = client
        .query_opt(
            "SELECT 1 FROM information_schema.columns \
             WHERE table_schema = $1 AND table_name = $2 AND column_name = $3",
            &[&schema, &table, &column],
        )
        .await?;
    Ok(row.is_some())
}

/// Replace an earlier kryzhen's meaningless `(singleton boolean, version bigint)`
/// `mallard.migrator_version` with mallard's real shape.
///
/// That legacy table only ever recorded one thing — its own `MULTI_SCHEMA_VERSION = 1` —
/// stamped once `applied_migrations` had its `UNIQUE(name)` index and
/// `applied_migration_schemas` existed. That is exactly the combined effect of
/// [`SELF_MIGRATIONS`] 0 through 2, so a database with the legacy table already satisfies
/// all three; replace it and stamp them all as reached rather than re-running anything.
async fn migrate_legacy_migrator_version(client: &mut impl GenericClient) -> Result<()> {
    if !column_exists(client, "mallard", "migrator_version", "singleton").await? {
        return Ok(());
    }
    let tx = client.transaction().await?;
    tx.batch_execute("DROP TABLE mallard.migrator_version")
        .await?;
    tx.batch_execute(CREATE_MIGRATOR_VERSION_TABLE).await?;
    for (v, _) in SELF_MIGRATIONS {
        tx.execute(
            "INSERT INTO mallard.migrator_version (version) VALUES ($1)",
            &[&v],
        )
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// The highest [`SELF_MIGRATIONS`] version already reached, or -1 if none has.
///
/// A database with no ledger yet but an existing `applied_migrations` table was created
/// by a pre-ledger kryzhen, whose `applied_migrations` shape has always matched mallard's
/// real one — i.e. versions 0 and 1 are already satisfied. Their `CREATE SCHEMA` /
/// `CREATE TABLE applied_migrations` cannot be replayed (both already exist), so create
/// just the ledger table and stamp 0 and 1 directly.
async fn current_self_migration_version(client: &mut impl GenericClient) -> Result<i64> {
    if table_exists(client, "mallard", "migrator_version").await? {
        let row = client
            .query_opt("SELECT max(version) FROM mallard.migrator_version", &[])
            .await?;
        let version: Option<i64> = row.and_then(|r| r.get(0));
        return Ok(version.unwrap_or(-1));
    }

    if table_exists(client, "mallard", "applied_migrations").await? {
        client.batch_execute(CREATE_MIGRATOR_VERSION_TABLE).await?;
        client
            .batch_execute("INSERT INTO mallard.migrator_version (version) VALUES (0), (1)")
            .await?;
        return Ok(1);
    }

    Ok(-1)
}

/// Bring `mallard.migrator_version`, `mallard.applied_migrations` and
/// `mallard.applied_migration_schemas` up to date by running whichever of
/// [`SELF_MIGRATIONS`] are still missing, each in its own transaction alongside its
/// version stamp — mirroring how mallard applies its own two self-migrations.
///
/// Handles three starting states: a totally fresh database (all three run); one created
/// by a pre-ledger kryzhen (0 and 1 are backfilled, only 2 actually runs); and one
/// created by an earlier kryzhen's meaningless legacy ledger (all three backfilled, see
/// [`migrate_legacy_migrator_version`]). Pointed at a database a real stock mallard
/// already manages, 0 and 1 are already in its own ledger and only kryzhen's own 2 runs.
pub async fn ensure_schema(client: &mut impl GenericClient) -> Result<()> {
    // Self-migrations 0/1 are mallard's own scripts and set search_path non-locally
    // (matching upstream byte-for-byte); restore the caller's own default afterward so
    // this call has no lingering effect on a long-lived connection's session state.
    //
    // `current_schema()` alone is NOT enough to restore from: it is only the first
    // *resolvable* entry of search_path (e.g. a connection configured with
    // `ag_catalog, "$user", public` reports current_schema() = "ag_catalog"), so
    // restoring via `SET search_path TO <that one schema>` would silently narrow a
    // multi-entry search_path down to one. Capture the raw setting via `SHOW` and
    // restore that verbatim; `current_schema()` is still what per-schema backfill below
    // records against (the schema these pre-existing rows actually resolve to).
    let original_schema: String = client
        .query_one("SELECT current_schema()", &[])
        .await?
        .get(0);
    let original_search_path: String = client.query_one("SHOW search_path", &[]).await?.get(0);

    migrate_legacy_migrator_version(client).await?;
    let version_before = current_self_migration_version(client).await?;
    let mut version = version_before;

    for (v, script) in SELF_MIGRATIONS {
        if v <= version {
            continue;
        }
        let tx = client.transaction().await?;
        tx.batch_execute(script).await?;
        tx.execute(
            "INSERT INTO mallard.migrator_version (version) VALUES ($1)",
            &[&v],
        )
        .await?;
        tx.commit().await?;
        version = v;
    }

    client
        .batch_execute(&format!("SET search_path TO {original_search_path}"))
        .await?;

    // One-time backfill, run only when self-migration 2 (applied_migration_schemas) was
    // just now newly reached in this call — never on a call that finds it already applied,
    // or its association rows would gain a stray entry for whatever schema this
    // particular connection happens to resolve to on every subsequent call. Covers a
    // pre-ledger/legacy kryzhen's existing rows and, extending a real stock-mallard
    // database, mallard's own pre-existing rows (it never tracked schemas at all) —
    // against the schema this connection resolves to.
    let (last_version, _) = SELF_MIGRATIONS[SELF_MIGRATIONS.len() - 1];
    if version_before < last_version {
        client
            .execute(
                "INSERT INTO mallard.applied_migration_schemas (migration_name, schema) \
                 SELECT name, $1 FROM mallard.applied_migrations \
                 ON CONFLICT (migration_name, schema) DO NOTHING",
                &[&original_schema],
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
/// In a single transaction: ensure `schema` itself exists (unlike mallard, kryzhen does
/// not require a dedicated `CREATE SCHEMA` migration first — the target schema is
/// created on demand), set `search_path` to it (so unqualified DDL templates into it),
/// run the body, upsert the canonical `applied_migrations` row on first sighting, and
/// record the `(name, schema)` association. Everything commits together, so a failure
/// leaves no partial state — including no orphaned empty schema.
pub async fn apply_one(client: &mut impl GenericClient, m: &Migration, schema: &str) -> Result<()> {
    let tx = client.transaction().await?;
    tx.batch_execute(&format!(
        "CREATE SCHEMA IF NOT EXISTS {}",
        quote_ident(schema)
    ))
    .await?;
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
