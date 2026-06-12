//! PostgreSQL applier: creates the mallard-compatible `mallard.applied_migrations`
//! tracking table ([`ensure_schema`]), reads applied migrations ([`load_applied`]), and
//! applies a migration with its bookkeeping row in one transaction ([`apply_one`]).

use crate::types::{Migration, MigrationName};
use crate::Result;
use std::collections::HashMap;
use tokio_postgres::Client;

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
pub async fn ensure_schema(client: &Client) -> Result<()> {
    client.batch_execute(ENSURE_SCHEMA).await?;
    client.batch_execute(ENSURE_TABLE).await?;
    Ok(())
}

/// Load applied migration names and their stored checksums.
pub async fn load_applied(client: &Client) -> Result<HashMap<MigrationName, [u8; 32]>> {
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
pub async fn apply_one(client: &mut Client, m: &Migration) -> Result<()> {
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
