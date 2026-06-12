use clap::Parser;
use kryzhen::{migrate, Config, SslMode};
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

    /// TLS negotiation: disable, prefer, require, verify-ca, or verify-full.
    #[arg(long, default_value = "prefer", value_parser = parse_sslmode)]
    sslmode: SslMode,

    /// CA certificate for verify-ca / verify-full (PEM file path).
    #[arg(long)]
    ssl_root_cert: Option<std::path::PathBuf>,

    /// Client certificate for mutual TLS (PEM file path).
    #[arg(long)]
    ssl_client_cert: Option<std::path::PathBuf>,

    /// Client private key for mutual TLS (PEM file path).
    #[arg(long)]
    ssl_client_key: Option<std::path::PathBuf>,

    /// Print the planned migration order; apply nothing.
    #[arg(long)]
    dry_run: bool,

    /// Verbose logging.
    #[arg(short, long)]
    verbose: bool,
}

/// clap value parser for `--sslmode`, delegating to [`SslMode`]'s `FromStr`.
fn parse_sslmode(s: &str) -> Result<SslMode, String> {
    s.parse()
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
        sslmode: args.sslmode,
        ssl_root_cert: args.ssl_root_cert,
        ssl_client_cert: args.ssl_client_cert,
        ssl_client_key: args.ssl_client_key,
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
