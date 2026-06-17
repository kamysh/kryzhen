use winnow::{
    combinator::{alt, preceded, repeat, separated, terminated},
    token::{none_of, rest, take_until, take_while},
    ModalResult, Parser,
};

use crate::types::{checksum, Migration, MigrationName};
use crate::{Error, Result};

pub fn parse_file(text: &str, file_label: &str) -> Result<Vec<Migration>> {
    let err = |msg: String| Error::Parse {
        file: file_label.to_string(),
        message: msg,
    };

    migration_file
        .parse(text)
        .map_err(|e| err(e.to_string()))
}

// ---------------------------------------------------------------------------
// Primitives
// ---------------------------------------------------------------------------

fn hspace(input: &mut &str) -> ModalResult<()> {
    take_while(0.., |c: char| c == ' ' || c == '\t')
        .void()
        .parse_next(input)
}

fn ws(input: &mut &str) -> ModalResult<()> {
    take_while(0.., |c: char| c.is_whitespace())
        .void()
        .parse_next(input)
}

fn comment_prefix(input: &mut &str) -> ModalResult<()> {
    (ws, "-- ").void().parse_next(input)
}

fn quoted_char(input: &mut &str) -> ModalResult<char> {
    alt((preceded('\\', alt(('"', '\\'))), none_of('"'))).parse_next(input)
}

fn quoted_string(input: &mut &str) -> ModalResult<String> {
    preceded(
        '"',
        terminated(
            repeat(0.., quoted_char).map(|v: Vec<char>| v.into_iter().collect()),
            '"',
        ),
    )
    .parse_next(input)
}

// ---------------------------------------------------------------------------
// Field value
// ---------------------------------------------------------------------------

enum FieldValue {
    Single(String),
    Multi(Vec<String>),
}

fn field_value(input: &mut &str) -> ModalResult<FieldValue> {
    preceded(
        ws,
        alt((
            preceded(
                ('[', ws),
                terminated(
                    separated(0.., quoted_string, (ws, ',', ws)).map(FieldValue::Multi),
                    (ws, ']'),
                ),
            ),
            quoted_string.map(FieldValue::Single),
        )),
    )
    .parse_next(input)
}

// ---------------------------------------------------------------------------
// Header fields
// ---------------------------------------------------------------------------

fn kv(input: &mut &str) -> ModalResult<(String, FieldValue)> {
    preceded(
        comment_prefix,
        (
            take_while(1.., |c: char| c.is_alphanumeric() || c == '_').map(str::to_owned),
            preceded((ws, ':', ws), field_value),
        ),
    )
    .parse_next(input)
}

fn field_comma(input: &mut &str) -> ModalResult<(String, FieldValue)> {
    terminated(kv, (hspace, ',', hspace, '\n')).parse_next(input)
}

fn field_semi(input: &mut &str) -> ModalResult<(String, FieldValue)> {
    terminated(kv, (hspace, ';', hspace, '\n')).parse_next(input)
}

fn header(input: &mut &str) -> ModalResult<Vec<(String, FieldValue)>> {
    let mut fields: Vec<(String, FieldValue)> = repeat(0.., field_comma).parse_next(input)?;
    fields.push(field_semi.parse_next(input)?);
    Ok(fields)
}

// ---------------------------------------------------------------------------
// Body and shebang
// ---------------------------------------------------------------------------

fn body(input: &mut &str) -> ModalResult<String> {
    preceded(
        ws,
        alt((take_until(0.., "\n-- #!"), rest)).map(|s: &str| s.trim_end().to_owned()),
    )
    .parse_next(input)
}

fn shebang(input: &mut &str) -> ModalResult<()> {
    (take_until(0.., "-- #!migration"), "-- #!migration")
        .void()
        .parse_next(input)
}

// ---------------------------------------------------------------------------
// Migration block
// ---------------------------------------------------------------------------

fn migration(input: &mut &str) -> ModalResult<Migration> {
    shebang.parse_next(input)?;
    let fields = header(input)?;
    let script = body(input)?;

    fn get<'a>(fields: &'a [(String, FieldValue)], key: &str) -> Option<&'a FieldValue> {
        fields.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    let name = match get(&fields, "name") {
        Some(FieldValue::Single(s)) => MigrationName(s.clone()),
        _ => return Err(winnow::error::ErrMode::Cut(winnow::error::ContextError::new())),
    };
    let description = match get(&fields, "description") {
        Some(FieldValue::Single(s)) => s.clone(),
        _ => return Err(winnow::error::ErrMode::Cut(winnow::error::ContextError::new())),
    };
    let requires = match get(&fields, "requires") {
        None => vec![],
        Some(FieldValue::Single(s)) => vec![MigrationName(s.clone())],
        Some(FieldValue::Multi(xs)) => xs.iter().cloned().map(MigrationName).collect(),
    };

    Ok(Migration {
        name,
        description,
        requires,
        checksum: checksum(&script),
        script,
    })
}

