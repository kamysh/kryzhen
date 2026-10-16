use super::*;

fn args(extra: &[&str]) -> Args {
    Args::try_parse_from(
        ["kryzhen", "--database", "test", "--root", "migrations/"]
            .into_iter()
            .chain(extra.iter().copied()),
    )
    .unwrap()
}

#[test]
fn documented_tls_modes_enforce_transport_policy() {
    use tokio_postgres::config::SslMode as DriverSslMode;
    for (mode, expected) in [
        ("disable", DriverSslMode::Disable),
        ("prefer", DriverSslMode::Prefer),
        ("require", DriverSslMode::Require),
        ("verify-ca", DriverSslMode::Require),
        ("verify-full", DriverSslMode::Require),
    ] {
        let parsed = args(&["--sslmode", mode]);
        assert_eq!(
            connection_config(&parsed.db).unwrap().get_ssl_mode(),
            expected
        );
        assert_eq!(parsed.root.unwrap(), PathBuf::from("migrations/"));
    }
}

#[test]
fn password_is_not_interpreted_as_connection_string() {
    let password = "  a ' \" password:with\\ spaces  ";
    let parsed = args(&["--password", password]);
    let config = connection_config(&parsed.db).unwrap();
    assert_eq!(config.get_password(), Some(password.as_bytes()));
}

#[test]
fn pgpass_preserves_quotes_spaces_and_decodes_escapes() {
    let content = "localhost:5432:test:postgres:  a ' \" password\\:with\\\\ spaces  \r\n";
    assert_eq!(
        pgpass_password(content, "localhost", 5432, "test", "postgres").as_deref(),
        Some("  a ' \" password:with\\ spaces  ")
    );
}

#[test]
fn pgpass_matches_escaped_fields_and_literal_star() {
    let content = r"\:\:1:5432:db\:name:user\\name:correct";
    assert_eq!(
        pgpass_password(content, "::1", 5432, "db:name", "user\\name").as_deref(),
        Some("correct")
    );
    let content = "localhost:5432:\\*:postgres:literal\n*:*:*:*:fallback";
    assert_eq!(
        pgpass_password(content, "localhost", 5432, "*", "postgres").as_deref(),
        Some("literal")
    );
    assert_eq!(
        pgpass_password(content, "localhost", 5432, "other", "postgres").as_deref(),
        Some("fallback")
    );
}

#[test]
fn pgpass_uses_first_match_and_skips_comments_and_incomplete_entries() {
    let content = "#*:*:*:*:comment\n\nlocalhost:5432:test\nother:5432:test:postgres:wrong\nlocalhost:*:test:postgres:first\n*:*:*:*:second\n";
    assert_eq!(
        pgpass_password(content, "localhost", 5432, "test", "postgres").as_deref(),
        Some("first")
    );
    assert_eq!(
        pgpass_password(
            "localhost:5432:test:postgres",
            "localhost",
            5432,
            "test",
            "postgres"
        ),
        None
    );
}

#[test]
fn pgpass_handles_empty_password_line_endings_and_unicode() {
    for ending in ["", "\n", "\r\n", "\r"] {
        let content = format!("localhost:5432:test:postgres:{ending}");
        assert_eq!(
            pgpass_password(&content, "localhost", 5432, "test", "postgres"),
            Some(String::new())
        );
    }
    assert_eq!(
        pgpass_password("*:*:*:*: парóль ", "localhost", 5432, "test", "postgres").as_deref(),
        Some(" парóль ")
    );
    assert_eq!(
        pgpass_password("*:*:*:*:end\\", "localhost", 5432, "test", "postgres").as_deref(),
        Some("end\\")
    );
    assert_eq!(
        pgpass_password(
            "*:*:*:*:first:ignored",
            "localhost",
            5432,
            "test",
            "postgres"
        )
        .as_deref(),
        Some("first")
    );
}

#[test]
fn incomplete_or_disabled_tls_identity_is_rejected() {
    for extra in [
        vec!["--ssl-client-cert", "client.pem"],
        vec!["--ssl-client-key", "client.key"],
        vec!["--sslmode", "disable", "--ssl-root-cert", "ca.pem"],
        vec![
            "--sslmode",
            "disable",
            "--ssl-client-cert",
            "client.pem",
            "--ssl-client-key",
            "client.key",
        ],
    ] {
        assert!(connection_config(&args(&extra).db).is_err());
    }
}

#[test]
fn supplied_certificate_paths_are_read() {
    let parsed = args(&[
        "--sslmode",
        "verify-full",
        "--ssl-root-cert",
        "/nonexistent/kryzhen-test-ca.pem",
    ]);
    assert!(tls_connector(&parsed.db).is_err());
    let parsed = args(&[
        "--ssl-client-cert",
        "/nonexistent/kryzhen-test-cert.pem",
        "--ssl-client-key",
        "/nonexistent/kryzhen-test-key.pem",
    ]);
    assert!(tls_connector(&parsed.db).is_err());
}
