//! Parser for migration files: splits a `.sql` file into its `#!migration` blocks (in
//! file order) and parses each block's header fields and SQL body. `#!test` blocks are
//! rejected. See [`parse_file`].
//!
//! Faithfully mirrors the Haskell mallard Megaparsec grammar:
//! - `-` is treated as whitespace (transparently skips `-- ` comment prefixes)
//! - Quoted strings allow any character except `"` (including `;`, `,`, backticks)
//! - Fields are `key: value` pairs separated by `,`; the header ends at the first
//!   unquoted `;`

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
    let mut p = Parser::new(text);
    p.skip_ws();

    while !p.is_eof() {
        match p.find_shebang() {
            None => break,
            Some(pos) => p.pos = pos + 2,
        }
        let directive = p.read_word();
        match directive.as_str() {
            "migration" => out.push(parse_migration_block(&mut p, &err)?),
            "test" => {
                return Err(err(
                    "unsupported directive `#!test` (only `#!migration` is supported)".into(),
                ))
            }
            other => {
                return Err(err(format!(
                    "unsupported directive `#!{other}` (only `#!migration` is supported)"
                )))
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Low-level parser state
// ---------------------------------------------------------------------------

struct Parser<'a> {
    src: &'a str,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str) -> Self {
        Self { src, pos: 0 }
    }

    fn rest(&self) -> &'a str {
        &self.src[self.pos..]
    }

    fn is_eof(&self) -> bool {
        self.pos >= self.src.len()
    }

    /// Skip characters that are whitespace or `-` (mirrors mallard's spaceConsumer).
    fn skip_ws(&mut self) {
        while let Some(c) = self.rest().chars().next() {
            if c.is_whitespace() || c == '-' {
                self.pos += c.len_utf8();
            } else {
                break;
            }
        }
    }

    /// Read alphanumeric/underscore word (field name or directive keyword), then skip_ws.
    fn read_word(&mut self) -> String {
        let start = self.pos;
        while let Some(c) = self.rest().chars().next() {
            if c.is_alphanumeric() || c == '_' {
                self.pos += c.len_utf8();
            } else {
                break;
            }
        }
        let word = self.src[start..self.pos].to_string();
        self.skip_ws();
        word
    }

    /// Expect a specific character (after skip_ws), advance past it and skip_ws again.
    fn expect(&mut self, ch: char, err: &impl Fn(String) -> Error) -> Result<()> {
        self.skip_ws();
        match self.rest().chars().next() {
            Some(c) if c == ch => {
                self.pos += ch.len_utf8();
                self.skip_ws();
                Ok(())
            }
            Some(c) => Err(err(format!("expected `{ch}`, got `{c}`"))),
            None => Err(err(format!("expected `{ch}`, got end of file"))),
        }
    }

    /// Try to consume a specific character (after skip_ws); return true if consumed.
    fn try_consume(&mut self, ch: char) -> bool {
        self.skip_ws();
        if self.rest().starts_with(ch) {
            self.pos += ch.len_utf8();
            self.skip_ws();
            true
        } else {
            false
        }
    }

    /// Like `try_consume(ch)` but does NOT call `skip_ws` after consuming.
    /// Used for the header-terminating `;`: `skip_ws` treats `-` as whitespace
    /// (so it can transparently skip `-- ` comment prefixes inside the header),
    /// but past the terminator the `--` is the start of a SQL line comment in
    /// the body. The post-consume `skip_ws` would strip it and turn the comment
    /// text into bare SQL tokens.
    fn try_consume_terminator(&mut self, ch: char) -> bool {
        self.skip_ws();
        if self.rest().starts_with(ch) {
            self.pos += ch.len_utf8();
            true
        } else {
            false
        }
    }

    /// Parse a double-quoted string: any chars except `"`. Mirrors `noneOf "\""`.
    fn parse_quoted(&mut self, err: &impl Fn(String) -> Error) -> Result<String> {
        self.skip_ws();
        if !self.rest().starts_with('"') {
            return Err(err(format!(
                "expected a quoted string, got `{}`",
                self.rest()
                    .chars()
                    .next()
                    .map_or_else(|| "EOF".to_string(), |c| c.to_string())
            )));
        }
        self.pos += 1; // opening "
        match self.rest().find('"') {
            None => Err(err("unterminated quoted string".into())),
            Some(i) => {
                let s = self.src[self.pos..self.pos + i].to_string();
                self.pos += i + 1; // closing "
                self.skip_ws();
                Ok(s)
            }
        }
    }

    /// Find the byte offset of the next `#!` in the remaining text, or None.
    fn find_shebang(&self) -> Option<usize> {
        self.rest().find("#!").map(|i| self.pos + i)
    }

    /// Collect raw body text up to (but not including) the next `#!` or EOF.
    /// Backs up to the start of the line containing `#!` so `-- #!` prefixes
    /// belong to the next block. Trims surrounding whitespace and `-`.
    fn read_body(&mut self) -> String {
        let start = self.pos;
        let end = match self.rest().find("#!") {
            None => self.src.len(),
            Some(i) => {
                let abs = self.pos + i;
                self.src[start..abs]
                    .rfind('\n')
                    .map_or(abs, |nl| start + nl + 1)
            }
        };
        self.pos = end;
        let raw = &self.src[start..end];
        // Mirror mallard's `dropWhileEnd isWhiteSpace` (isWhiteSpace includes '-')
        let trimmed = raw.trim_end_matches(|c: char| c.is_whitespace() || c == '-');
        trimmed.trim().to_string()
    }
}