fn migration_file(input: &mut &str) -> ModalResult<Vec<Migration>> {
    repeat(0.., migration).parse_next(input)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_block() {
        let text = concat!(
            "-- #!migration\n",
            "-- name: \"tables/phone\",\n",
            "-- description: \"Phone numbers attached to a person.\",\n",
            "-- requires: [\"tables/person\"];\n",
            "CREATE TABLE phone (id bigint);\n",
        );
        let m = parse_file(text, "phone.sql").unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].name, MigrationName("tables/phone".into()));
        assert_eq!(m[0].description, "Phone numbers attached to a person.");
        assert_eq!(m[0].requires, vec![MigrationName("tables/person".into())]);
        assert_eq!(m[0].script, "CREATE TABLE phone (id bigint);");
        assert_eq!(m[0].checksum, checksum("CREATE TABLE phone (id bigint);"));
    }

    #[test]
    fn requires_optional() {
        let text = concat!(
            "-- #!migration\n",
            "-- name: \"a\",\n",
            "-- description: \"first\";\n",
            "SELECT 1;\n",
        );
        let m = parse_file(text, "a.sql").unwrap();
        assert!(m[0].requires.is_empty());
    }

    #[test]
    fn requires_can_be_single_string() {
        let text = concat!(
            "-- #!migration\n",
            "-- name: \"b\",\n",
            "-- description: \"second\",\n",
            "-- requires: \"a\";\n",
            "SELECT 2;\n",
        );
        let m = parse_file(text, "b.sql").unwrap();
        assert_eq!(m[0].requires, vec![MigrationName("a".into())]);
    }

    #[test]
    fn parses_multiple_blocks_in_file_order() {
        let text = concat!(
            "-- #!migration\n",
            "-- name: \"a\",\n",
            "-- description: \"first\";\n",
            "CREATE TABLE a ();\n",
            "-- #!migration\n",
            "-- name: \"b\",\n",
            "-- description: \"second\";\n",
            "CREATE TABLE b ();\n",
        );
        let m = parse_file(text, "multi.sql").unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].name, MigrationName("a".into()));
        assert_eq!(m[1].name, MigrationName("b".into()));
        assert_eq!(m[0].script, "CREATE TABLE a ();");
        assert_eq!(m[1].script, "CREATE TABLE b ();");
    }

    #[test]
    fn missing_name_is_error() {
        let text = concat!(
            "-- #!migration\n",
            "-- description: \"no name\";\n",
            "SELECT 1;\n",
        );
        assert!(matches!(
            parse_file(text, "x.sql").unwrap_err(),
            Error::Parse { .. }
        ));
    }

    #[test]
    fn body_preserves_leading_sql_comment() {
        let text = concat!(
            "-- #!migration\n",
            "-- name: \"initial\",\n",
            "-- description: \"sets up the graph\";\n",
            "-- Create the AGE graph if it doesn't already exist.\n",
            "SELECT 1;\n",
        );
        let m = parse_file(text, "001.sql").unwrap();
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
        assert_eq!(m[0].name, MigrationName("belief-embeddings".into()));
        assert!(m[0].description.contains(';'));
        assert!(m[0].description.contains('`'));
        assert_eq!(
            m[0].requires,
            vec![MigrationName("database-search-path".into())]
        );
    }

    #[test]
    fn quoted_string_plain() {
        let mut input = r#""hello""#;
        assert_eq!(quoted_string(&mut input).unwrap(), "hello");
    }

    #[test]
    fn quoted_string_escaped_quote() {
        let mut input = r#""say \"hi\"""#;
        assert_eq!(quoted_string(&mut input).unwrap(), r#"say "hi""#);
    }

    #[test]
    fn quoted_string_escaped_backslash() {
        let mut input = r#""a\\b""#;
        assert_eq!(quoted_string(&mut input).unwrap(), r#"a\b"#);
    }

    #[test]
    fn quoted_string_escaped_backslash_before_quote() {
        let mut input = r#""a\\\"b""#;
        assert_eq!(quoted_string(&mut input).unwrap(), r#"a\"b"#);
    }

    #[test]
    fn description_with_escaped_quote() {
        let text = concat!(
            "-- #!migration\n",
            "-- name: \"x\",\n",
            r#"-- description: "say \"hi\"";"#,
            "\nSELECT 1;\n",
        );
        let m = parse_file(text, "x.sql").unwrap();
        assert_eq!(m[0].description, r#"say "hi""#);
    }
}
