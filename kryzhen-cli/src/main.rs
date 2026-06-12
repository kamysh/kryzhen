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

    Ok(())
}
