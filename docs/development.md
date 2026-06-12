# Development

How to build, test, and contribute to kryzhen.

## Prerequisites

- A stable **Rust toolchain** (edition 2021), e.g. via [rustup](https://rustup.rs/) or
  your system package manager. kryzhen is a standard Cargo project — `cargo build`,
  `cargo test`, etc. work as usual. The crates depend only on pure-Rust libraries
  (`tokio-postgres` with `NoTls`, `sha2`, `walkdir`, `clap`, `tracing`), so no system C
  libraries are needed.
- [Docker](https://www.docker.com/) — required only to run the integration tests, which
  spin up a throwaway PostgreSQL container via
  [testcontainers](https://docs.rs/testcontainers).

All commands below assume `cargo` is on your `PATH`.

## Optional: Nix dev shell

The repository also ships a `flake.nix` that provides a pinned Rust toolchain, if you
prefer reproducible builds with Nix. It is entirely optional — rustup works equally
well. To use it:

```bash
nix develop                                              # interactive shell
nix develop --command bash -c "cargo build --workspace"  # one-off command
```

> Nix flakes only see git-tracked files, so if you use the flake, `git add` any new
> file the build needs (e.g. a new source file or fixture) before building.

## Build

```bash
cargo build --workspace
```

## Test

The unit tests are pure (no database) and run anywhere:

```bash
cargo test --workspace --lib
```

The integration tests under `kryzhen/tests/applier.rs` apply migrations against a real
PostgreSQL started by testcontainers, so they require Docker. The full suite (unit +
integration) is:

```bash
cargo test --workspace
```

The integration tests include a run against mallard's own `example-contacts` migration
tree (vendored verbatim under `kryzhen/tests/fixtures/mallard-example-contacts/`),
which exercises real-world dependency resolution, the multiple-blocks-per-file path,
and `#!test` rejection.

## Lint and format

Both must pass before committing (CI parity):

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

To auto-format:

```bash
cargo fmt --all
```

New code should match the default `rustfmt` formatting. Fix clippy findings with real
changes rather than `#[allow]` attributes.

## API documentation

```bash
cargo doc --no-deps --open
```

The public API is documented with rustdoc, including runnable examples (doc-tests).
Doc-tests run as part of `cargo test`.

## Project layout

```
kryzhen/                library crate
  src/lib.rs            public API: migrate(), Config, Report
  src/types.rs          MigrationName, Migration, Error, checksum()
  src/parser.rs         parse a file's #!migration blocks
  src/file.rs           directory walk + implicit in-file linear deps
  src/graph.rs          topological sort + cycle/dangling detection
  src/validation.rs     duplicate names + checksum tamper detection
  src/postgres.rs       mallard-compatible applier
  tests/applier.rs      integration tests (testcontainers)
  tests/fixtures/       vendored mallard example migrations
kryzhen-cli/            binary crate (produces the `kryzhen` executable)
docs/                   design spec, implementation plan, this guide
```

## Contributing

Contributions are welcome. By submitting a pull request you agree to the
[Contributor License Agreement](../CLA.md). The project is published under the
[Apache License 2.0](../LICENSE).

Before opening a PR, please make sure `cargo fmt --all -- --check`,
`cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace`
all pass.
