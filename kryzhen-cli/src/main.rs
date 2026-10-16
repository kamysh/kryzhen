use clap::{Parser, Subcommand};
use kryzhen::sqlx_import::{self, SqlxImportError};
use kryzhen::{file, graph, hack_add, hack_delete, hack_fix_checksum, migrate};
use native_tls::TlsConnector;
use postgres_native_tls::MakeTlsConnector;
use std::path::PathBuf;
use tokio_postgres::{Client, NoTls};

/// Forward-only, dependency-resolved SQL migrations for PostgreSQL (mallard-compatible).
#[derive(Parser, Debug)]
#[command(name = "kryzhen", version)]
struct Args {
    /// Root directory of the migration tree.
    #[arg(short, long, global = true)]
    root: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Cmd>,

    #[command(flatten)]
    db: DbArgs,

    /// Print the planned migration order; apply nothing.
    #[arg(long, global = true)]
    dry_run: bool,

    /// Verbose logging.
    #[arg(short = 'v', long)]
    verbose: bool,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Parse migrations and verify all dependencies are satisfied.
    Verify,
    /// Manually manipulate mallard.applied_migrations or migrate from other tools.
    Hack {
        // Boxed to keep the enum's variants similarly sized (clippy::large_enum_variant):
        // HackCmd carries DbArgs plus nested subcommands and is far larger than `Verify`.
        #[command(subcommand)]
        action: Box<HackCmd>,
    },
}

#[derive(Subcommand, Debug)]
enum HackCmd {
    /// Record a migration as applied without running its SQL.
    Add {
        name: String,
        #[command(flatten)]
        db: DbArgs,
        #[arg(short = 'v', long)]
        verbose: bool,
    },
    /// Remove a migration record without running any SQL.
    Delete {
        name: String,
        #[command(flatten)]
        db: DbArgs,
        #[arg(short = 'v', long)]
        verbose: bool,
    },
    /// Recompute and overwrite the stored checksum for an already-applied migration.
    FixChecksum {
        name: String,
        #[command(flatten)]
        db: DbArgs,
        #[arg(short = 'v', long)]
        verbose: bool,
    },
    /// Migrate from another migration tool.
    MigrateFrom {
        #[command(subcommand)]
        source: MigrateFromCmd,
    },
}

#[derive(Subcommand, Debug)]
enum MigrateFromCmd {
    /// Migrate from sqlx (_sqlx_migrations table).
    Sqlx {
        #[command(subcommand)]
        phase: SqlxPhaseCmd,
    },
}

#[derive(Subcommand, Debug)]
enum SqlxPhaseCmd {
    /// Verify checksums and rewrite files with #!migration headers; write receipt.
    Convert {
        #[command(flatten)]
        db: DbArgs,
        #[arg(short = 'v', long)]
        verbose: bool,
    },
    /// Re-verify _sqlx_migrations against the receipt, then insert into mallard.
    Import {
        #[command(flatten)]
        db: DbArgs,
        #[arg(short = 'v', long)]
        verbose: bool,
    },
    /// Run convert then import in sequence.
    All {
        #[command(flatten)]
        db: DbArgs,
        #[arg(short = 'v', long)]
        verbose: bool,
    },
}

// ---------------------------------------------------------------------------
// DB connection flags
// ---------------------------------------------------------------------------

#[derive(clap::Args, Clone, Debug)]
struct DbArgs {
    #[arg(long, global = true)]
    database: Option<String>,
    #[arg(long, global = true, default_value = "127.0.0.1")]
    host: String,
    #[arg(long, global = true, default_value_t = 5432)]
    port: u16,
    #[arg(long, global = true, default_value = "postgres")]
    user: String,
    /// Target schema for the migrations. Migrations are templates: unqualified DDL is
    /// created in this schema (via search_path). Apply to several schemas by running
    /// once per schema. When omitted, defaults to the connection's current_schema()
    /// (the first entry in its search_path).
    #[arg(long, global = true)]
    schema: Option<String>,
    /// TLS mode: disable, prefer, require, verify-ca, verify-full.
    #[arg(long, global = true, default_value = "prefer", value_parser = parse_sslmode)]
    sslmode: SslMode,
    /// Password (for testing; prefer ~/.pgpass in production).
    #[arg(long, global = true)]
    password: Option<String>,
    #[arg(long, global = true)]
    ssl_root_cert: Option<PathBuf>,
    #[arg(long, global = true)]
    ssl_client_cert: Option<PathBuf>,
    #[arg(long, global = true)]
    ssl_client_key: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SslMode {
    Disable,
    Prefer,
    Require,
    VerifyCa,
    VerifyFull,
}

impl std::str::FromStr for SslMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "disable" => Ok(SslMode::Disable),
            "prefer" => Ok(SslMode::Prefer),
            "require" => Ok(SslMode::Require),
            "verify-ca" => Ok(SslMode::VerifyCa),
            "verify-full" => Ok(SslMode::VerifyFull),
            other => Err(format!(
                "unknown sslmode {other:?}; use disable, prefer, require, verify-ca, or verify-full"
            )),
        }
    }
}

