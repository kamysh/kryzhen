//! Parser for migration files: splits a `.sql` file into its `#!migration` blocks (in
//! file order) and parses each block's header fields and SQL body. `#!test` blocks are
//! rejected. See [`parse_file`].

use crate::types::{checksum, Migration, MigrationName};
use crate::{Error, Result};

/// Parse all `#!migration` blocks from one file's text, in file order.
/// `file_label` is used in error messages.
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
        // The next block begins at its `#!`; back up to the start of that line so the
        // line prefix (e.g. `-- `) stays with the next block rather than leaking into
        // this block's body.
        let next = match rest_start.find("#!") {
            None => rest_start.len(),
            Some(hash) => rest_start[..hash].rfind('\n').map_or(hash, |nl| nl + 1),
        };
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
        .and_then(|(_, v)| v.as_str())
        .ok_or_else(|| err("missing or non-string `name` field".into()))?;
    let description = fields
        .iter()
        .find(|(k, _)| k == "description")
        .and_then(|(_, v)| v.as_str())
        .ok_or_else(|| err("missing or non-string `description` field".into()))?;
    let requires = match fields.iter().find(|(k, _)| k == "requires") {
        None => Vec::new(),
        Some((_, FieldValue::Text(s))) => vec![MigrationName(s.clone())],
        Some((_, FieldValue::List(xs))) => xs.iter().cloned().map(MigrationName).collect(),
    };

    Ok(Migration {
        name: MigrationName(name.to_string()),
        description: description.to_string(),
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
    fn as_str(&self) -> Option<&str> {
        match self {
            FieldValue::Text(s) => Some(s.as_str()),
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
///
/// Backslash-escaped quotes inside header strings are NOT supported: a `"` always
/// ends the string. This matches the upstream mallard format, whose parser uses
/// `noneOf "\""` and likewise does not handle escapes.
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

    #[test]
    fn missing_name_is_error() {
        let text = r#"
-- #!migration
-- description: "no name";
SELECT 1;
"#;
        assert!(matches!(
            parse_file(text, "x.sql").unwrap_err(),
            Error::Parse { .. }
        ));
    }

    #[test]
    fn missing_semicolon_is_error() {
        // No `;` anywhere in the block, so the header field list is unterminated and
        // `find(';')` returns None — exercises the "missing terminating `;`" branch.
        let text = "-- #!migration\n-- name: \"x\", description: \"y\"\nSELECT 1\n";
        assert!(matches!(
            parse_file(text, "x.sql").unwrap_err(),
            Error::Parse { .. }
        ));
    }
}
