use clap::{Parser, Subcommand};
use kryzhen::sqlx_import::{self, SqlxImportError};
use kryzhen::{file, hack_add, hack_delete, hack_fix_checksum, migrate};
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
    /// Manually manipulate mallard.applied_migrations or migrate from other tools.
    Hack {
        #[command(subcommand)]
        action: HackCmd,
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
    /// TLS mode: disable, prefer, require.
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
}

impl std::str::FromStr for SslMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "disable" => Ok(SslMode::Disable),
            "prefer" => Ok(SslMode::Prefer),
            "require" => Ok(SslMode::Require),
            other => Err(format!(
                "unknown sslmode {other:?}; use disable, prefer, or require"
            )),
        }
    }
}

fn parse_sslmode(s: &str) -> Result<SslMode, String> {
    s.parse()
}

fn pgpass_lookup(host: &str, port: u16, dbname: &str, user: &str) -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let path = std::path::Path::new(&home).join(".pgpass");
    let content = std::fs::read_to_string(path).ok()?;
    let port_s = port.to_string();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.splitn(5, ':').collect();
        if parts.len() != 5 {
            continue;
        }
        let m = |pat: &str, val: &str| pat == "*" || pat == val;
        if m(parts[0], host) && m(parts[1], &port_s) && m(parts[2], dbname) && m(parts[3], user) {
            return Some(parts[4].to_owned());
        }
    }
    None
}

async fn connect(db: &DbArgs) -> anyhow::Result<Client> {
    let database = db
        .database
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("--database is required"))?;

    let password = db
        .password
        .clone()
        .or_else(|| pgpass_lookup(&db.host, db.port, database, &db.user));

    let conn_str = match password {
        Some(pw) => format!(
            "host={} port={} user={} dbname={} password={}",
            db.host, db.port, db.user, database, pw,
        ),
        None => format!(
            "host={} port={} user={} dbname={}",
            db.host, db.port, db.user, database,
        ),
    };

    let client = match db.sslmode {
        SslMode::Disable => {
            let conn_str = format!("{conn_str} sslmode=disable");
            let (client, conn) = tokio_postgres::connect(&conn_str, NoTls).await?;
            tokio::spawn(async move {
                let _ = conn.await;
            });
            client
        }
        SslMode::Prefer => {
            let conn_str_tls = format!("{conn_str} sslmode=prefer");
            let builder = TlsConnector::builder()
                .danger_accept_invalid_certs(true)
                .build()?;
            let connector = MakeTlsConnector::new(builder);
            match tokio_postgres::connect(&conn_str_tls, connector).await {
                Ok((client, conn)) => {
                    tokio::spawn(async move {
                        let _ = conn.await;
                    });
                    client
                }
                Err(_) => {
                    let conn_str_notls = format!("{conn_str} sslmode=disable");
                    let (client, conn) = tokio_postgres::connect(&conn_str_notls, NoTls).await?;
                    tokio::spawn(async move {
                        let _ = conn.await;
                    });
                    client
                }
            }
        }
        SslMode::Require => {
            let conn_str = format!("{conn_str} sslmode=require");
            let builder = TlsConnector::builder()
                .danger_accept_invalid_certs(true)
                .build()?;
            let connector = MakeTlsConnector::new(builder);
            let (client, conn) = tokio_postgres::connect(&conn_str, connector).await?;
            tokio::spawn(async move {
                let _ = conn.await;
            });
            client
        }
    };
    Ok(client)
}

// ---------------------------------------------------------------------------
// Logging
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    match args.command {
        // ------------------------------------------------------------------ default: migrate
        None => {
            init_logging(args.verbose);
            let root = require_root(args.root)?;
            let client = connect(&args.db).await?;
            let migrations = file::load_dir(&root)?;
            let report = migrate(&mut { client }, &migrations, args.dry_run).await?;
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

        // ------------------------------------------------------------------ hack add
        Some(Cmd::Hack {
            action: HackCmd::Add { name, db, verbose },
        }) => {
            init_logging(verbose);
            let root = require_root(args.root)?;
            let client = connect(&db).await?;
            let migrations = file::load_dir(&root)?;
            hack_add(&client, &migrations, &name)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            println!("Recorded {name:?} as applied.");
        }

        // ------------------------------------------------------------------ hack delete
        Some(Cmd::Hack {
            action: HackCmd::Delete { name, db, verbose },
        }) => {
            init_logging(verbose);
            let client = connect(&db).await?;
            hack_delete(&client, &name)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            println!("Removed {name:?} from applied_migrations.");
        }

        // ------------------------------------------------------------------ hack fix-checksum
        Some(Cmd::Hack {
            action: HackCmd::FixChecksum { name, db, verbose },
        }) => {
            init_logging(verbose);
            let root = require_root(args.root)?;
            let client = connect(&db).await?;
            let migrations = file::load_dir(&root)?;
            hack_fix_checksum(&client, &migrations, &name)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            println!("Fixed checksum for {name:?}.");
        }

        // ------------------------------------------------------------------ hack migrate-from sqlx convert
        Some(Cmd::Hack {
            action:
                HackCmd::MigrateFrom {
                    source:
                        MigrateFromCmd::Sqlx {
                            phase: SqlxPhaseCmd::Convert { db, verbose },
                        },
                },
        }) => {
            init_logging(verbose);
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
        Some(Cmd::Hack {
            action:
                HackCmd::MigrateFrom {
                    source:
                        MigrateFromCmd::Sqlx {
                            phase: SqlxPhaseCmd::Import { db, verbose },
                        },
                },
        }) => {
            init_logging(verbose);
            if args.dry_run {
                anyhow::bail!("--dry-run is not supported for 'hack migrate-from sqlx import'; omit it to run for real");
            }
            let root = require_root(args.root)?;
            let client = connect(&db).await?;
            let receipt = sqlx_import::import(&client, &root)
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
        Some(Cmd::Hack {
            action:
                HackCmd::MigrateFrom {
                    source:
                        MigrateFromCmd::Sqlx {
                            phase: SqlxPhaseCmd::All { db, verbose },
                        },
                },
        }) => {
            init_logging(verbose);
            if args.dry_run {
                anyhow::bail!("--dry-run is not supported for 'hack migrate-from sqlx all'; omit it to run for real");
            }
            let root = require_root(args.root)?;
            let client = connect(&db).await?;

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
            let r = sqlx_import::import(&client, &root)
                .await
                .map_err(fmt_sqlx_err)?;
            if r.already_imported {
                println!("already imported (_sqlx_migrations not present).");
            } else {
                println!("{} migration(s) imported. _sqlx_migrations dropped.", r.migrations.len());
            }

            println!("\nDone. You can now use `kryzhen migrate` as normal.");
        }
    }

    Ok(())
}

fn fmt_sqlx_err(e: SqlxImportError) -> anyhow::Error {
    anyhow::anyhow!("{e}")
}