fn parse_sslmode(s: &str) -> Result<SslMode, String> {
    s.parse()
}

fn pgpass_lookup(host: &str, port: u16, dbname: &str, user: &str) -> Option<String> {
    let path = std::env::var_os("PGPASSFILE")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".pgpass")))?;
    let metadata = std::fs::metadata(&path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            eprintln!(
                "Ignoring password file {}: group or world access is permitted; use chmod 600",
                path.display()
            );
            return None;
        }
    }
    let content = std::fs::read_to_string(path).ok()?;
    pgpass_password(&content, host, port, dbname, user)
}

// A backslash quotes the next character; only an unescaped colon ends a field.
fn pgpass_field(input: &str) -> (String, Option<&str>) {
    let mut value = String::new();
    let mut chars = input.char_indices();
    while let Some((index, ch)) = chars.next() {
        match ch {
            ':' => return (value, Some(&input[index + 1..])),
            '\\' => value.push(chars.next().map_or('\\', |(_, escaped)| escaped)),
            _ => value.push(ch),
        }
    }
    (value, None)
}

fn pgpass_password(
    content: &str,
    host: &str,
    port: u16,
    dbname: &str,
    user: &str,
) -> Option<String> {
    let port_s = port.to_string();
    'lines: for line in content.split('\n') {
        let line = line.trim_end_matches('\r');
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut rest = line;
        for expected in [host, &port_s, dbname, user] {
            let wildcard = rest.starts_with("*:");
            let (value, next) = pgpass_field(rest);
            let Some(next) = next else {
                continue 'lines;
            };
            if !wildcard && value != expected {
                continue 'lines;
            }
            rest = next;
        }
        return Some(pgpass_field(rest).0);
    }
    None
}

fn connection_config(db: &DbArgs) -> anyhow::Result<tokio_postgres::Config> {
    use tokio_postgres::config::SslMode as DriverSslMode;
    let database = db
        .database
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("--database is required"))?;
    if db.ssl_client_cert.is_some() != db.ssl_client_key.is_some() {
        anyhow::bail!("--ssl-client-cert and --ssl-client-key must be supplied together");
    }
    if db.sslmode == SslMode::Disable
        && (db.ssl_root_cert.is_some() || db.ssl_client_cert.is_some())
    {
        anyhow::bail!("TLS certificate options cannot be used with --sslmode disable");
    }
    let mut config = tokio_postgres::Config::new();
    config
        .host(&db.host)
        .port(db.port)
        .user(&db.user)
        .dbname(database);
    if let Some(password) = db
        .password
        .clone()
        .or_else(|| pgpass_lookup(&db.host, db.port, database, &db.user))
    {
        config.password(password);
    }
    config.ssl_mode(match db.sslmode {
        SslMode::Disable => DriverSslMode::Disable,
        SslMode::Prefer => DriverSslMode::Prefer,
        SslMode::Require | SslMode::VerifyCa | SslMode::VerifyFull => DriverSslMode::Require,
    });
    Ok(config)
}

fn tls_connector(db: &DbArgs) -> anyhow::Result<MakeTlsConnector> {
    let mut builder = TlsConnector::builder();
    match db.sslmode {
        SslMode::Prefer | SslMode::Require => {
            builder
                .danger_accept_invalid_certs(true)
                .danger_accept_invalid_hostnames(true);
        }
        SslMode::VerifyCa => {
            builder.danger_accept_invalid_hostnames(true);
        }
        SslMode::VerifyFull => {}
        SslMode::Disable => anyhow::bail!("TLS is disabled"),
    }
    if let Some(path) = &db.ssl_root_cert {
        builder.disable_built_in_roots(true);
        builder.add_root_certificate(native_tls::Certificate::from_pem(&std::fs::read(path)?)?);
    }
    if let (Some(cert), Some(key)) = (&db.ssl_client_cert, &db.ssl_client_key) {
        builder.identity(native_tls::Identity::from_pkcs8(
            &std::fs::read(cert)?,
            &std::fs::read(key)?,
        )?);
    }
    Ok(MakeTlsConnector::new(builder.build()?))
}