// ---------------------------------------------------------------------------
// Field value
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Header field parsing
// ---------------------------------------------------------------------------

/// Parse `key: value` pairs separated by `,`, ending at `;`.
/// Mirrors mallard's `parseHeaderFields` + `semiColon`.
fn parse_header_fields(
    p: &mut Parser<'_>,
    err: &impl Fn(String) -> Error,
) -> Result<Vec<(String, FieldValue)>> {
    let mut fields = Vec::new();
    loop {
        p.skip_ws();
        if p.try_consume_terminator(';') {
            break;
        }
        if !fields.is_empty() {
            p.expect(',', err)?;
            if p.try_consume_terminator(';') {
                break;
            }
        }
        let key = p.read_word();
        if key.is_empty() {
            return Err(err("header is missing its terminating `;`".into()));
        }
        p.expect(':', err)?;
        let value = parse_field_value(p, err)?;
        fields.push((key, value));
    }
    Ok(fields)
}

fn parse_field_value(p: &mut Parser<'_>, err: &impl Fn(String) -> Error) -> Result<FieldValue> {
    p.skip_ws();
    if p.rest().starts_with('[') {
        p.pos += 1; // `[`
        p.skip_ws();
        let mut items = Vec::new();
        if !p.try_consume(']') {
            loop {
                items.push(p.parse_quoted(err)?);
                if p.try_consume(']') {
                    break;
                }
                p.expect(',', err)?;
            }
        }
        Ok(FieldValue::List(items))
    } else {
        Ok(FieldValue::Text(p.parse_quoted(err)?))
    }
}

// ---------------------------------------------------------------------------
// Migration block
// ---------------------------------------------------------------------------

fn parse_migration_block(p: &mut Parser<'_>, err: &impl Fn(String) -> Error) -> Result<Migration> {
    let fields = parse_header_fields(p, err)?;

    let name = fields
        .iter()
        .find(|(k, _)| k == "name")
        .and_then(|(_, v)| v.as_str())
        .ok_or_else(|| err("missing or non-string `name` field".into()))?
        .to_string();

    let description = fields
        .iter()
        .find(|(k, _)| k == "description")
        .and_then(|(_, v)| v.as_str())
        .ok_or_else(|| err("missing or non-string `description` field".into()))?
        .to_string();

    let requires = match fields.iter().find(|(k, _)| k == "requires") {
        None => Vec::new(),
        Some((_, FieldValue::Text(s))) => vec![MigrationName(s.clone())],
        Some((_, FieldValue::List(xs))) => xs.iter().cloned().map(MigrationName).collect(),
    };

    let body = p.read_body();

    Ok(Migration {
        name: MigrationName(name),
        description,
        requires,
        checksum: checksum(&body),
        script: body,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

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
        let text = "-- #!migration\n-- name: \"x\", description: \"y\"\nSELECT 1\n";
        assert!(matches!(
            parse_file(text, "x.sql").unwrap_err(),
            Error::Parse { .. }
        ));
    }

    #[test]
    fn body_preserves_leading_sql_comment() {
        // Regression: pre-0.6.1, `skip_ws` after the header `;` ate the
        // leading `--` of the next line (a SQL comment), so the body began
        // with bare prose. PostgreSQL then parsed `Create the AGE graph...`
        // as a CREATE statement followed by `the` and bailed with
        // `syntax error at or near "the"`.
        let text = concat!(
            "-- #!migration\n",
            "-- name: \"initial\",\n",
            "-- description: \"sets up the graph\";\n",
            "-- Create the AGE graph if it doesn't already exist.\n",
            "SELECT 1;\n",
        );
        let m = parse_file(text, "001.sql").unwrap();
        assert_eq!(m.len(), 1);
        assert!(
            m[0].script.starts_with("--"),
            "body should preserve leading `--` comment, got: {:?}",
            m[0].script
        );
    }

    #[test]
    fn description_with_semicolon_and_backticks() {
        let text = concat!(
            "-- #!migration\n",
            "-- name: \"belief-embeddings\",\n",
            "-- description: \"embeddings; backfilled by `mimir reembed`\",\n",
            "-- requires: \"database-search-path\";\n",
            "CREATE TABLE t ();\n",
        );
        let m = parse_file(text, "003.sql").unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].name, MigrationName("belief-embeddings".into()));
        assert!(m[0].description.contains(';'));
        assert!(m[0].description.contains('`'));
        assert_eq!(
            m[0].requires,
            vec![MigrationName("database-search-path".into())]
        );
    }
}
