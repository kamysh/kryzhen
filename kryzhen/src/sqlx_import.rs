//! Two-phase sqlx → kryzhen migration: convert → import.
//!
//! [`convert`] reads `_sqlx_migrations`, verifies file checksums against the
//! DB, rewrites files with `#!migration` headers, and writes a receipt that
//! anchors the sqlx checksums. [`import`] reads the receipt, re-verifies
//! `_sqlx_migrations` checksums, and inserts rows into
//! `mallard.applied_migrations` (idempotent — skips already-present rows).

use crate::types::{checksum, MigrationName};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha384};
use std::path::{Path, PathBuf};
use tokio_postgres::GenericClient;

// ---------------------------------------------------------------------------
// Receipt
// ---------------------------------------------------------------------------

/// One entry in the receipt — one sqlx migration.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReceiptEntry {
    pub version: i64,
    /// sqlx description field, e.g. `"create users"`.
    pub sqlx_description: String,
    /// Kryzhen name derived from description: strip leading `NNN_`, replace spaces with `-`.
    pub kryzhen_name: String,
    /// Filename relative to migrations dir, e.g. `"001_create_users.sql"`.
    pub filename: String,
    /// Hex SHA-384 as stored in `_sqlx_migrations.checksum`.
    pub sqlx_checksum_hex: String,
}

/// Persisted receipt produced by [`convert`]. Commit this alongside the
/// rewritten migration files. Contains the sqlx checksums needed to verify
/// `_sqlx_migrations` on any machine before importing.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Receipt {
    pub migrations: Vec<ReceiptEntry>,
    /// Number of files actually rewritten this run (0 if all already had headers).
    #[serde(default, skip_serializing)]
    pub newly_converted: usize,
    /// True when import was called but _sqlx_migrations was already dropped.
    #[serde(default, skip_serializing)]
    pub already_imported: bool,
}

/// Errors specific to the sqlx import workflow.
#[derive(Debug, thiserror::Error)]
pub enum SqlxImportError {
    #[error("no receipt found at {path}; run `kryzhen hack migrate-from sqlx convert` first")]
    NoReceipt { path: PathBuf },
    #[error("file not found: {path} (sqlx recorded migration {name:?} as applied)")]
    FileMissing { path: PathBuf, name: String },
    #[error(
        "checksum mismatch for {filename}:\n  expected (sqlx): {expected}\n  got (disk):      {got}\nThe file differs from what sqlx applied."
    )]
    ChecksumMismatch {
        filename: String,
        expected: String,
        got: String,
    },
    #[error(
        "_sqlx_migrations checksum mismatch for {name}: receipt has {receipt_hex}, DB has {db_hex}"
    )]
    ReceiptDbMismatch {
        name: String,
        receipt_hex: String,
        db_hex: String,
    },
    #[error(
        "_sqlx_migrations has {db_count} applied migration(s) but receipt has {receipt_count}"
    )]
    CountMismatch {
        db_count: usize,
        receipt_count: usize,
    },
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("database error: {0}")]
    Db(#[from] tokio_postgres::Error),
    #[error("{0}")]
    Core(#[from] crate::types::Error),
}

// ---------------------------------------------------------------------------
// Name normalisation
// ---------------------------------------------------------------------------

/// Normalise a sqlx description to a kryzhen migration name.
///
/// Rules:
/// - Strip a leading `NNN_` numeric prefix (digits followed by `_`).
/// - Replace every remaining space with `-`.
///
/// Example: `"001_create users"` → `"create-users"`.
pub fn normalise_name(description: &str) -> String {
    let stripped = strip_numeric_prefix(description);
    stripped.replace(' ', "-")
}

fn strip_numeric_prefix(s: &str) -> &str {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i > 0 && i < bytes.len() && bytes[i] == b'_' {
        &s[i + 1..]
    } else {
        s
    }
}

// ---------------------------------------------------------------------------
// Body extraction
// ---------------------------------------------------------------------------

/// Split a raw sqlx file into (description, body).
///
/// Leading `-- comment` lines are collected and joined as the description.
/// The body is everything after the leading comment block, trimmed.
/// If there are no leading comments the description is empty and the full
/// content is the body.
pub fn extract_description_and_body(content: &str) -> (String, String) {
    let mut desc_lines: Vec<&str> = Vec::new();
    let mut rest_start = 0;

    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(comment) = trimmed.strip_prefix("--") {
            desc_lines.push(comment.trim());
            rest_start += line.len() + 1; // +1 for '\n'
        } else {
            break;
        }
    }

    let description = desc_lines
        .into_iter()
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");

    let body = content[rest_start..].trim().to_string();
    (description, body)
}