async fn connect(db: &DbArgs) -> anyhow::Result<Client> {
    let config = connection_config(db)?;
    if db.sslmode == SslMode::Disable {
        let (client, connection) = config.connect(NoTls).await?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        Ok(client)
    } else {
        // The driver handles a server declining SSL in Prefer mode. A TLS or
        // authentication error must propagate rather than trigger a plaintext retry.
        let (client, connection) = config.connect(tls_connector(db)?).await?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        Ok(client)
    }
}

fn init_logging(verbose: bool) {
    let level = if verbose { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| level.into()),
        )
        .init();
}

fn require_root(root: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    root.ok_or_else(|| anyhow::anyhow!("--root is required"))
}

/// Reject `--schema` on subcommands that do not act on a single target schema, so the
/// global flag is never silently ignored. `hack fix-checksum` touches the
/// schema-independent canonical row; `hack migrate-from sqlx` records against the
/// connection's own `current_schema()`. Neither honours an override.
/// Require an explicit `--schema` for subcommands that always act on a single named schema
/// (`hack add`, `hack delete`). Unlike `migrate`, these do not fall back to `current_schema()`:
/// the hack interface always names the schema it manipulates.
fn require_schema<'a>(db: &'a DbArgs, subcommand: &str) -> anyhow::Result<&'a str> {
    db.schema.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "--schema is required for '{subcommand}' (it targets a single named schema)"
        )
    })
}

