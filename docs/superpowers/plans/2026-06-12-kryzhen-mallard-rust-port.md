# kryzhen (mallard Rust port) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Port the Haskell `mallard` migration tool to Rust as a `kryzhen` library crate plus a `kryzhen` CLI binary: forward-only, dependency-resolved SQL migrations for PostgreSQL, compatible with mallard's on-disk format and `mallard.applied_migrations` tracking table.

**Architecture:** Cargo workspace with a `kryzhen` library (types → parser → file → graph → validation → postgres → public `migrate()`) and a thin `kryzhen-cli` binary (clap → library). Bottom-up TDD: each pure module is built and unit-tested before the postgres applier; the applier and end-to-end flow are covered by integration tests against a real PostgreSQL.

**Tech Stack:** Rust 2021, tokio + tokio-postgres (async), sha2 (SHA-256), walkdir (directory traversal), clap (CLI), thiserror (library errors), anyhow + tracing (CLI), testcontainers + tokio-postgres (integration tests).

**Spec:** `docs/superpowers/specs/2026-06-12-kryzhen-mallard-rust-port-design.md`

---

## File structure

Workspace root `kryzhen/`:

```
Cargo.toml                      # workspace manifest [members: kryzhen, kryzhen-cli]
kryzhen/
  Cargo.toml                    # library crate
  src/
    lib.rs                      # public API: migrate(), Config, Report; module decls + re-exports
    types.rs                    # MigrationName, Migration, Error, checksum()
    parser.rs                   # parse one file's #!migration blocks
    file.rs                     # walk root dir, read .sql, apply implicit in-file linear deps
    graph.rs                    # build dep graph, cycle check, topological sort
    validation.rs               # duplicate names, checksum vs DB
    postgres.rs                 # ensure schema/table, load applied set, apply pending
  tests/
    applier.rs                  # integration tests against real PostgreSQL
kryzhen-cli/
  Cargo.toml                    # binary crate, produces `kryzhen`
  src/
    main.rs                     # clap args -> kryzhen::migrate()
```

Responsibilities are one-per-file as in the spec §3. Pure modules (`types`, `parser`, `file`, `graph`, `validation`) have no DB dependency and are unit-tested in-file with `#[cfg(test)]`. `postgres` is exercised by `kryzhen/tests/applier.rs`.

---

## Conventions for every task

- Run all cargo commands from the workspace root `kryzhen/`.
- After code changes: `cargo fmt --all` then `cargo clippy --workspace --all-targets -- -D warnings` must exit 0 before each commit (match rustfmt default formatting).
- Commit messages end with the trailer:
  ```
  Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
  ```
- TDD: write the failing test, watch it fail, implement minimally, watch it pass, commit.

---

## Task 1: Workspace scaffold

**Files:**
- Create: `Cargo.toml` (workspace)
- Create: `kryzhen/Cargo.toml`
- Create: `kryzhen/src/lib.rs`
- Create: `kryzhen-cli/Cargo.toml`
- Create: `kryzhen-cli/src/main.rs`

- [ ] **Step 1: Create the workspace manifest**

`Cargo.toml`:

```toml
[workspace]
members = ["kryzhen", "kryzhen-cli"]
resolver = "2"

[workspace.package]
version = "0.1.0"
edition = "2021"
license = "BSD-3-Clause"

[workspace.dependencies]
thiserror = "1"
sha2 = "0.10"
walkdir = "2"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
tokio-postgres = "0.7"
clap = { version = "4", features = ["derive"] }
anyhow = "1"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
```

- [ ] **Step 2: Create the library crate manifest**

`kryzhen/Cargo.toml`:

```toml
[package]
name = "kryzhen"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
thiserror.workspace = true
sha2.workspace = true
walkdir.workspace = true
tokio.workspace = true
tokio-postgres.workspace = true

[dev-dependencies]
tokio = { workspace = true, features = ["macros", "rt-multi-thread"] }
```

- [ ] **Step 3: Create a placeholder lib.rs**

`kryzhen/src/lib.rs`:

```rust
//! kryzhen — forward-only, dependency-resolved SQL migrations for PostgreSQL.
//! Compatible with the Haskell `mallard` tool's file format and tracking table.
```

- [ ] **Step 4: Create the CLI crate manifest**

`kryzhen-cli/Cargo.toml`:

```toml
[package]
name = "kryzhen-cli"
version.workspace = true
edition.workspace = true
license.workspace = true

[[bin]]
name = "kryzhen"
path = "src/main.rs"

[dependencies]
kryzhen = { path = "../kryzhen" }
clap.workspace = true
tokio.workspace = true
anyhow.workspace = true
tracing.workspace = true
tracing-subscriber.workspace = true
```

