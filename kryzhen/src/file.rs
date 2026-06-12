//! Directory loading: walk a root for `*.sql` files, parse each, and apply the
//! implicit in-file linear dependency (each block requires its in-file predecessor).
//! See [`load_dir`] and [`apply_implicit_deps`].

use crate::parser::parse_file;
use crate::types::Migration;
use crate::Result;
use std::path::Path;

/// Apply the implicit in-file linear dependency to a file's ordered blocks:
/// each block (after the first) implicitly requires the previous block in the file,
/// merged with its explicit requires (spec §9.2). Predecessor appended if absent.
pub fn apply_implicit_deps(mut blocks: Vec<Migration>) -> Vec<Migration> {
    for i in 1..blocks.len() {
        let prev = blocks[i - 1].name.clone();
        if !blocks[i].requires.contains(&prev) {
            blocks[i].requires.push(prev);
        }
    }
    blocks
}

/// Walk `root` recursively, parse every `*.sql` file, and return all migrations
/// with implicit in-file deps applied. Files are processed in sorted path order.
pub fn load_dir(root: &Path) -> Result<Vec<Migration>> {
    use walkdir::WalkDir;

    let mut paths: Vec<_> = WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .filter(|p| p.extension().is_some_and(|x| x == "sql"))
        .collect();
    paths.sort();

    let mut all = Vec::new();
    for path in paths {
        let text = std::fs::read_to_string(&path)?;
        let label = path.display().to_string();
        let blocks = parse_file(&text, &label)?;
        all.extend(apply_implicit_deps(blocks));
    }
    Ok(all)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{checksum, MigrationName};

    fn mig(name: &str, requires: &[&str]) -> Migration {
        Migration {
            name: MigrationName(name.into()),
            description: String::new(),
            requires: requires
                .iter()
                .map(|s| MigrationName(s.to_string()))
                .collect(),
            script: String::new(),
            checksum: checksum(""),
        }
    }

    #[test]
    fn first_block_unchanged_others_require_predecessor() {
        let out = apply_implicit_deps(vec![mig("a", &[]), mig("b", &[]), mig("c", &[])]);
        assert!(out[0].requires.is_empty());
        assert_eq!(out[1].requires, vec![MigrationName("a".into())]);
        assert_eq!(out[2].requires, vec![MigrationName("b".into())]);
    }

    #[test]
    fn implicit_merges_with_explicit_without_duplication() {
        let out = apply_implicit_deps(vec![mig("a", &[]), mig("b", &["x"])]);
        assert_eq!(
            out[1].requires,
            vec![MigrationName("x".into()), MigrationName("a".into())]
        );
    }

    #[test]
    fn implicit_not_duplicated_if_already_explicit() {
        let out = apply_implicit_deps(vec![mig("a", &[]), mig("b", &["a"])]);
        assert_eq!(out[1].requires, vec![MigrationName("a".into())]);
    }

    #[test]
    fn load_dir_reads_sql_and_applies_implicit_deps() {
        let dir = std::env::temp_dir().join(format!("kryzhen-load-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("two.sql"),
            "-- #!migration\n-- name: \"a\",\n-- description: \"x\";\nSELECT 1;\n\
             -- #!migration\n-- name: \"b\",\n-- description: \"y\";\nSELECT 2;\n",
        )
        .unwrap();

        let migs = load_dir(&dir).unwrap();
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(migs.len(), 2);
        assert_eq!(migs[1].requires, vec![MigrationName("a".into())]);
    }
}