// ---------------------------------------------------------------------------
// Phase 1: convert
// ---------------------------------------------------------------------------

/// Read `_sqlx_migrations`, verify file checksums, rewrite files with
/// `#!migration` headers, and write `.kryzhen-import-receipt.json`.
///
/// Idempotent: files that already contain a `-- #!migration` header are not
/// rewritten (their header is preserved as-is).
pub async fn convert(
    client: &impl GenericClient,
    migrations_dir: &Path,
) -> std::result::Result<Receipt, SqlxImportError> {
    let rows = client
        .query(
            "SELECT version, description, checksum FROM _sqlx_migrations \
             WHERE success = true ORDER BY version",
            &[],
        )
        .await?;

    let dir_files = collect_sql_files(migrations_dir)?;
    let names: Vec<String> = rows
        .iter()
        .map(|r| {
            let desc: String = r.get(1);
            normalise_name(&desc)
        })
        .collect();

    let mut entries: Vec<ReceiptEntry> = Vec::new();
    let mut newly_converted: usize = 0;

    for (i, row) in rows.iter().enumerate() {
        let version: i64 = row.get(0);
        let description: String = row.get(1);
        let sqlx_checksum: Vec<u8> = row.get(2);
        let sqlx_hex = hex(&sqlx_checksum);
        let kryzhen_name = names[i].clone();

        let filename = find_file_for(&dir_files, &description, &kryzhen_name).ok_or_else(|| {
            SqlxImportError::FileMissing {
                path: migrations_dir.join(format!("<{description}>.sql")),
                name: description.clone(),
            }
        })?;

        let full_path = migrations_dir.join(&filename);
        let raw = std::fs::read(&full_path)?;
        let file_hex = hex(&Sha384::digest(&raw));

        // Only verify checksum against original (un-headered) file content.
        // If the file already has a header, we trust the receipt instead.
        let content = std::fs::read_to_string(&full_path)?;
        if !content.contains("-- #!migration") && file_hex != sqlx_hex {
            return Err(SqlxImportError::ChecksumMismatch {
                filename,
                expected: sqlx_hex,
                got: file_hex,
            });
        }

        // Rewrite file with header if not already converted.
        if !content.contains("-- #!migration") {
            let (desc_text, body) = extract_description_and_body(&content);
            let requires: Vec<String> = if i > 0 {
                vec![names[i - 1].clone()]
            } else {
                vec![]
            };
            let header = build_header(&kryzhen_name, &desc_text, &requires);
            let new_content = format!("{header}\n{body}\n");
            std::fs::write(&full_path, &new_content)?;
            newly_converted += 1;
        }

        entries.push(ReceiptEntry {
            version,
            sqlx_description: description,
            kryzhen_name,
            filename,
            sqlx_checksum_hex: sqlx_hex,
        });
    }

    let receipt = Receipt {
        migrations: entries,
        newly_converted,
        already_imported: false,
    };
    write_receipt(migrations_dir, &receipt)?;
    Ok(receipt)
}

// ---------------------------------------------------------------------------
// Phase 2: import
// ---------------------------------------------------------------------------

