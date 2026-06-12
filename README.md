# kryzhen

A Rust port of the Haskell [`mallard`](https://hackage.haskell.org/package/mallard) SQL migration tool ([source](https://github.com/AndrewRademacher/mallard)). Forward-only (accretive — no down-migrations), dependency-resolved migrations for PostgreSQL. Compatible with mallard's on-disk migration file format and its `mallard.applied_migrations` tracking table.

Available as:

- **library crate** `kryzhen` — call `migrate()` programmatically.
- **CLI binary** `kryzhen` (crate `kryzhen-cli`) — run migrations from the command line.

---

## Migration file format

Migration files are plain `.sql` files. Each file may contain one or more `#!migration` blocks. A block begins with a header comment and is followed by the SQL body:

```sql
-- #!migration
-- name: "tables/phone",
-- description: "Phone numbers attached to a person.",
-- requires: ["tables/person"];
CREATE TABLE phone (id bigint);
```

### Header fields

| Field | Required | Type | Notes |
|---|---|---|---|
| `name` | yes | string | Unique identifier for this migration. |
| `description` | yes | string | Human-readable description. |
| `requires` | no | string or list | A single `"name"` or a list `["a", "b"]`. |

Header fields are separated by commas; the last field is terminated with `;`. The SQL body follows directly after the header.

> **Note:** `#!test` blocks are **not** supported — kryzhen handles migrations only, and a `#!test` block is a parse error.

### Multiple blocks per file + implicit linear dependency

A single `.sql` file may contain several `#!migration` blocks. Each block after the first **implicitly depends on the block immediately before it** in the same file, in addition to any explicit `requires`. This merged requires set is what gets persisted to the tracking table.

---

## Dependency resolution

Migrations are applied in **topological order** of their `requires` graph. The following are hard errors:

- Dependency cycles.
- A `requires` reference to a migration name that does not exist.
- Duplicate migration names (across all files).

---

## Tracking table

kryzhen creates a `mallard` schema (if absent) with an `applied_migrations` table:

| Column | Description |
|---|---|
| `id` | Serial primary key. |
| `name` | Migration name. |
| `description` | Migration description. |
| `requires` | Dependencies recorded at apply time. |
| `checksum` | SHA-256 of the whitespace-trimmed SQL body (`bytea`, 32 raw bytes). |
| `script_text` | Full SQL body as applied. |
| `applied_on` | Timestamp of application. |

This is the same schema used by mallard, so the two tools can share a database.

---

## Tamper detection

On every run, kryzhen recomputes the SHA-256 checksum of each already-applied migration's SQL body and **aborts if it differs** from the stored value. Do not edit a migration file after it has been applied.

---

## Atomicity and idempotency

Each migration runs inside its own transaction together with its bookkeeping `INSERT` into `applied_migrations`. A failure aborts the run. Already-applied migrations are skipped on re-run, so running kryzhen multiple times is safe.

---

## Library usage

The preferred way to construct a `Config` is through the builder:

```rust
use kryzhen::{migrate, Config, SslMode};
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let report = migrate(
        Config::builder(PathBuf::from("migrations"), "127.0.0.1", 5432, "postgres", "secret", "mydb")
            .sslmode(SslMode::Require)
            .build(),
    )
    .await?;

    println!("Applied:         {:?}", report.applied);
    println!("Already applied: {:?}", report.already_applied);
    Ok(())
}
```

Struct literal construction is also supported for all fields.

### `Config` fields

| Field | Type | Default | Description |
|---|---|---|---|
| `root` | `PathBuf` | *(required)* | Root directory of the migration tree. |
| `host` | `String` | *(required)* | PostgreSQL host. |
| `port` | `u16` | *(required)* | PostgreSQL port. |
| `user` | `String` | *(required)* | Database user. |
| `password` | `String` | *(required)* | Database password (may be empty). |
| `database` | `String` | *(required)* | Database name. |
| `sslmode` | `SslMode` | `Prefer` | TLS negotiation mode. See [TLS](#tls). |
| `ssl_root_cert` | `Option<PathBuf>` | `None` | CA certificate for `verify-ca` / `verify-full` (PEM). |
| `ssl_client_cert` | `Option<PathBuf>` | `None` | Client certificate for mutual TLS (PEM). |
| `ssl_client_key` | `Option<PathBuf>` | `None` | Client private key for mutual TLS (PEM). |
| `dry_run` | `bool` | `false` | If `true`, resolve and plan but apply nothing. Still connects to load applied migrations and verify checksums. |

### `ConfigBuilder` methods

| Method | Description |
|---|---|
| `Config::builder(root, host, port, user, password, database)` | Create a builder with required fields. |
| `.sslmode(SslMode)` | Set the TLS negotiation mode (default: `Prefer`). |
| `.ssl_root_cert(path)` | Set the CA certificate path for `verify-ca` / `verify-full`. |
| `.ssl_client_cert(path)` | Set the client certificate path for mutual TLS. |
| `.ssl_client_key(path)` | Set the client private key path for mutual TLS. |
| `.dry_run(bool)` | Enable or disable dry-run mode. |
| `.build()` | Consume the builder and return a `Config`. |

### `Report` fields

| Field | Type | Description |
|---|---|---|
| `applied` | `Vec<String>` | Names applied this run (or planned, in dry-run mode), in topological order. |
| `already_applied` | `Vec<String>` | Names that were already present before this run. |

---

## CLI usage

```
kryzhen --database mydb [ROOT]
```

`ROOT` defaults to `.` (current directory).

### Flags

| Flag | Default | Description |
|---|---|---|
| `--database <DATABASE>` | *(required)* | Database name. |
| `--host <HOST>` | `127.0.0.1` | Server host. |
| `--port <PORT>` | `5432` | Server port. |
| `--user <USER>` | `postgres` | Username. |
| `--password <PASSWORD>` | *(empty)* | Password. |
| `--sslmode <MODE>` | `prefer` | TLS mode: `disable`, `prefer`, `require`, `verify-ca`, or `verify-full`. See [TLS](#tls). |
| `--ssl-root-cert <PATH>` | *(none)* | CA certificate for `verify-ca` / `verify-full` (PEM). |
| `--ssl-client-cert <PATH>` | *(none)* | Client certificate for mutual TLS (PEM). |
| `--ssl-client-key <PATH>` | *(none)* | Client private key for mutual TLS (PEM). |
| `--dry-run` | off | Print the planned migration order; apply nothing. |
| `-v, --verbose` | off | Enable debug-level logging. |
| `-h, --help` | | Print help. |
| `-V, --version` | | Print version. |

### Example — dry run

```
kryzhen --database mydb --dry-run migrations/
```

Prints the migrations that would be applied, in topological order, without modifying the database. (It still connects, to load the applied set and verify checksums.)

---

## TLS

kryzhen negotiates TLS using the standard libpq `sslmode` values:

| Mode | Behaviour |
|---|---|
| `disable` | Never use TLS; connect in plaintext. |
| `prefer` *(default)* | Use TLS if the server offers it; fall back to plaintext otherwise. |
| `require` | Require TLS; fail if the server does not offer it. Certificate not verified. |
| `verify-ca` | Require TLS and verify the server certificate against `ssl_root_cert`. |
| `verify-full` | Like `verify-ca`, and also verify the server hostname matches the certificate CN/SAN. |

In `prefer` and `require` the connection is **encrypted but the server certificate is not verified** — matching libpq's behaviour for those modes. This lets kryzhen connect to a database using a self-signed or private-CA certificate (common for internal PostgreSQL deployments).

For `verify-ca` and `verify-full`, set `ssl_root_cert` (library) or `--ssl-root-cert` (CLI) to the CA certificate PEM file.

### Mutual TLS

To present a client certificate to the server (for PostgreSQL `clientcert=verify-full` authentication), set both `ssl_client_cert` and `ssl_client_key` (library) or `--ssl-client-cert` and `--ssl-client-key` (CLI). Mutual TLS can be combined with any sslmode that establishes a TLS connection (`prefer`, `require`, `verify-ca`, `verify-full`).

TLS uses OpenSSL (via `native-tls`), so the build needs OpenSSL and `pkg-config` available — see [docs/development.md](docs/development.md).

---

## Development

See [docs/development.md](docs/development.md) for how to build, test, and contribute.
API documentation is available via `cargo doc --open`.

---

## License

Apache License 2.0 — see [LICENSE](LICENSE).
Contributions are subject to the [Contributor License Agreement](CLA.md).
