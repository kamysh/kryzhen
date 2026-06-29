use kryzhen::migrate;
use kryzhen::postgres::{apply_one, ensure_schema, load_applied};
use kryzhen::types::{checksum, Migration, MigrationName};
use testcontainers::core::ImageExt;
use testcontainers::runners::AsyncRunner;
use testcontainers::ContainerAsync;
use testcontainers_modules::postgres::Postgres;
use tokio_postgres::{Client, NoTls};

async fn connect() -> (Client, ContainerAsync<Postgres>) {
    // Pin a modern PostgreSQL: the per-schema association table's id defaults to
    // gen_random_uuid(), which is core only on PG13+. The testcontainers default tag is
    // older, where that function lives in the pgcrypto extension.
    let node = Postgres::default().with_tag("17").start().await.unwrap();
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
    apply_one(&mut client, &m, "public").await.unwrap();

    let rows = client.query("SELECT count(*) FROM t", &[]).await.unwrap();
    let count: i64 = rows[0].get(0);
    assert_eq!(count, 0);

    let applied = load_applied(&client).await.unwrap();
    assert_eq!(
        applied.get(&MigrationName("create_t".into())),
        Some(&checksum("CREATE TABLE t (id int);"))
    );
}

#[tokio::test]
async fn apply_two_migrations_both_recorded() {
    let (mut client, _node) = connect().await;
    ensure_schema(&client).await.unwrap();
    apply_one(
        &mut client,
        &mig("m1", "CREATE TABLE a (id int);"),
        "public",
    )
    .await
    .unwrap();
    apply_one(
        &mut client,
        &mig("m2", "CREATE TABLE b (id int);"),
        "public",
    )
    .await
    .unwrap();
    let applied = load_applied(&client).await.unwrap();
    assert_eq!(applied.len(), 2);
    assert!(applied.contains_key(&MigrationName("m1".into())));
    assert!(applied.contains_key(&MigrationName("m2".into())));
}

#[tokio::test]
async fn migrate_end_to_end_applies_in_dependency_order_and_is_idempotent() {
    let (mut client, _node) = connect().await;

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

    let migrations = kryzhen::file::load_dir(&dir).unwrap();
    let report = migrate(&mut client, &migrations, "public", false)
        .await
        .unwrap();
    assert_eq!(report.applied, vec!["a".to_string(), "b".to_string()]);

    let report2 = migrate(&mut client, &migrations, "public", false)
        .await
        .unwrap();
    assert!(report2.applied.is_empty());

    std::fs::remove_dir_all(&dir).ok();
}

/// Absolute path to the vendored copy of mallard's own `sql/example-contacts`
/// example migration tree (fetched verbatim from AndrewRademacher/mallard).
fn mallard_example_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mallard-example-contacts")
}