/// Verify `_sqlx_migrations` against the receipt, then insert rows into
/// `mallard.applied_migrations`. Already-present rows are skipped silently.
///
/// Requires a receipt written by [`convert`]. Fails if `_sqlx_migrations`
/// does not match the receipt (wrong machine, wrong DB, or DB not migrated
/// with sqlx yet).
pub async fn import(
    client: &impl GenericClient,
    migrations_dir: &Path,
) -> std::result::Result<Receipt, SqlxImportError> {
    let receipt = read_receipt(migrations_dir)?;

    // If _sqlx_migrations was already dropped by a prior import run, we're done.
    let exists: bool = client
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM pg_tables \
             WHERE schemaname = 'public' AND tablename = '_sqlx_migrations')",
            &[],
        )
        .await?
        .get(0);
    if !exists {
        return Ok(Receipt { already_imported: true, ..receipt });
    }

    // Re-verify _sqlx_migrations against receipt checksums.
    let rows = client
        .query(
            "SELECT version, description, checksum FROM _sqlx_migrations \
             WHERE success = true ORDER BY version",
            &[],
        )
        .await?;

    if rows.len() != receipt.migrations.len() {
        return Err(SqlxImportError::CountMismatch {
            db_count: rows.len(),
            receipt_count: receipt.migrations.len(),
        });
    }

    for (row, entry) in rows.iter().zip(receipt.migrations.iter()) {
        let sqlx_checksum: Vec<u8> = row.get(2);
        let db_hex = hex(&sqlx_checksum);
        if db_hex != entry.sqlx_checksum_hex {
            return Err(SqlxImportError::ReceiptDbMismatch {
                name: entry.kryzhen_name.clone(),
                receipt_hex: entry.sqlx_checksum_hex.clone(),
                db_hex,
            });
        }
    }

    // Insert into mallard.applied_migrations, skipping already-present rows.
    crate::postgres::ensure_schema(client).await?;
    let already_applied = crate::postgres::load_applied(client).await?;

    let names: Vec<String> = receipt
        .migrations
        .iter()
        .map(|e| e.kryzhen_name.clone())
        .collect();

    for (i, entry) in receipt.migrations.iter().enumerate() {
        let mname = MigrationName(entry.kryzhen_name.clone());
        if already_applied.contains_key(&mname) {
            continue;
        }

        let path = migrations_dir.join(&entry.filename);
        let content = std::fs::read_to_string(&path)?;
        let migrations = crate::parser::parse_file(&content, &entry.filename)
            .map_err(|e| SqlxImportError::Io(std::io::Error::other(e.to_string())))?;
        let body = migrations
            .into_iter()
            .find(|m| m.name.0 == entry.kryzhen_name)
            .map(|m| m.script)
            .ok_or_else(|| SqlxImportError::Io(std::io::Error::other(format!(
                "migration '{}' not found in file '{}'",
                entry.kryzhen_name, entry.filename
            ))))?;
        let checksum_bytes = checksum(&body);

        let description = if entry.sqlx_description.is_empty() {
            entry.kryzhen_name.clone()
        } else {
            entry.sqlx_description.clone()
        };

        let requires: Vec<String> = if i > 0 {
            vec![names[i - 1].clone()]
        } else {
            vec![]
        };

        client
            .execute(
                "INSERT INTO mallard.applied_migrations \
                 (name, description, requires, checksum, script_text) \
                 VALUES ($1, $2, $3, $4, $5)",
                &[
                    &entry.kryzhen_name,
                    &description,
                    &requires,
                    &checksum_bytes.as_slice(),
                    &body,
                ],
            )
            .await?;
    }

    client
        .execute("DROP TABLE IF EXISTS _sqlx_migrations", &[])
        .await?;

    Ok(receipt)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn receipt_path(dir: &Path) -> PathBuf {
    dir.join(".kryzhen-import-receipt.json")
}

fn write_receipt(dir: &Path, receipt: &Receipt) -> std::result::Result<(), SqlxImportError> {
    let json = serde_json::to_string_pretty(receipt)?;
    std::fs::write(receipt_path(dir), json)?;
    Ok(())
}

