{
  description = "kryzhen: forward-only, dependency-resolved SQL migrations for PostgreSQL (a Rust port of mallard)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };

        # Stable Rust toolchain with the components needed for build, lint, and fmt.
        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [ "rust-src" "clippy" "rustfmt" ];
        };

        # All kryzhen runtime/dev dependencies (tokio-postgres with NoTls, sha2,
        # walkdir, clap, tracing, and the testcontainers dev-deps) are pure Rust, so
        # the build needs only the toolchain and a linker. The integration tests
        # require an external Docker daemon for testcontainers.
        kryzhen = pkgs.rustPlatform.buildRustPackage {
          pname = "kryzhen";
          version = "0.1.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          # The integration tests under kryzhen/tests/applier.rs need Docker, which is
          # not available in the Nix sandbox; run them via `cargo test` in the devShell.
          doCheck = false;
        };
      in
      {
        packages = {
          default = kryzhen;
          kryzhen = kryzhen;
        };

        devShells.default = pkgs.mkShell {
          name = "kryzhen-dev";

          packages = [
            rustToolchain
            pkgs.git
          ];

          shellHook = ''
            echo "kryzhen dev shell — rustc $(rustc --version | cut -d' ' -f2)"
          '';
        };
      });
}
