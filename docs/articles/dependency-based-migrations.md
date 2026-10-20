# Kryzhen PostgreSQL migrations with dependencies and multiple schemas

Database migrations have an order because schema objects depend on one another.
A table with a foreign key needs the referenced table to exist. Several modules
may evolve independently, and several tenant schemas may need the same changes
at different times.

[Kryzhen](https://github.com/kamysh/kryzhen) represents those dependencies in SQL
migration headers. It provides a Rust library and a CLI for forward-only
PostgreSQL migrations, with independent tracking for each target schema.

It is a Rust port of the Haskell
[mallard](https://github.com/AndrewRademacher/mallard) migration tool, using its
migration format and `mallard.applied_migrations` tracking table. Kryzhen adds
per-schema tracking alongside that canonical migration history.

## Declaring dependencies in SQL

Consider an application with accounts and account events. Save the first
migration in `migrations/accounts.sql`:

```sql
-- #!migration
-- name: "tables/account",
-- description: "Accounts belonging to one tenant.";
CREATE TABLE account (
    id bigint PRIMARY KEY
);
```

Save the dependent migration in `migrations/events.sql`:

```sql
-- #!migration
-- name: "tables/account_event",
-- description: "Events associated with an account.",
-- requires: ["tables/account"];
CREATE TABLE account_event (
    id bigint PRIMARY KEY,
    account_id bigint NOT NULL REFERENCES account(id)
);
```

The header states the ordering requirement. Kryzhen topologically sorts the
migrations before applying them, rejects missing dependencies and cycles, and
rejects duplicate migration names. Independent ready migrations are ordered
deterministically by name.

A file can contain several migration blocks. Each later block implicitly
depends on the preceding block in that file, in addition to its explicit
dependencies. Separate files make the cross-file dependency in this example
visible.

## Applying the same migrations to two schemas

With the CLI installed and a PostgreSQL database prepared for this example, the
same files can be applied to two target schemas:

```sh
kryzhen --database example --schema tenant_a --root migrations/
kryzhen --database example --schema tenant_b --root migrations/
```

The connection's credentials and permissions must allow the operations. The
[installation and connection documentation](https://github.com/kamysh/kryzhen#installation)
covers binary downloads and connection configuration.

Within each migration transaction, Kryzhen creates the target schema if needed
and sets the local search path to that schema. The unqualified `account` and
`account_event` tables therefore belong to the selected tenant schema. Explicitly
schema-qualified SQL still refers to the schema it names.

Tracking records which migrations have reached each schema. Running the command
again skips migrations already applied there. A newly added tenant schema can
receive the complete migration set while an existing schema receives only its
pending migrations.

An omitted `--schema` targets the connection's `current_schema()`. SQL that needs
an object in another schema, including an extension function, should qualify
that reference: the migration's local search path contains the target schema.

## Transactions and checksums

Each migration's SQL and bookkeeping commit in one transaction. If it fails,
that migration rolls back and the run stops. Earlier migrations may already
have committed. A run spanning several migrations or several tenant schemas is
not one global transaction.

Kryzhen stores a SHA-256 checksum of each applied SQL body, trimmed at its ends.
Before applying pending work, it checks the bodies of already-applied migrations
against their stored checksums. Changing the SQL body of an applied migration
causes an error; create a new forward migration for a later change.

The checksum does not cover the complete file or migration metadata. It is not
a cryptographic signature or a complete audit of changes to the header.

A dry run checks the plan and existing body checksums without creating or
upgrading tracking tables or applying migration SQL:

```sh
kryzhen --database example --schema tenant_a --root migrations/ --dry-run
```

The
[migration implementation](https://github.com/kamysh/kryzhen/blob/d8a0c6cbbb5a6ed040945a82373d9c433c88df34/kryzhen/src/lib.rs)
shows the validation, ordering and per-schema skip logic. The
[PostgreSQL application code](https://github.com/kamysh/kryzhen/blob/d8a0c6cbbb5a6ed040945a82373d9c433c88df34/kryzhen/src/postgres.rs)
keeps each migration and its bookkeeping together.

## Embedding Kryzhen in a Rust application

The library accepts a PostgreSQL client, parsed migrations, an optional target
schema and a dry-run flag. Applications can load migrations from files or parse
embedded SQL, then use the same dependency and tracking behavior through the
library. The repository contains a
[complete library example](https://github.com/kamysh/kryzhen#library-usage).

The current documented library installation uses a versioned Git dependency.
Prebuilt CLI binaries are documented for Linux x86_64, Linux ARM64 and Apple
Silicon. Kryzhen uses Apache 2.0; contributions are subject to the repository's
CLA.

[Muninn](https://github.com/kamysh/muninn) and
[Mimir](https://github.com/kamysh/mimir) use Kryzhen for their own database
migrations. The migration library and CLI can also be used independently.

This article was drafted by an AI assistant from the project's source code and
documentation. The account and tenant example is illustrative.