fn reject_schema_override(db: &DbArgs, subcommand: &str) -> anyhow::Result<()> {
    if db.schema.is_some() {
        anyhow::bail!(
            "--schema is not supported for '{subcommand}' (it does not target a single schema); \
             remove --schema (it records against the connection's current_schema())"
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    match args.command {
        // ------------------------------------------------------------------ verify
        Some(Cmd::Verify) => {
            init_logging(args.verbose);
            let root = require_root(args.root)?;
            let migrations = file::load_dir(&root)?;
            let sorted = graph::topo_sort(migrations).map_err(|e| anyhow::anyhow!("{e}"))?;
            println!("OK — {} migration(s) in dependency order:", sorted.len());
            for m in &sorted {
                println!("  {}", m.name);
            }
        }

        // ------------------------------------------------------------------ default: migrate
        None => {
            init_logging(args.verbose);
            let root = require_root(args.root)?;
            let client = connect(&args.db).await?;
            let migrations = file::load_dir(&root)?;
            let report = migrate(
                &mut { client },
                &migrations,
                args.db.schema.as_deref(),
                args.dry_run,
            )
            .await?;
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
                println!(
                    "Already up to date ({} applied).",
                    report.already_applied.len()
                );
            } else {
                println!("Applied {} migration(s):", report.applied.len());
                for name in &report.applied {
                    println!("  {name}");
                }
            }
        }

        // ------------------------------------------------------------------ hack *
        // `action` is boxed (clippy::large_enum_variant); deref once and match.
        Some(Cmd::Hack { action }) => match *action {
            // ------------------------------------------------------------------ hack add
            HackCmd::Add { name, db, verbose } => {
                init_logging(verbose);
                let schema = require_schema(&db, "hack add")?;
                let root = require_root(args.root)?;
                let mut client = connect(&db).await?;
                let migrations = file::load_dir(&root)?;
                hack_add(&mut client, &migrations, &name, schema)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                println!("Recorded {name:?} as applied to schema {schema:?}.");
            }

            // ------------------------------------------------------------------ hack delete
            HackCmd::Delete { name, db, verbose } => {
                init_logging(verbose);
                let schema = require_schema(&db, "hack delete")?;
                let mut client = connect(&db).await?;
                hack_delete(&mut client, &name, schema)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                println!("Removed {name:?} from schema {schema:?} in applied_migrations.");
            }

            // ------------------------------------------------------------------ hack fix-checksum
            HackCmd::FixChecksum { name, db, verbose } => {
                init_logging(verbose);
                reject_schema_override(&db, "hack fix-checksum")?;
                let root = require_root(args.root)?;
                let client = connect(&db).await?;
                let migrations = file::load_dir(&root)?;
                hack_fix_checksum(&client, &migrations, &name)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
                println!("Fixed checksum for {name:?}.");
            }

            // ------------------------------------------------------------------ hack migrate-from sqlx convert
            HackCmd::MigrateFrom {
                source:
                    MigrateFromCmd::Sqlx {
                        phase: SqlxPhaseCmd::Convert { db, verbose },
                    },
            } => {
                init_logging(verbose);
                reject_schema_override(&db, "hack migrate-from sqlx convert")?;
                if args.dry_run {
                    anyhow::bail!("--dry-run is not supported for 'hack migrate-from sqlx convert'; omit it to run for real");
                }
                let root = require_root(args.root)?;
                let client = connect(&db).await?;
                let receipt = sqlx_import::convert(&client, &root)
                    .await
                    .map_err(fmt_sqlx_err)?;
                if receipt.newly_converted == 0 {
                    println!(
                    "Already converted ({} migration(s) already have headers). Nothing written.",
                    receipt.migrations.len(),
                );
                } else {
                    println!(
                        "Converted {} migration(s). Receipt written to: {}",
                        receipt.newly_converted,
                        root.join(".kryzhen-import-receipt.json").display(),
                    );
                    println!("\nNext step: kryzhen hack migrate-from sqlx import");
                }
            }

            // ------------------------------------------------------------------ hack migrate-from sqlx import
            HackCmd::MigrateFrom {
                source:
                    MigrateFromCmd::Sqlx {
                        phase: SqlxPhaseCmd::Import { db, verbose },
                    },
            } => {
                init_logging(verbose);
                reject_schema_override(&db, "hack migrate-from sqlx import")?;
                if args.dry_run {
                    anyhow::bail!("--dry-run is not supported for 'hack migrate-from sqlx import'; omit it to run for real");
                }
                let root = require_root(args.root)?;
                let receipt_path = root.join(sqlx_import::RECEIPT_FILENAME);
                let receipt = sqlx_import::read_receipt(&receipt_path).map_err(fmt_sqlx_err)?;
                let migrations = file::load_dir(&root)?;
                let mut client = connect(&db).await?;
                let receipt = sqlx_import::import(&mut client, &receipt, &migrations)
                    .await
                    .map_err(fmt_sqlx_err)?;
                if receipt.already_imported {
                    println!("Already imported (_sqlx_migrations not present). Nothing to do.");
                } else {
                    println!(
                    "Imported {} migration(s) into mallard.applied_migrations. _sqlx_migrations dropped.",
                    receipt.migrations.len()
                );
                    println!("\nYou can now use `kryzhen migrate` as normal.");
                }
            }

            // ------------------------------------------------------------------ hack migrate-from sqlx all
            HackCmd::MigrateFrom {
                source:
                    MigrateFromCmd::Sqlx {
                        phase: SqlxPhaseCmd::All { db, verbose },
                    },
            } => {
                init_logging(verbose);
                reject_schema_override(&db, "hack migrate-from sqlx all")?;
                if args.dry_run {
                    anyhow::bail!("--dry-run is not supported for 'hack migrate-from sqlx all'; omit it to run for real");
                }
                let root = require_root(args.root)?;
                let mut client = connect(&db).await?;

                print!("Phase 1/2 convert... ");
                let r = sqlx_import::convert(&client, &root)
                    .await
                    .map_err(fmt_sqlx_err)?;
                if r.newly_converted == 0 {
                    println!("already converted ({} migration(s)).", r.migrations.len());
                } else {
                    println!("{} migration(s) converted.", r.newly_converted);
                }

                print!("Phase 2/2 import... ");
                let receipt_path = root.join(sqlx_import::RECEIPT_FILENAME);
                let receipt = sqlx_import::read_receipt(&receipt_path).map_err(fmt_sqlx_err)?;
                let migrations = file::load_dir(&root)?;
                let r = sqlx_import::import(&mut client, &receipt, &migrations)
                    .await
                    .map_err(fmt_sqlx_err)?;
                if r.already_imported {
                    println!("already imported (_sqlx_migrations not present).");
                } else {
                    println!(
                        "{} migration(s) imported. _sqlx_migrations dropped.",
                        r.migrations.len()
                    );
                }

                println!("\nDone. You can now use `kryzhen migrate` as normal.");
            }
        },
    }

    Ok(())
}

fn fmt_sqlx_err(e: SqlxImportError) -> anyhow::Error {
    anyhow::anyhow!("{e}")
}

#[cfg(test)]
mod tests;
