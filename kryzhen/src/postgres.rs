//! PostgreSQL applier: creates the mallard-compatible `mallard.applied_migrations`
//! tracking table ([`ensure_schema`]), reads applied migrations ([`load_applied`]), and
//! applies a migration with its bookkeeping row in one transaction ([`apply_one`]).

use crate::types::{Migration, MigrationName};
use crate::Result;
use std::collections::HashMap;
use tokio_postgres::GenericClient;

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

/// Create the mallard schema and tracking table if they do not exist.
pub async fn ensure_schema(client: &impl GenericClient) -> Result<()> {
    client.batch_execute(ENSURE_SCHEMA).await?;
    client.batch_execute(ENSURE_TABLE).await?;
    Ok(())
}

/// Load applied migration names and their stored checksums.
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

/// Apply one migration and record it, atomically.
pub async fn apply_one(client: &mut impl GenericClient, m: &Migration) -> Result<()> {
    let tx = client.transaction().await?;
    tx.batch_execute(&m.script).await?;
    let requires: Vec<String> = m.requires.iter().map(|r| r.0.clone()).collect();
    let checksum_bytes: &[u8] = &m.checksum;
    tx.execute(
        "INSERT INTO mallard.applied_migrations \
         (name, description, requires, checksum, script_text) \
         VALUES ($1, $2, $3, $4, $5)",
        &[
            &m.name.0,
            &m.description,
            &requires,
            &checksum_bytes,
            &m.script,
        ],
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Record a migration as applied without running its SQL (used by `hack_add` and
/// `migrate_from_sqlx`).
pub async fn record_applied(client: &impl GenericClient, m: &Migration) -> Result<()> {
    let requires: Vec<String> = m.requires.iter().map(|r| r.0.clone()).collect();
    let checksum_bytes: &[u8] = &m.checksum;
    client
        .execute(
            "INSERT INTO mallard.applied_migrations \
             (name, description, requires, checksum, script_text) \
             VALUES ($1, $2, $3, $4, $5)",
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

/// Remove a migration record without running any SQL (used by `hack_delete`).
pub async fn remove_applied(client: &impl GenericClient, name: &MigrationName) -> Result<()> {
    client
        .execute(
            "DELETE FROM mallard.applied_migrations WHERE name = $1",
            &[&name.0],
        )
        .await?;
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

/// Load all applied migration names and their `requires` arrays.
pub async fn load_applied_with_requires(
    client: &impl GenericClient,
) -> Result<HashMap<MigrationName, Vec<MigrationName>>> {
    let rows = client
        .query("SELECT name, requires FROM mallard.applied_migrations", &[])
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
