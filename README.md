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

```rust
use kryzhen::{migrate, Config};
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let report = kryzhen::migrate(Config {
        root: PathBuf::from("migrations"),
        host: "127.0.0.1".into(),
        port: 5432,
        user: "postgres".into(),
        password: "secret".into(),
        database: "mydb".into(),
        dry_run: false,
    })
    .await?;

    println!("Applied:         {:?}", report.applied);
    println!("Already applied: {:?}", report.already_applied);
    Ok(())
}
```

### `Config` fields

| Field | Type | Description |
|---|---|---|
| `root` | `PathBuf` | Root directory of the migration tree. |
| `host` | `String` | PostgreSQL host. |
| `port` | `u16` | PostgreSQL port. |
| `user` | `String` | Database user. |
| `password` | `String` | Database password. |
| `database` | `String` | Database name. |
| `dry_run` | `bool` | If `true`, resolve and plan but apply nothing. Still connects to the database to load applied migrations and verify checksums. |

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

## Development

See [docs/development.md](docs/development.md) for how to build, test, and contribute.
API documentation is available via `cargo doc --open`.

---

## License

Apache License 2.0 — see [LICENSE](LICENSE).
Contributions are subject to the [Contributor License Agreement](CLA.md).