/// Copy only the migration files (schema.sql + tables/) of the mallard example into a
/// fresh temp dir, deliberately excluding `tests/` (which holds a `#!test` block that
/// kryzhen does not support). Returns the temp dir path.
fn stage_mallard_migrations(tag: &str) -> std::path::PathBuf {
    let src = mallard_example_dir();
    let dst = std::env::temp_dir().join(format!("kryzhen-mallard-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(dst.join("tables")).unwrap();
    std::fs::copy(src.join("schema.sql"), dst.join("schema.sql")).unwrap();
    std::fs::copy(src.join("tables/person.sql"), dst.join("tables/person.sql")).unwrap();
    std::fs::copy(src.join("tables/phone.sql"), dst.join("tables/phone.sql")).unwrap();
    dst
}

#[tokio::test]
async fn applies_mallard_example_contacts_tree() {
    let (mut client, _node) = connect().await;

    let dir = stage_mallard_migrations("apply");
    let migrations = kryzhen::file::load_dir(&dir).unwrap();

    let report = migrate(&mut client, &migrations, "public", false)
        .await
        .unwrap();
    assert_eq!(
        report.applied,
        vec![
            "schema".to_string(),
            "tables/person".to_string(),
            "tables/phone".to_string(),
            "tables/phone/name".to_string(),
        ]
    );

    let report2 = migrate(&mut client, &migrations, "public", false)
        .await
        .unwrap();
    assert!(report2.applied.is_empty());
    assert_eq!(report2.already_applied.len(), 4);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn implicit_dep_does_not_duplicate_explicit_predecessor() {
    let migs = kryzhen::file::load_dir(&mallard_example_dir().join("tables")).unwrap();
    let phone_name = migs
        .iter()
        .find(|m| m.name == MigrationName("tables/phone/name".into()))
        .expect("tables/phone/name migration present");
    assert_eq!(
        phone_name.requires,
        vec![MigrationName("tables/phone".into())],
        "explicit predecessor must not be duplicated by the implicit in-file dep"
    );
}

#[test]
fn mallard_test_block_is_rejected() {
    let err = kryzhen::file::load_dir(&mallard_example_dir()).unwrap_err();
    assert!(
        matches!(err, kryzhen::Error::Parse { .. }),
        "a #!test block must be a parse error, got: {err:?}"
    );
}

/// The same migration set applied to two schemas builds tables independently in each,
/// records per-(migration, schema) associations, and is idempotent per schema. A schema
/// that lags behind catches up when migrate() is run against it again.
#[tokio::test]
async fn same_migrations_apply_to_multiple_schemas_independently() {
    let (mut client, _node) = connect().await;

    // Two tenant schemas. Migration bodies are UNQUALIFIED, so they template into
    // whichever schema is the migrate() target via search_path.
    client
        .batch_execute("CREATE SCHEMA tenant_a; CREATE SCHEMA tenant_b;")
        .await
        .unwrap();

    let dir = std::env::temp_dir().join(format!("kryzhen-multi-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("a.sql"),
        "-- #!migration\n-- name: \"a\",\n-- description: \"a\";\nCREATE TABLE widgets (id int);\n",
    )
    .unwrap();
    let migs_v1 = kryzhen::file::load_dir(&dir).unwrap();

    // Apply v1 to tenant_a only.
    let r = migrate(&mut client, &migs_v1, "tenant_a", false)
        .await
        .unwrap();
    assert_eq!(r.applied, vec!["a".to_string()]);

    // tenant_a has widgets; tenant_b does not (the migration never touched it).
    let in_a: i64 = client
        .query_one(
            "SELECT count(*) FROM information_schema.tables WHERE table_schema='tenant_a' AND table_name='widgets'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    let in_b: i64 = client
        .query_one(
            "SELECT count(*) FROM information_schema.tables WHERE table_schema='tenant_b' AND table_name='widgets'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(in_a, 1, "tenant_a.widgets should exist");
    assert_eq!(in_b, 0, "tenant_b.widgets should NOT exist yet");

    // Re-running against tenant_a is a no-op (per-schema skip-set).
    let r = migrate(&mut client, &migs_v1, "tenant_a", false)
        .await
        .unwrap();
    assert!(r.applied.is_empty());
    assert_eq!(r.already_applied, vec!["a".to_string()]);

    // Now bring tenant_b up to the SAME v1 — it was lagging and catches up.
    let r = migrate(&mut client, &migs_v1, "tenant_b", false)
        .await
        .unwrap();
    assert_eq!(r.applied, vec!["a".to_string()]);
    let in_b: i64 = client
        .query_one(
            "SELECT count(*) FROM information_schema.tables WHERE table_schema='tenant_b' AND table_name='widgets'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(in_b, 1, "tenant_b.widgets should now exist");

    // The canonical applied_migrations has ONE row for "a" (schema-independent body),
    // and the association table has one row per schema.
    let canonical: i64 = client
        .query_one(
            "SELECT count(*) FROM mallard.applied_migrations WHERE name='a'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(canonical, 1, "one canonical row per migration name");
    let assoc: i64 = client
        .query_one(
            "SELECT count(*) FROM mallard.applied_migration_schemas WHERE migration_name='a'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(assoc, 2, "one association row per (migration, schema)");

    std::fs::remove_dir_all(&dir).ok();
}

/// A database created by a pre-multi-schema kryzhen (rows in applied_migrations, no
/// association table) is soft-upgraded on the next run: ensure_schema backfills
/// (name, 'public') and stamps migrator_version, so the migrations are NOT re-run.
#[tokio::test]
async fn soft_upgrade_backfills_existing_database_without_rerun() {
    let (mut client, _node) = connect().await;

    // Simulate an OLD kryzhen database: the canonical schema/table with one applied
    // migration, a real table it created, and NO association/version tables.
    client
        .batch_execute(
            "CREATE SCHEMA mallard; \
             CREATE TABLE mallard.applied_migrations( \
                 id bigserial NOT NULL, name text NOT NULL, description text NOT NULL, \
                 requires text[] NOT NULL, checksum bytea NOT NULL, script_text text NOT NULL, \
                 applied_on timestamptz NOT NULL DEFAULT now(), PRIMARY KEY (id)); \
             CREATE TABLE legacy_t (id int);",
        )
        .await
        .unwrap();
    let body = "CREATE TABLE legacy_t (id int);";
    client
        .execute(
            "INSERT INTO mallard.applied_migrations (name, description, requires, checksum, script_text) \
             VALUES ('legacy', 'legacy', ARRAY[]::text[], $1, $2)",
            &[&kryzhen::types::checksum(body).as_slice(), &body],
        )
        .await
        .unwrap();

    // The on-disk migration set matches what's already applied. Without the soft upgrade,
    // migrate() would see an empty per-schema set and try to re-run `legacy`, failing on
    // `relation "legacy_t" already exists`.
    let dir = std::env::temp_dir().join(format!("kryzhen-upgrade-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("legacy.sql"),
        format!("-- #!migration\n-- name: \"legacy\",\n-- description: \"legacy\";\n{body}\n"),
    )
    .unwrap();
    let migrations = kryzhen::file::load_dir(&dir).unwrap();

    let report = migrate(&mut client, &migrations, "public", false)
        .await
        .expect("soft upgrade must let an up-to-date legacy DB run without re-applying");
    assert!(report.applied.is_empty(), "nothing should be re-applied");
    assert_eq!(report.already_applied, vec!["legacy".to_string()]);

    // Backfill produced the association row, and the version is stamped.
    let assoc: i64 = client
        .query_one(
            "SELECT count(*) FROM mallard.applied_migration_schemas \
             WHERE migration_name='legacy' AND schema='public'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(assoc, 1, "backfill should record (legacy, public)");
    let version: i64 = client
        .query_one(
            "SELECT coalesce(max(version),0) FROM mallard.migrator_version",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(version, 1, "migrator_version should be stamped to 1");

    // Applying a NEW migration to the soft-upgraded old table must work. This is the case
    // the original test missed: the old applied_migrations had no UNIQUE(name), so apply_one's
    // `INSERT ... ON CONFLICT (name)` would error 42P10 unless ensure_schema retrofitted the
    // unique index. Append a second migration on disk and re-run.
    std::fs::write(
        dir.join("legacy.sql"),
        format!(
            "-- #!migration\n-- name: \"legacy\",\n-- description: \"legacy\";\n{body}\n\n\
             -- #!migration\n-- name: \"after-upgrade\",\n-- description: \"after\";\n\
             CREATE TABLE after_upgrade_t (id int);\n"
        ),
    )
    .unwrap();
    let migrations = kryzhen::file::load_dir(&dir).unwrap();
    let report = migrate(&mut client, &migrations, "public", false)
        .await
        .expect("a new migration must apply onto a soft-upgraded (no-UNIQUE) old table");
    assert_eq!(report.applied, vec!["after-upgrade".to_string()]);

    // The version stamp is idempotent: re-running ensure_schema (via migrate) leaves exactly
    // one migrator_version row.
    let version_rows: i64 = client
        .query_one("SELECT count(*) FROM mallard.migrator_version", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(version_rows, 1, "migrator_version must stay a single row");

    std::fs::remove_dir_all(&dir).ok();
}

/// kryzhen connects and applies a migration over a real TLS handshake.
/// Set `KRYZHEN_TLS_DSN_PORT` to opt in (see inline docs for setup).
#[tokio::test]
async fn migrate_over_tls_require() {
    let Ok(port) = std::env::var("KRYZHEN_TLS_DSN_PORT") else {
        eprintln!("KRYZHEN_TLS_DSN_PORT unset; skipping real-TLS test");
        return;
    };
    let port: u16 = port
        .parse()
        .expect("KRYZHEN_TLS_DSN_PORT must be a port number");

    let (probe, conn) = tokio_postgres::connect(
        &format!("host=127.0.0.1 port={port} user=postgres password=postgres dbname=postgres sslmode=disable"),
        NoTls,
    )
    .await
    .expect("connect to fixture server");
    tokio::spawn(async move {
        let _ = conn.await;
    });

    let server_ssl: String = probe.query_one("SHOW ssl", &[]).await.unwrap().get(0);
    assert_eq!(server_ssl, "on", "fixture server must have ssl=on");

    probe
        .batch_execute("DROP TABLE IF EXISTS tls_t; DROP SCHEMA IF EXISTS mallard CASCADE;")
        .await
        .unwrap();

    let dir = std::env::temp_dir().join(format!("kryzhen-tls-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("a.sql"),
        "-- #!migration\n-- name: \"a\",\n-- description: \"a\";\nCREATE TABLE tls_t (id int);\n",
    )
    .unwrap();

    let builder = native_tls::TlsConnector::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();
    let connector = postgres_native_tls::MakeTlsConnector::new(builder);
    let conn_str = format!(
        "host=127.0.0.1 port={port} user=postgres password=postgres dbname=postgres sslmode=require"
    );
    let (mut client, conn) = tokio_postgres::connect(&conn_str, connector)
        .await
        .expect("connect with sslmode=require");
    tokio::spawn(async move {
        let _ = conn.await;
    });

    let migrations = kryzhen::file::load_dir(&dir).unwrap();
    let report = migrate(&mut client, &migrations, "public", false)
        .await
        .expect("migrate over sslmode=require should succeed");
    assert_eq!(report.applied, vec!["a".to_string()]);

    std::fs::remove_dir_all(&dir).ok();
}
