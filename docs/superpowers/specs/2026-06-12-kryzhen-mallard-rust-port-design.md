# kryzhen — Rust port of mallard

**Status:** Design / spec
**Date:** 2026-06-12

A forward-only, dependency-resolved SQL migration tool for PostgreSQL, ported from
the Haskell package [`mallard`](https://hackage.haskell.org/package/mallard). Shipped
as both a reusable **library crate** and a **CLI binary**. Faithful to mallard's model
and on-disk/in-database formats, with a few modern conveniences and two deliberate
behavioral additions (see §9).

---

## 1. Goals & non-goals

**Goals**
- Read and write databases managed by stock mallard (same `mallard` schema, same
  `applied_migrations` table layout, same checksum scheme).
- Parse mallard's migration file format (`#!migration` headers in contiguous SQL
  comments; multiple blocks per file).
- Resolve migration dependencies into a topological order; reject cycles.
- Apply pending migrations transactionally, forward-only (no down-migrations).
- Detect tampering: refuse to run if a previously-applied migration's content changed.
- Usable both as a library (`kryzhen`) and a CLI (`kryzhen-cli`).

**Non-goals**
- No `#!test` support. The `#!test` directive is **not** recognized — encountering one
  is a parse error (the user does not need tests).
- No down/rollback migrations (matches mallard's accretive model).
- No databases other than PostgreSQL.

---

## 2. Workspace layout

A Cargo workspace with two crates:

- **`kryzhen`** (library) — all logic: types, parsing, graph, validation, postgres
  applier. No `main`, no CLI dependencies. Public async API.
- **`kryzhen-cli`** (binary, produces the `kryzhen` executable) — thin `clap` wrapper
  that parses flags and calls the library. Depends on `kryzhen`.

Standard Rust library + CLI split: the library is reusable and testable in isolation;
the binary is a thin adapter.

---

## 3. Library modules

| Module       | Responsibility |
|--------------|----------------|
| `types`      | Core structs (`Migration`, `MigrationName`) and the error enum (`thiserror`). |
| `parser`     | Parse one file's `#!migration` blocks: split on `#!`, parse each header (`name`, `description`, `requires`), capture the SQL body. Preserves file order. |
| `file`       | Walk the root directory, read `.sql` files, hand each to the parser → `Vec<Migration>`. Applies the implicit in-file linear dependency (§9.2). |
| `graph`      | Build the dependency graph from `requires`, detect cycles, topological sort → ordered apply list. |
| `validation` | Duplicate-name detection, dangling-`requires` detection, checksum verification vs. the DB. |
| `postgres`   | Ensure the `mallard` schema/table exist, read applied migrations, apply pending ones (each in a transaction). |
| `lib.rs`     | Public entry point `migrate(config) -> Result<Report>` tying it all together; re-exports public types. |

---

## 4. Core data types

```rust
pub struct MigrationName(pub String);

pub struct Migration {
    pub name: MigrationName,
    pub description: String,
    /// Explicit `requires` from the header PLUS the implicit in-file predecessor (§9.2),
    /// in that merged form. This is what gets persisted to the `requires` column.
    pub requires: Vec<MigrationName>,
    /// The SQL body of this block, with leading/trailing whitespace trimmed (§6).
    pub script: String,
    /// SHA-256 of `script` (the trimmed body). 32 raw bytes.
    pub checksum: [u8; 32],
}
```

`Report` summarizes a run: which migrations were already applied, which were applied
this run (in order), and (for `--dry-run`) which would be applied.

---

## 5. Migration file format (parsing)

Faithful to mallard:

```sql
-- #!migration
-- name: "tables/phone",
-- description: "Phone numbers attached to a person.",
-- requires: ["tables/person"];
CREATE TABLE phone ( ... );
```

Rules (from mallard):
- Header lives in contiguous SQL line comments at the start of a block.
- A block begins with `#!migration`. (`#!test` is **rejected** — parse error.)
- Header fields are separated by `,`; the final field ends with `;`.
- Fields: `name` (string), `description` (string), `requires` (string or array of
  strings; optional — absent means no explicit deps).
- A single file may contain **multiple blocks**, each starting at the next `#!`.
  Blocks are captured in file order.
- The block's SQL body is everything after the header `;` up to the next `#!` or EOF.

Implementation note: the parser is hand-written (small, explicit) or `nom`-based —
to be decided in the plan. Either way it must preserve in-file block order so the
implicit linear dependency (§9.2) is well-defined.

---

## 6. Checksum

- **Checksum = SHA-256 over the migration's SQL body with leading and trailing
  whitespace trimmed**, UTF-8 encoded. Stored as 32 raw bytes in a `bytea` column.
- This matches mallard: mallard's parser consumes surrounding whitespace via its
  `symbol`/`spaceConsumer` combinators before hashing the body, so the hashed content
  is effectively the trimmed body. We compute it explicitly as `body.trim()` hashed
  with SHA-256.
- `script_text` persisted to the DB is the same trimmed body.

---

## 7. Tracking table (mallard-compatible)

On first run, create if absent (idempotent):

```sql
CREATE SCHEMA IF NOT EXISTS mallard;

CREATE TABLE IF NOT EXISTS mallard.applied_migrations(
    id           bigserial    NOT NULL,
    name         text         NOT NULL,
    description  text         NOT NULL,
    requires     text[]       NOT NULL,
    checksum     bytea        NOT NULL,
    script_text  text         NOT NULL,
    applied_on   timestamptz  NOT NULL DEFAULT now(),
    PRIMARY KEY (id)
);
```

This is mallard's exact schema and table name, so kryzhen reads and writes databases
that stock mallard already manages.

- **SELECT** `name, description, requires, checksum, script_text FROM
  mallard.applied_migrations` to read the applied set.
- **INSERT** `(name, description, requires, checksum, script_text)` for each newly
  applied migration (mallard's exact column set; `id`/`applied_on` defaulted).

---

## 8. Data flow & application

```
root dir ──file──▶ Vec<Migration> (per-file blocks, file order preserved)
                          │  file layer adds implicit in-file linear deps (§9.2)
                          ▼
                   graph: merge explicit+implicit requires,
                          topological sort, cycle check
                          │
                          ▼
              validation: duplicate names, dangling requires,
                          checksum vs. mallard.applied_migrations
                          │
                          ▼
        postgres: ensure schema/table; load applied set;
                  for each PENDING migration in topo order:
                      BEGIN;
                      run its SQL body;
                      INSERT into mallard.applied_migrations;
                      COMMIT;
```

**Transactions & safety**
- Each migration applies in its own transaction; the bookkeeping `INSERT` is part of
  the same transaction, so migration + record commit atomically.
- A failing migration rolls back and aborts the whole run with a non-zero exit. Earlier
  committed migrations stay applied (forward-only).
- **Tamper detection:** for every migration already present in the table, recompute the
  checksum from disk and compare to the stored `checksum`; any mismatch aborts the run
  with `ChecksumMismatch` before applying anything.

---

## 9. Deliberate additions / divergences from mallard

### 9.1 No tests
`#!test` is unsupported. A `#!test` directive is a parse error. This narrows mallard's
feature set by user request.

### 9.2 Implicit in-file linear dependencies (new)
Within a single file containing blocks `[A, B, C]` in file order, each block implicitly
`requires` the block immediately preceding it **in that file**, in addition to any
explicitly declared `requires`. So the effective deps are:
- `A`: explicit only (first block, no implicit predecessor).
- `B`: explicit ∪ `{A}`.
- `C`: explicit ∪ `{B}`.

The implicit edge references the *previous block's name* (file order, not name sort),
so block order in the file defines "previous". The merged (explicit ∪ implicit)
requirement set:
- participates in the topo-sort and cycle check identically to explicit deps, and
- **is persisted to the `requires` column** of `mallard.applied_migrations` (the stored
  value is the merged set, which can differ from what stock mallard would record for the
  same file).

---

## 10. CLI (`kryzhen`)

Faithful to mallard's flags, plus modern polish:

```
kryzhen --database <DB> [--host 127.0.0.1] [--port 5432]
        [--user postgres] [--password ""] [ROOT_DIR]

  --database ARG   Database name (required).
  --host ARG       Server host (default: 127.0.0.1).
  --port ARG       Server port (default: 5432).
  --user ARG       Username (default: postgres).
  --password ARG   Password (default: "").
  --dry-run        Print the planned migration order; apply nothing.
  -v, --verbose    Structured logging (tracing).
  ROOT_DIR         Root directory of the migration tree (positional).
```

The `-t/--test` flag from mallard is **removed** (tests dropped).

---

## 11. Async model

`tokio` runtime + `tokio-postgres`. The library's public API is `async` (e.g.
`pub async fn migrate(...)`). The CLI wraps it in `#[tokio::main]`. Library consumers
in a sync context use their own runtime / `block_on`.

---

## 12. Error handling

- Library: a typed error enum via `thiserror`:
  `ParseError`, `CycleError`, `DuplicateName`, `DanglingDependency`, `ChecksumMismatch`,
  `Db`, `Io`. Each carries enough context to point at the offending file/migration.
- CLI: surfaces library errors with context and a non-zero exit code; `--verbose`
  raises log detail via `tracing`.

---

## 13. Testing

- **Unit:** parser (single/multiple blocks, malformed headers, `#!test` rejected,
  body trimming), graph (topo order, cycle detection, implicit-dep edges), validation
  (duplicates, dangling requires, checksum mismatch).
- **Integration:** the postgres applier against a real PostgreSQL — table creation,
  apply ordering, idempotent re-run, tamper detection, and reading a database written
  by stock mallard. Mechanism (`testcontainers` vs. a dev DB) decided in the plan.

---

## 14. Open items for the implementation plan

- Parser implementation choice: hand-written vs. `nom`.
- Integration-test database mechanism: `testcontainers` vs. external dev DB.
- Crate/binary naming: keep `kryzhen` (working name) or rename.