fn read_receipt(dir: &Path) -> std::result::Result<Receipt, SqlxImportError> {
    let path = receipt_path(dir);
    if !path.exists() {
        return Err(SqlxImportError::NoReceipt { path });
    }
    let json = std::fs::read_to_string(&path)?;
    Ok(serde_json::from_str(&json)?)
}


fn build_header(name: &str, description: &str, requires: &[String]) -> String {
    let mut lines = vec![
        "-- #!migration".to_string(),
        format!("-- name: \"{name}\","),
    ];
    if description.is_empty() {
        lines.push(format!("-- description: \"{name}\""));
    } else {
        lines.push(format!("-- description: \"{description}\""));
    }
    if requires.is_empty() {
        let last = lines.last_mut().unwrap();
        if !last.ends_with(';') {
            last.push(';');
        }
    } else {
        let last = lines.last_mut().unwrap();
        if !last.ends_with(',') {
            last.push(',');
        }
        if requires.len() == 1 {
            lines.push(format!("-- requires: \"{}\";", requires[0]));
        } else {
            let list = requires
                .iter()
                .map(|r| format!("\"{r}\""))
                .collect::<Vec<_>>()
                .join(", ");
            lines.push(format!("-- requires: [{list}];"));
        }
    }
    lines.join("\n")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Collect `.sql` filenames (relative) from a directory.
fn collect_sql_files(dir: &Path) -> std::result::Result<Vec<String>, SqlxImportError> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".sql") {
            files.push(name);
        }
    }
    files.sort();
    Ok(files)
}

/// Find the filename in `dir_files` that best matches the sqlx description.
fn find_file_for(dir_files: &[String], description: &str, kryzhen_name: &str) -> Option<String> {
    let file_key = |stem: &str| -> String {
        let s = strip_numeric_prefix(stem);
        s.replace(['_', '-'], " ").to_lowercase()
    };

    let desc_key = description.to_lowercase();
    let kname_key = kryzhen_name.replace('-', " ").to_lowercase();

    for f in dir_files {
        let stem = f.strip_suffix(".sql").unwrap_or(f);
        let fk = file_key(stem);
        if fk == desc_key || fk == kname_key {
            return Some(f.clone());
        }
    }

    for f in dir_files {
        let stem = f.strip_suffix(".sql").unwrap_or(f);
        let fk = file_key(stem);
        if fk.contains(&kname_key) || kname_key.contains(&fk) {
            return Some(f.clone());
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalise_name_strips_prefix_and_replaces_spaces() {
        assert_eq!(normalise_name("001_create users"), "create-users");
        assert_eq!(normalise_name("create users"), "create-users");
        assert_eq!(
            normalise_name("database search path"),
            "database-search-path"
        );
        assert_eq!(normalise_name("123_add email index"), "add-email-index");
    }

    #[test]
    fn extract_description_and_body_separates_comments() {
        let content = "-- First line\n-- Second line\nSELECT 1;\n";
        let (desc, body) = extract_description_and_body(content);
        assert_eq!(desc, "First line Second line");
        assert_eq!(body, "SELECT 1;");
    }

    #[test]
    fn extract_description_and_body_no_comments() {
        let content = "SELECT 1;\n";
        let (desc, body) = extract_description_and_body(content);
        assert_eq!(desc, "");
        assert_eq!(body, "SELECT 1;");
    }

    #[test]
    fn build_header_no_requires() {
        let h = build_header("create-users", "create users", &[]);
        assert!(h.contains("-- #!migration"));
        assert!(h.contains("name: \"create-users\""));
        assert!(h.contains("description: \"create users\";"));
        assert!(!h.contains("requires"));
    }

    #[test]
    fn build_header_with_requires() {
        let h = build_header("add-index", "add index", &["create-users".to_string()]);
        assert!(h.contains("requires: \"create-users\";"));
    }
}
