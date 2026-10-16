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
    let parsed = args(&["--password", "a ' password\\ with spaces"]);
    let config = connection_config(&parsed.db).unwrap();
    assert_eq!(
        config.get_password(),
        Some(b"a ' password\\ with spaces".as_slice())
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
