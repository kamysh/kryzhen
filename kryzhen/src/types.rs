//! Core data types: [`MigrationName`], [`Migration`], the library [`Error`] enum, and
//! the [`checksum`] function (SHA-256 of a migration's whitespace-trimmed SQL body).

use sha2::{Digest, Sha256};
use std::fmt;

/// A migration's unique name, e.g. `"tables/phone"`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MigrationName(pub String);

impl fmt::Display for MigrationName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A single migration block parsed from a `.sql` file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Migration {
    pub name: MigrationName,
    pub description: String,
    /// Explicit `requires` merged with the implicit in-file predecessor (spec §9.2).
    pub requires: Vec<MigrationName>,
    /// SQL body with leading/trailing whitespace trimmed (spec §6).
    pub script: String,
    /// SHA-256 of `script`. 32 raw bytes.
    pub checksum: [u8; 32],
}

/// Library error type.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("parse error in {file}: {message}")]
    Parse { file: String, message: String },
    #[error("duplicate migration name: {0}")]
    DuplicateName(MigrationName),
    #[error("migration {migration} requires {missing}, which does not exist")]
    DanglingDependency {
        migration: MigrationName,
        missing: MigrationName,
    },
    #[error("dependency cycle detected involving: {0:?}")]
    Cycle(Vec<MigrationName>),
    #[error("checksum mismatch for already-applied migration {0}: file content changed")]
    ChecksumMismatch(MigrationName),
    #[error("migration {name} has a {len}-byte checksum in the database; expected 32")]
    CorruptChecksum { name: MigrationName, len: usize },
    #[error("database error: {0}")]
    Db(#[from] tokio_postgres::Error),
}

/// Compute the kryzhen/mallard checksum of a migration body:
/// SHA-256 over the body with leading/trailing whitespace trimmed (spec §6).
pub fn checksum(body: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(body.trim().as_bytes());
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_name_displays_inner_string() {
        assert_eq!(
            MigrationName("tables/phone".into()).to_string(),
            "tables/phone"
        );
    }

    #[test]
    fn error_messages_render() {
        let e = Error::DuplicateName(MigrationName("a".into()));
        assert_eq!(e.to_string(), "duplicate migration name: a");
    }

    #[test]
    fn checksum_trims_surrounding_whitespace() {
        assert_eq!(checksum("  SELECT 1;  "), checksum("SELECT 1;"));
        assert_eq!(checksum("\n\tSELECT 1;\n"), checksum("SELECT 1;"));
    }

    #[test]
    fn checksum_is_plain_sha256_of_trimmed_body() {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(b"SELECT 1;"); // exact trimmed bytes, no hidden transformation
        let expected: [u8; 32] = h.finalize().into();
        assert_eq!(checksum("  SELECT 1;  "), expected);
    }
}