- [ ] **Step 5: Create a placeholder main.rs**

`kryzhen-cli/src/main.rs`:

```rust
fn main() {
    println!("kryzhen");
}
```

- [ ] **Step 6: Build the workspace**

Run: `cargo build --workspace`
Expected: compiles successfully (warnings about unused are fine at this stage).

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock kryzhen/ kryzhen-cli/
git commit -m "Scaffold kryzhen workspace (library + CLI)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: Core types and error enum

**Files:**
- Create: `kryzhen/src/types.rs`
- Modify: `kryzhen/src/lib.rs`

- [ ] **Step 1: Write the failing test**

Add `kryzhen/src/types.rs`:

```rust
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
    #[error("database error: {0}")]
    Db(#[from] tokio_postgres::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_name_displays_inner_string() {
        assert_eq!(MigrationName("tables/phone".into()).to_string(), "tables/phone");
    }

    #[test]
    fn error_messages_render() {
        let e = Error::DuplicateName(MigrationName("a".into()));
        assert_eq!(e.to_string(), "duplicate migration name: a");
    }
}
```

- [ ] **Step 2: Wire the module into lib.rs**

Add to `kryzhen/src/lib.rs`:

```rust
pub mod types;

pub use types::{Error, Migration, MigrationName};

/// Library result type.
pub type Result<T> = std::result::Result<T, Error>;
```

- [ ] **Step 3: Run the tests**

Run: `cargo test -p kryzhen types::`
Expected: 2 tests pass.

- [ ] **Step 4: Commit**

```bash
git add kryzhen/src/types.rs kryzhen/src/lib.rs
git commit -m "Add core types and error enum

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: Checksum helper

**Files:**
- Modify: `kryzhen/src/types.rs`
- Modify: `kryzhen/src/lib.rs`

- [ ] **Step 1: Add the checksum function**

In `kryzhen/src/types.rs`, add this `use` at the top of the file (with the other imports)
and the function above the `tests` module:

```rust
use sha2::{Digest, Sha256};

/// Compute the kryzhen/mallard checksum of a migration body:
/// SHA-256 over the body with leading/trailing whitespace trimmed (spec §6).
pub fn checksum(body: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(body.trim().as_bytes());
    hasher.finalize().into()
}
```

- [ ] **Step 2: Add the failing tests**

Append inside the existing `tests` module in `kryzhen/src/types.rs`:

```rust
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
```

> These prove two things: trimming is applied (whitespace-insensitive), and the result
> is exactly `SHA-256(trimmed_body)` with nothing else done to the bytes.

- [ ] **Step 3: Run the tests to verify they pass**

Run: `cargo test -p kryzhen types::`
Expected: 4 tests pass (2 prior + 2 new).

- [ ] **Step 4: Re-export the helper**

Add to `kryzhen/src/lib.rs`:

```rust
pub use types::checksum;
```

- [ ] **Step 5: Commit**

```bash
git add kryzhen/src/types.rs kryzhen/src/lib.rs
git commit -m "Add SHA-256 checksum over trimmed migration body

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: Parser — single migration block

**Files:**
- Create: `kryzhen/src/parser.rs`
- Modify: `kryzhen/src/lib.rs`

The parser takes the full text of one `.sql` file and returns the migration blocks in
file order. Header lines are SQL line comments (`--`); the directive is `#!migration`.
Each header field ends with `,`, the last with `;`. Body is everything after the header
`;` up to the next `#!` or EOF. `#!test` is rejected (spec §9.1).

This task handles the parsing machinery (which already supports multiple blocks via
`split_blocks`); Task 5 adds a multi-block test.

- [ ] **Step 1: Write the failing test + skeleton**

Create `kryzhen/src/parser.rs`:

