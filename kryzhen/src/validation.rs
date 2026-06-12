//! Validation checks run before applying: [`check_duplicate_names`] rejects colliding
//! migration names, and [`check_checksums`] aborts if an already-applied migration's
//! file has changed since it was applied (tamper detection).

use crate::types::{Migration, MigrationName};
use crate::{Error, Result};
use std::collections::{HashMap, HashSet};

/// Error if two migrations share a name.
pub fn check_duplicate_names(migrations: &[Migration]) -> Result<()> {
    let mut seen: HashSet<&MigrationName> = HashSet::new();
    for m in migrations {
        if !seen.insert(&m.name) {
            return Err(Error::DuplicateName(m.name.clone()));
        }
    }
    Ok(())
}

/// Error if any already-applied migration's on-disk checksum differs from the stored one.
pub fn check_checksums(
    disk: &[Migration],
    applied: &HashMap<MigrationName, [u8; 32]>,
) -> Result<()> {
    for m in disk {
        if let Some(stored) = applied.get(&m.name) {
            if *stored != m.checksum {
                return Err(Error::ChecksumMismatch(m.name.clone()));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::checksum;

    fn mig(name: &str, body: &str) -> Migration {
        Migration {
            name: MigrationName(name.into()),
            description: String::new(),
            requires: vec![],
            script: body.into(),
            checksum: checksum(body),
        }
    }

    #[test]
    fn duplicate_names_rejected() {
        let err = check_duplicate_names(&[mig("a", "x"), mig("a", "y")]).unwrap_err();
        assert!(matches!(err, Error::DuplicateName(_)));
    }

    #[test]
    fn unique_names_ok() {
        assert!(check_duplicate_names(&[mig("a", "x"), mig("b", "y")]).is_ok());
    }

    #[test]
    fn matching_checksum_ok() {
        let disk = [mig("a", "SELECT 1;")];
        let mut applied = HashMap::new();
        applied.insert(MigrationName("a".into()), checksum("SELECT 1;"));
        assert!(check_checksums(&disk, &applied).is_ok());
    }

    #[test]
    fn changed_checksum_rejected() {
        let disk = [mig("a", "SELECT 2;")];
        let mut applied = HashMap::new();
        applied.insert(MigrationName("a".into()), checksum("SELECT 1;"));
        let err = check_checksums(&disk, &applied).unwrap_err();
        assert!(matches!(err, Error::ChecksumMismatch(_)));
    }

    #[test]
    fn unapplied_migration_ignored_by_checksum_check() {
        let disk = [mig("new", "SELECT 9;")];
        let applied = HashMap::new();
        assert!(check_checksums(&disk, &applied).is_ok());
    }
}
