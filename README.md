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

The caller owns the database connection. Build it with `tokio_postgres` and pass it to
`migrate`:

```rust
use kryzhen::{file, migrate, Report};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let (mut client, conn) = tokio_postgres::connect(
        "host=127.0.0.1 user=postgres dbname=mydb",
        tokio_postgres::NoTls,
    ).await?;
    tokio::spawn(async move { let _ = conn.await; });

    let migrations = file::load_dir("migrations")?;
    let report: Report = migrate(&mut client, &migrations, false).await?;

    println!("Applied:         {:?}", report.applied);
    println!("Already applied: {:?}", report.already_applied);
    Ok(())
}
```

Pass `dry_run = true` to resolve and plan without applying anything (still connects and
verifies checksums).

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

## Escape hatches

The `kryzhen hack` subcommand provides low-level manipulation of `mallard.applied_migrations` for situations where normal migration flow isn't sufficient.

### `hack add` — record a migration as applied without running its SQL

```bash
kryzhen hack add <NAME> --root migrations/ --database mydb
```

Inserts a row for `NAME` into `mallard.applied_migrations` without executing the SQL body. Useful when a migration has already been applied by other means (e.g. provisioned by Terraform, applied by another tool). Fails if any of the migration's `requires` are not yet applied.

### `hack delete` — remove a migration record without running any SQL

```bash
kryzhen hack delete <NAME> --database mydb
```

Removes the `NAME` row from `mallard.applied_migrations`. Fails if any currently-applied migration lists `NAME` in its `requires`.

### `hack fix-checksum` — update a stored checksum to match the current file

```bash
kryzhen hack fix-checksum <NAME> --root migrations/ --database mydb
```

Recomputes the checksum from the current on-disk content of migration `NAME` and overwrites the stored value in `mallard.applied_migrations`. Use this when a migration file was legitimately edited after it was applied (e.g. reformatted, whitespace-only change) and tamper detection is now blocking normal runs with:

```
checksum mismatch for already-applied migration [NAME]: file content changed — run `kryzhen hack fix-checksum NAME` to update
```

> **Warning:** `hack fix-checksum` bypasses tamper detection. Only use it when you are certain the file change was intentional and harmless.

---

## Migrating from sqlx

If you have an existing project that uses [sqlx migrations](https://docs.rs/sqlx/latest/sqlx/migrate/index.html), `kryzhen hack migrate-from sqlx` imports your applied migration history into `mallard.applied_migrations` in two phases:

```bash
# Phase 1: verify checksums, rewrite files with #!migration headers, write receipt
kryzhen hack migrate-from sqlx convert [MIGRATIONS_DIR] --database mydb

# Phase 2: re-verify _sqlx_migrations against the receipt, insert into mallard
kryzhen hack migrate-from sqlx import [MIGRATIONS_DIR] --database mydb

# Or run both phases in one command:
kryzhen hack migrate-from sqlx all [MIGRATIONS_DIR] --database mydb
```

`MIGRATIONS_DIR` defaults to `.`. The `_sqlx_migrations` table is never modified.

After a successful import you can use `kryzhen migrate` as normal. sqlx should no longer manage those migrations.

### What each phase does

| Phase | Input | Output |
|---|---|---|
| `convert` | `_sqlx_migrations` rows + original files | Files rewritten with `#!migration` headers; `.kryzhen-import-receipt.json` written |
| `import` | Receipt + `_sqlx_migrations` (re-verified) + converted files | Rows inserted into `mallard.applied_migrations` |

### Team workflow

After Person A runs `convert` and `import`, commit both the rewritten `.sql` files and `.kryzhen-import-receipt.json` to git. Teammates who sync the repo run only `import` — it re-verifies their `_sqlx_migrations` against the receipt and inserts any missing rows, skipping rows already present.

---

## Development

See [docs/development.md](docs/development.md) for how to build, test, and contribute.
API documentation is available via `cargo doc --open`.

---

## License

Apache License 2.0 — see [LICENSE](LICENSE).
Contributions are subject to the [Contributor License Agreement](CLA.md).