```rust
use crate::types::{checksum, Migration, MigrationName};
use crate::{Error, Result};

/// Parse all `#!migration` blocks from one file's text, in file order.
/// `file_label` is used in error messages.
pub fn parse_file(text: &str, file_label: &str) -> Result<Vec<Migration>> {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ONE: &str = r#"
-- #!migration
-- name: "tables/phone",
-- description: "Phone numbers attached to a person.",
-- requires: ["tables/person"];
CREATE TABLE phone (id bigint);
"#;

    #[test]
    fn parses_single_block() {
        let m = parse_file(ONE, "phone.sql").unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].name, MigrationName("tables/phone".into()));
        assert_eq!(m[0].description, "Phone numbers attached to a person.");
        assert_eq!(m[0].requires, vec![MigrationName("tables/person".into())]);
        assert_eq!(m[0].script, "CREATE TABLE phone (id bigint);");
        assert_eq!(m[0].checksum, checksum("CREATE TABLE phone (id bigint);"));
    }

    #[test]
    fn requires_optional() {
        let text = r#"
-- #!migration
-- name: "a",
-- description: "first";
SELECT 1;
"#;
        let m = parse_file(text, "a.sql").unwrap();
        assert!(m[0].requires.is_empty());
    }

    #[test]
    fn requires_can_be_single_string() {
        let text = r#"
-- #!migration
-- name: "b",
-- description: "second",
-- requires: "a";
SELECT 2;
"#;
        let m = parse_file(text, "b.sql").unwrap();
        assert_eq!(m[0].requires, vec![MigrationName("a".into())]);
    }

    #[test]
    fn rejects_test_directive() {
        let text = r#"
-- #!test
-- name: "t",
-- description: "a test";
SELECT 1;
"#;
        let err = parse_file(text, "t.sql").unwrap_err();
        assert!(matches!(err, Error::Parse { .. }));
    }

    #[test]
    fn missing_name_is_error() {
        let text = r#"
-- #!migration
-- description: "no name";
SELECT 1;
"#;
        assert!(matches!(parse_file(text, "x.sql").unwrap_err(), Error::Parse { .. }));
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p kryzhen parser::`
Expected: FAIL — `parse_file` is `todo!()` (panics).

- [ ] **Step 3: Implement the parser**

Replace the `parse_file` stub in `kryzhen/src/parser.rs` with the full implementation
below (hand-written, no parser-combinator dependency):

```rust
pub fn parse_file(text: &str, file_label: &str) -> Result<Vec<Migration>> {
    let err = |message: String| Error::Parse {
        file: file_label.to_string(),
        message,
    };

    let mut out = Vec::new();
    for block in split_blocks(text) {
        out.push(parse_block(&block, &err)?);
    }
    Ok(out)
}

/// A raw block: the directive keyword and the remaining text (header lines + body).
struct RawBlock {
    directive: String,
    rest: String,
}

/// Split file text on `#!`. Text before the first `#!` is ignored. Each `#!` starts a
/// new block; the word immediately after `#!` is the directive. The block's text runs
/// up to the next `#!` or EOF.
fn split_blocks(text: &str) -> Vec<RawBlock> {
    let mut blocks = Vec::new();
    let mut search = text;
    while let Some(idx) = search.find("#!") {
        let after = &search[idx + 2..];
        let dir_end = after.find(char::is_whitespace).unwrap_or(after.len());
        let directive = after[..dir_end].to_string();
        let rest_start = &after[dir_end..];
        let next = rest_start.find("#!").unwrap_or(rest_start.len());
        blocks.push(RawBlock {
            directive,
            rest: rest_start[..next].to_string(),
        });
        search = &rest_start[next..];
    }
    blocks
}

fn parse_block(block: &RawBlock, err: &impl Fn(String) -> Error) -> Result<Migration> {
    if block.directive != "migration" {
        return Err(err(format!(
            "unsupported directive `#!{}` (only `#!migration` is supported)",
            block.directive
        )));
    }

    // Header field list ends at the first `;`.
    let semi = block
        .rest
        .find(';')
        .ok_or_else(|| err("header is missing its terminating `;`".into()))?;
    let header_region = &block.rest[..semi];
    let body = block.rest[semi + 1..].trim();

    // Strip leading `--` from each comment line and join into one field string.
    let header_text: String = header_region
        .lines()
        .map(|l| l.trim_start().trim_start_matches("--").trim())
        .collect::<Vec<_>>()
        .join(" ");

    let fields = parse_fields(&header_text, err)?;

    let name = fields
        .iter()
        .find(|(k, _)| k == "name")
        .and_then(|(_, v)| v.as_text())
        .ok_or_else(|| err("missing or non-string `name` field".into()))?;
    let description = fields
        .iter()
        .find(|(k, _)| k == "description")
        .and_then(|(_, v)| v.as_text())
        .ok_or_else(|| err("missing or non-string `description` field".into()))?;
    let requires = match fields.iter().find(|(k, _)| k == "requires") {
        None => Vec::new(),
        Some((_, FieldValue::Text(s))) => vec![MigrationName(s.clone())],
        Some((_, FieldValue::List(xs))) => xs.iter().cloned().map(MigrationName).collect(),
    };

    Ok(Migration {
        name: MigrationName(name),
        description,
        requires,
        checksum: checksum(body),
        script: body.to_string(),
    })
}

enum FieldValue {
    Text(String),
    List(Vec<String>),
}

impl FieldValue {
    fn as_text(&self) -> Option<String> {
        match self {
            FieldValue::Text(s) => Some(s.clone()),
            FieldValue::List(_) => None,
        }
    }
}

/// Parse `key: value, key: value` where value is `"..."` or `["a", "b"]`.
fn parse_fields(text: &str, err: &impl Fn(String) -> Error) -> Result<Vec<(String, FieldValue)>> {
    let mut fields = Vec::new();
    for raw in split_top_level_commas(text) {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let colon = raw
            .find(':')
            .ok_or_else(|| err(format!("malformed header field: `{raw}`")))?;
        let key = raw[..colon].trim().to_string();
        let value = raw[colon + 1..].trim();
        let parsed = if value.starts_with('[') {
            FieldValue::List(parse_string_list(value, err)?)
        } else {
            FieldValue::Text(parse_quoted(value, err)?)
        };
        fields.push((key, parsed));
    }
    Ok(fields)
}

/// Split on commas that are not inside `[...]` or `"..."`.
fn split_top_level_commas(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    let mut depth = 0i32;
    for c in text.chars() {
        match c {
            '"' => {
                in_quote = !in_quote;
                cur.push(c);
            }
            '[' if !in_quote => {
                depth += 1;
                cur.push(c);
            }
            ']' if !in_quote => {
                depth -= 1;
                cur.push(c);
            }
            ',' if !in_quote && depth == 0 => {
                parts.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
    }
    parts.push(cur);
    parts
}

/// Strip surrounding double quotes; error if absent.
fn parse_quoted(value: &str, err: &impl Fn(String) -> Error) -> Result<String> {
    let v = value.trim();
    let inner = v
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .ok_or_else(|| err(format!("expected a quoted string, got `{v}`")))?;
    Ok(inner.to_string())
}

/// Parse `["a", "b"]` into a vec of strings.
fn parse_string_list(value: &str, err: &impl Fn(String) -> Error) -> Result<Vec<String>> {
    let v = value.trim();
    let inner = v
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .ok_or_else(|| err(format!("expected a list `[...]`, got `{v}`")))?;
    let mut out = Vec::new();
    for item in split_top_level_commas(inner) {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        out.push(parse_quoted(item, err)?);
    }
    Ok(out)
}
```

- [ ] **Step 4: Wire the module into lib.rs**

Add to `kryzhen/src/lib.rs`:

```rust
pub mod parser;
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p kryzhen parser::`
Expected: all 5 parser tests pass.

- [ ] **Step 6: fmt + clippy + commit**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings
git add kryzhen/src/parser.rs kryzhen/src/lib.rs
git commit -m "Add migration file parser (rejects #!test)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: Parser — multiple blocks per file

**Files:**
- Modify: `kryzhen/src/parser.rs` (test only)

- [ ] **Step 1: Write the test**

Add to the `tests` module in `kryzhen/src/parser.rs`:

```rust
    const MULTI: &str = r#"
-- #!migration
-- name: "a",
-- description: "first";
CREATE TABLE a ();
-- #!migration
-- name: "b",
-- description: "second";
CREATE TABLE b ();
"#;

    #[test]
    fn parses_multiple_blocks_in_file_order() {
        let m = parse_file(MULTI, "multi.sql").unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].name, MigrationName("a".into()));
        assert_eq!(m[1].name, MigrationName("b".into()));
        assert_eq!(m[0].script, "CREATE TABLE a ();");
        assert_eq!(m[1].script, "CREATE TABLE b ();");
    }
```

- [ ] **Step 2: Run the test**

Run: `cargo test -p kryzhen parser::parses_multiple_blocks_in_file_order`
Expected: PASS (Task 4's `split_blocks` already splits on each `#!`).

- [ ] **Step 3: Commit**

```bash
git add kryzhen/src/parser.rs
git commit -m "Test multiple migration blocks per file

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 6: File walking + implicit in-file linear dependencies

**Files:**
- Create: `kryzhen/src/file.rs`
- Modify: `kryzhen/src/lib.rs`

`load_dir(root)` walks `root` recursively, reads every `*.sql` file (sorted by path for
determinism), parses each into its ordered blocks, and applies the implicit in-file
linear dependency (spec §9.2): within a single file, block N (N>0) gains its
predecessor's name in `requires` (appended if not already present).

- [ ] **Step 1: Write the failing test + implementation of `apply_implicit_deps`**

Create `kryzhen/src/file.rs`:

```rust
use crate::parser::parse_file;
use crate::types::{Migration, MigrationName};
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
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::checksum;

    fn mig(name: &str, requires: &[&str]) -> Migration {
        Migration {
            name: MigrationName(name.into()),
            description: String::new(),
            requires: requires.iter().map(|s| MigrationName(s.to_string())).collect(),
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
}
```

- [ ] **Step 2: Run the unit tests for implicit deps**

Run: `cargo test -p kryzhen file::tests`
Expected: 3 tests pass (`apply_implicit_deps` implemented; `load_dir` is `todo!()` but
unused by these tests).

- [ ] **Step 3: Implement `load_dir`**

Replace the `load_dir` stub:

```rust
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
```

- [ ] **Step 4: Add a temp-directory test for `load_dir`**

Add to the `tests` module in `file.rs`:

```rust
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
```

- [ ] **Step 5: Wire the module + run all file tests**

Add to `kryzhen/src/lib.rs`:

```rust
pub mod file;
```

Run: `cargo test -p kryzhen file::`
Expected: 4 tests pass.

- [ ] **Step 6: fmt + clippy + commit**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings
git add kryzhen/src/file.rs kryzhen/src/lib.rs
git commit -m "Add directory loading with implicit in-file linear deps

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 7: Dependency graph — topological sort + cycle detection

**Files:**
- Create: `kryzhen/src/graph.rs`
- Modify: `kryzhen/src/lib.rs`

`topo_sort(migrations)` returns the migrations ordered so each appears after all of its
`requires`. It errors on cycles (`Error::Cycle`) and on a `requires` referencing a name
not present (`Error::DanglingDependency`), so it is safe to call standalone. Order is
deterministic (lexicographic among ready nodes).

- [ ] **Step 1: Write the failing test + skeleton**

Create `kryzhen/src/graph.rs`:

```rust
use crate::types::{Migration, MigrationName};
use crate::{Error, Result};
use std::collections::{HashMap, HashSet};

/// Order migrations so each appears after all its `requires`.
/// Errors on cycles and on requires that reference an unknown migration.
pub fn topo_sort(migrations: Vec<Migration>) -> Result<Vec<Migration>> {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::checksum;

    fn mig(name: &str, requires: &[&str]) -> Migration {
        Migration {
            name: MigrationName(name.into()),
            description: String::new(),
            requires: requires.iter().map(|s| MigrationName(s.to_string())).collect(),
            script: String::new(),
            checksum: checksum(""),
        }
    }

    fn names(ms: &[Migration]) -> Vec<String> {
        ms.iter().map(|m| m.name.0.clone()).collect()
    }

    #[test]
    fn orders_dependencies_before_dependents() {
        let out = topo_sort(vec![mig("b", &["a"]), mig("a", &[])]).unwrap();
        assert_eq!(names(&out), vec!["a", "b"]);
    }

    #[test]
    fn diamond_orders_root_first_and_sink_last() {
        let out = topo_sort(vec![
            mig("d", &["b", "c"]),
            mig("b", &["a"]),
            mig("c", &["a"]),
            mig("a", &[]),
        ])
        .unwrap();
        let n = names(&out);
        let pos = |x: &str| n.iter().position(|y| y == x).unwrap();
        assert!(pos("a") < pos("b") && pos("a") < pos("c"));
        assert!(pos("b") < pos("d") && pos("c") < pos("d"));
    }

    #[test]
    fn detects_cycle() {
        let err = topo_sort(vec![mig("a", &["b"]), mig("b", &["a"])]).unwrap_err();
        assert!(matches!(err, Error::Cycle(_)));
    }

    #[test]
    fn dangling_dependency_errors() {
        let err = topo_sort(vec![mig("a", &["missing"])]).unwrap_err();
        assert!(matches!(err, Error::DanglingDependency { .. }));
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p kryzhen graph::`
Expected: FAIL — `topo_sort` is `todo!()`.

- [ ] **Step 3: Implement Kahn's algorithm with deterministic ordering**

Replace the `topo_sort` stub:

```rust
pub fn topo_sort(migrations: Vec<Migration>) -> Result<Vec<Migration>> {
    let mut by_name: HashMap<MigrationName, Migration> = HashMap::new();
    for m in migrations {
        by_name.insert(m.name.clone(), m);
    }

    // Validate all requires resolve.
    for m in by_name.values() {
        for req in &m.requires {
            if !by_name.contains_key(req) {
                return Err(Error::DanglingDependency {
                    migration: m.name.clone(),
                    missing: req.clone(),
                });
            }
        }
    }

    // in-degree = number of unmet requires; dependents = reverse edges.
    let mut indegree: HashMap<MigrationName, usize> = HashMap::new();
    let mut dependents: HashMap<MigrationName, Vec<MigrationName>> = HashMap::new();
    for m in by_name.values() {
        indegree.entry(m.name.clone()).or_insert(0);
        for req in &m.requires {
            *indegree.entry(m.name.clone()).or_insert(0) += 1;
            dependents.entry(req.clone()).or_default().push(m.name.clone());
        }
    }

    // ready = zero-indegree nodes; pop the lexicographically smallest each step.
    let mut ready: Vec<MigrationName> = indegree
        .iter()
        .filter(|(_, &d)| d == 0)
        .map(|(n, _)| n.clone())
        .collect();
    ready.sort();

    let mut order: Vec<Migration> = Vec::new();
    let mut emitted: HashSet<MigrationName> = HashSet::new();

    while let Some(name) = ready.pop() {
        if !emitted.insert(name.clone()) {
            continue;
        }
        order.push(by_name.get(&name).unwrap().clone());
        if let Some(deps) = dependents.get(&name) {
            for d in deps {
                let e = indegree.get_mut(d).unwrap();
                *e -= 1;
                if *e == 0 {
                    ready.push(d.clone());
                }
            }
        }
        ready.sort();
    }

    if order.len() != by_name.len() {
        let remaining: Vec<MigrationName> = by_name
            .keys()
            .filter(|n| !emitted.contains(*n))
            .cloned()
            .collect();
        return Err(Error::Cycle(remaining));
    }

    Ok(order)
}
```

> `ready.sort()` then `ready.pop()` yields the lexicographically smallest ready node each
> iteration → deterministic output. Migration counts are small; clarity over speed.

- [ ] **Step 4: Wire the module + run tests**

Add to `kryzhen/src/lib.rs`:

```rust
pub mod graph;
```

Run: `cargo test -p kryzhen graph::`
Expected: 4 tests pass.

- [ ] **Step 5: fmt + clippy + commit**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings
git add kryzhen/src/graph.rs kryzhen/src/lib.rs
git commit -m "Add dependency graph: topo sort + cycle detection

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 8: Validation

**Files:**
- Create: `kryzhen/src/validation.rs`
- Modify: `kryzhen/src/lib.rs`

Two pure checks. Duplicate-name detection must run before the graph (duplicate names
would silently collapse in the graph's `HashMap`). Checksum verification compares
on-disk checksums to the stored ones for already-applied migrations.

- [ ] **Step 1: Write the validation module + tests**

Create `kryzhen/src/validation.rs`:

```rust
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
```

- [ ] **Step 2: Wire the module + run tests**

Add to `kryzhen/src/lib.rs`:

```rust
pub mod validation;
```

Run: `cargo test -p kryzhen validation::`
Expected: 5 tests pass.

- [ ] **Step 3: fmt + clippy + commit**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings
git add kryzhen/src/validation.rs kryzhen/src/lib.rs
git commit -m "Add validation: duplicate names + checksum tamper detection

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 9: Postgres applier

**Files:**
- Create: `kryzhen/src/postgres.rs`
- Create: `kryzhen/tests/applier.rs`
- Modify: `kryzhen/src/lib.rs`
- Modify: `kryzhen/Cargo.toml` (add testcontainers dev-deps)

The applier ensures the mallard schema/table, loads the applied set, and applies one
migration atomically (run SQL + INSERT in a single transaction).

- [ ] **Step 1: Add the testcontainers dev-dependencies**

In `kryzhen/Cargo.toml`, under `[dev-dependencies]`, add:

```toml
testcontainers = "0.23"
testcontainers-modules = { version = "0.11", features = ["postgres"] }
```

- [ ] **Step 2: Implement the applier**

Create `kryzhen/src/postgres.rs`:

```rust
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
        let mut cs = [0u8; 32];
        let n = bytes.len().min(32);
        cs[..n].copy_from_slice(&bytes[..n]);
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
```

- [ ] **Step 3: Wire the module into lib.rs**

Add to `kryzhen/src/lib.rs`:

```rust
pub mod postgres;
```

- [ ] **Step 4: Write the integration test**

Create `kryzhen/tests/applier.rs`:

```rust
use kryzhen::postgres::{apply_one, ensure_schema, load_applied};
use kryzhen::types::{checksum, Migration, MigrationName};
use testcontainers::runners::AsyncRunner;
use testcontainers::ContainerAsync;
use testcontainers_modules::postgres::Postgres;
use tokio_postgres::{Client, NoTls};

async fn connect() -> (Client, ContainerAsync<Postgres>) {
    let node = Postgres::default().start().await.unwrap();
    let port = node.get_host_port_ipv4(5432).await.unwrap();
    let conn_str =
        format!("host=127.0.0.1 port={port} user=postgres password=postgres dbname=postgres");
    let (client, connection) = tokio_postgres::connect(&conn_str, NoTls).await.unwrap();
    tokio::spawn(async move {
        let _ = connection.await;
    });
    (client, node)
}

fn mig(name: &str, body: &str) -> Migration {
    Migration {
        name: MigrationName(name.into()),
        description: format!("desc of {name}"),
        requires: vec![],
        script: body.into(),
        checksum: checksum(body),
    }
}

#[tokio::test]
async fn ensure_schema_is_idempotent() {
    let (client, _node) = connect().await;
    ensure_schema(&client).await.unwrap();
    ensure_schema(&client).await.unwrap();
    let applied = load_applied(&client).await.unwrap();
    assert!(applied.is_empty());
}

#[tokio::test]
async fn apply_one_runs_sql_and_records_with_checksum() {
    let (mut client, _node) = connect().await;
    ensure_schema(&client).await.unwrap();

    let m = mig("create_t", "CREATE TABLE t (id int);");
    apply_one(&mut client, &m).await.unwrap();

    let rows = client.query("SELECT count(*) FROM t", &[]).await.unwrap();
    let count: i64 = rows[0].get(0);
    assert_eq!(count, 0);

    let applied = load_applied(&client).await.unwrap();
    assert_eq!(
        applied.get(&MigrationName("create_t".into())),
        Some(&checksum("CREATE TABLE t (id int);"))
    );
}
```

- [ ] **Step 5: Run the integration tests**

Run: `cargo test -p kryzhen --test applier`
Expected: 2 tests pass. Requires Docker for testcontainers. If Docker is unavailable in
the environment, note it and run later — do NOT delete the tests.

- [ ] **Step 6: fmt + clippy + commit**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings
git add kryzhen/src/postgres.rs kryzhen/tests/applier.rs kryzhen/src/lib.rs kryzhen/Cargo.toml Cargo.lock
git commit -m "Add postgres applier + integration tests (mallard-compatible table)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 10: Public `migrate()` entry point

**Files:**
- Modify: `kryzhen/src/lib.rs`
- Modify: `kryzhen/tests/applier.rs` (add end-to-end test)

`migrate(config)` ties everything together: load → dedup check → topo sort → connect →
ensure schema → load applied → checksum check → apply each pending migration in order.

- [ ] **Step 1: Implement `Config`, `Report`, and `migrate` in lib.rs**

Add to `kryzhen/src/lib.rs`:

```rust
use std::collections::HashMap;
use std::path::PathBuf;

/// Connection + run configuration.
#[derive(Clone, Debug)]
pub struct Config {
    pub root: PathBuf,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub database: String,
    pub dry_run: bool,
}

/// Summary of a migration run.
#[derive(Clone, Debug, Default)]
pub struct Report {
    /// Names applied this run (or, in dry-run, that would be applied), in order.
    pub applied: Vec<String>,
    /// Names already present before this run.
    pub already_applied: Vec<String>,
}

/// Run all pending migrations under `config.root` against the configured database.
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
```

- [ ] **Step 2: Write the failing end-to-end test**

Add to `kryzhen/tests/applier.rs`:

```rust
use kryzhen::{migrate, Config};

#[tokio::test]
async fn migrate_end_to_end_applies_in_dependency_order_and_is_idempotent() {
    let (_client, node) = connect().await;
    let port = node.get_host_port_ipv4(5432).await.unwrap();

    let dir = std::env::temp_dir().join(format!("kryzhen-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("a.sql"),
        "-- #!migration\n-- name: \"a\",\n-- description: \"a\";\nCREATE TABLE a (id int);\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("b.sql"),
        "-- #!migration\n-- name: \"b\",\n-- description: \"b\",\n-- requires: \"a\";\nCREATE TABLE b (id int);\n",
    )
    .unwrap();

    let config = Config {
        root: dir.clone(),
        host: "127.0.0.1".into(),
        port,
        user: "postgres".into(),
        password: "postgres".into(),
        database: "postgres".into(),
        dry_run: false,
    };

    let report = migrate(config.clone()).await.unwrap();
    assert_eq!(report.applied, vec!["a".to_string(), "b".to_string()]);

    let report2 = migrate(config).await.unwrap();
    assert!(report2.applied.is_empty());

    std::fs::remove_dir_all(&dir).ok();
}
```

- [ ] **Step 3: Run the end-to-end test**

Run: `cargo test -p kryzhen --test applier migrate_end_to_end`
Expected: PASS (requires Docker). Verifies dependency order and idempotent re-run.

- [ ] **Step 4: fmt + clippy + commit**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings
git add kryzhen/src/lib.rs kryzhen/tests/applier.rs
git commit -m "Add public migrate() entry point + end-to-end test

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 11: CLI

**Files:**
- Modify: `kryzhen-cli/src/main.rs`

CLI flags mirror mallard (spec §10) plus `--dry-run`/`--verbose`. `-t/--test` is omitted.

- [ ] **Step 1: Implement the CLI**

Replace `kryzhen-cli/src/main.rs`:

```rust
use clap::Parser;
use kryzhen::{migrate, Config};
use std::path::PathBuf;

/// Forward-only, dependency-resolved SQL migrations for PostgreSQL (mallard-compatible).
#[derive(Parser, Debug)]
#[command(name = "kryzhen", version)]
struct Args {
    /// Root directory of the migration tree.
    #[arg(default_value = ".")]
    root: PathBuf,

    /// Database name.
    #[arg(long)]
    database: String,

    /// Server host.
    #[arg(long, default_value = "127.0.0.1")]
    host: String,

    /// Server port.
    #[arg(long, default_value_t = 5432)]
    port: u16,

    /// Username.
    #[arg(long, default_value = "postgres")]
    user: String,

    /// Password.
    #[arg(long, default_value = "")]
    password: String,

    /// Print the planned migration order; apply nothing.
    #[arg(long)]
    dry_run: bool,

    /// Verbose logging.
    #[arg(short, long)]
    verbose: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let level = if args.verbose { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| level.into()),
        )
        .init();

    let config = Config {
        root: args.root,
        host: args.host,
        port: args.port,
        user: args.user,
        password: args.password,
        database: args.database,
        dry_run: args.dry_run,
    };

    let report = migrate(config).await?;

    if args.dry_run {
        if report.applied.is_empty() {
            println!("Nothing to apply.");
        } else {
            println!("Would apply (in order):");
            for name in &report.applied {
                println!("  {name}");
            }
        }
    } else if report.applied.is_empty() {
        println!("Already up to date ({} applied).", report.already_applied.len());
    } else {
        println!("Applied {} migration(s):", report.applied.len());
        for name in &report.applied {
            println!("  {name}");
        }
    }

    Ok(())
}
```

- [ ] **Step 2: Verify it builds and shows help**

Run: `cargo run -p kryzhen-cli -- --help`
Expected: usage text listing `--database`, `--host`, `--port`, `--user`, `--password`,
`--dry-run`, `--verbose`, and the `root` positional. No `-t/--test`.

- [ ] **Step 3: fmt + clippy + commit**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings
git add kryzhen-cli/src/main.rs
git commit -m "Add kryzhen CLI (mallard-compatible flags + dry-run/verbose)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 12: README and final verification

**Files:**
- Create: `README.md`

- [ ] **Step 1: Write the README**

Create `README.md` documenting: what kryzhen is (mallard-compatible Rust port); the
migration file format with the `#!migration` example; multiple-blocks + implicit-linear-
dependency behavior; the `mallard.applied_migrations` table; checksum tamper detection;
library usage (`kryzhen::migrate` with a `Config`); and CLI usage with the full flag
list. Keep it concise and accurate to the implemented behavior.

- [ ] **Step 2: Full workspace verification**

Run each and confirm exit 0:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Expected: formatting clean, no clippy warnings, all tests pass. Integration tests require
Docker; if unavailable, run `cargo test --workspace --lib` and note that the integration
tests were not run in this environment.

- [ ] **Step 3: Commit**

```bash
git add README.md
git commit -m "Add README

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Self-review notes

- **Spec coverage:** workspace split (Task 1 ↔ §2); modules (Tasks 2–9 ↔ §3); types
  (Task 2 ↔ §4); file format incl. `#!test` rejection (Tasks 4–5 ↔ §5/§9.1); trimmed
  SHA-256 checksum (Task 3 ↔ §6); mallard table (Task 9 ↔ §7); data flow (Task 10 ↔ §8);
  implicit linear deps merged into persisted `requires` (Tasks 6 & 9 ↔ §9.2); CLI flags
  (Task 11 ↔ §10); async tokio-postgres (Tasks 9–10 ↔ §11); typed errors (Task 2 ↔ §12);
  unit + integration tests throughout (↔ §13). §14 open items resolved: hand-written
  parser; testcontainers integration; name kept `kryzhen`.
- **Type consistency:** `Migration`/`MigrationName`/`Error`/`checksum`/`Config`/`Report`
  and the functions `parse_file`, `load_dir`, `apply_implicit_deps`, `topo_sort`,
  `check_duplicate_names`, `check_checksums`, `ensure_schema`, `load_applied`,
  `apply_one`, `migrate` are used with matching signatures across all tasks.
- **Persisted `requires` (§9.2):** satisfied because `apply_implicit_deps` (Task 6)
  mutates `Migration.requires` to the merged set before the applier inserts that field
  (Task 9) — no separate merge needed at insert time.
- **Validation order:** duplicate-name check runs before `topo_sort` (Task 10 sequences
  it first) so duplicates aren't masked by the graph's `HashMap` keying.
```