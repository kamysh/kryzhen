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

        # kryzhen's TLS support uses native-tls (OpenSSL) for the `prefer`/`require`
        # sslmode. native-tls links OpenSSL via openssl-sys, so the build needs
        # pkg-config plus the OpenSSL library. The integration tests require an
        # external Docker daemon for testcontainers.
        commonAttrs = {
          pname = "kryzhen";
          version = "0.7.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          nativeBuildInputs = [ pkgs.pkg-config ];
          buildInputs = [ pkgs.openssl ];
          # The integration tests under kryzhen/tests/applier.rs need Docker, which is
          # not available in the Nix sandbox; run them via `cargo test` in the devShell.
          doCheck = false;
        };

        kryzhen = pkgs.rustPlatform.buildRustPackage commonAttrs;

        # Fully static binary via musl (pkgsStatic). pkgsStatic provides a static
        # OpenSSL so native-tls links cleanly into the musl binary. Build with
        # `nix build .#kryzhen-static`; the result lands in ./result/bin/.
        kryzhen-static = pkgs.pkgsStatic.rustPlatform.buildRustPackage (commonAttrs // {
          nativeBuildInputs = [ pkgs.pkgsStatic.pkg-config ];
          buildInputs = [ pkgs.pkgsStatic.openssl ];
        });
      in
      {
        packages = {
          default = kryzhen;
          kryzhen = kryzhen;
          kryzhen-static = kryzhen-static;
        };

        devShells.default = pkgs.mkShell {
          name = "kryzhen-dev";

          nativeBuildInputs = [ pkgs.pkg-config ];
          buildInputs = [ pkgs.openssl ];

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
