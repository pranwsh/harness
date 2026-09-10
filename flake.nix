{
  description = "harness — statically-linked single-binary agent harness (cargo workspace)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    crane.url = "github:ipetkov/crane";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = {
    self,
    nixpkgs,
    crane,
    flake-utils,
    ...
  }:
  # nixos-unstable (26.11+) dropped x86_64-darwin, so don't use eachDefaultSystem.
    flake-utils.lib.eachSystem
    [
      "x86_64-linux"
      "aarch64-linux"
      "aarch64-darwin"
    ] (
      system: let
        pkgs = nixpkgs.legacyPackages.${system};
        craneLib = crane.mkLib pkgs;

        # The default config is ./config.toml (cwd-relative, see
        # plugins/config/src/lib.rs; override with `harness --config PATH`);
        # local config.toml files are
        # gitignored/local-only, so only ship the example.
        # crane's default cleaning keeps *.toml at the root; be explicit anyway.
        src = craneLib.cleanCargoSource ./.;

        commonArgs = {
          inherit src;
          strictDeps = true;
          # Root Cargo.toml is a virtual workspace (no [package]),
          # so tell crane the name/version explicitly (matches harness/Cargo.toml).
          pname = "harness";
          version = "0.1.0";
        };

        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        harness = craneLib.buildPackage (
          commonArgs
          // {
            inherit cargoArtifacts;
            # Only binary in the workspace is `harness` (harness/src/main.rs).
            # All 19 plugins are rlibs statically linked into it.
            cargoExtraArgs = "-p harness";

            postInstall = ''
              install -Dm444 config.example.toml \
                $out/share/doc/harness/config.example.toml
            '';

            meta = with pkgs.lib; {
              description = "Agent harness TUI (single binary)";
              mainProgram = "harness";
              license = licenses.mit;
              platforms = platforms.unix;
            };
          }
        );
      in {
        packages = {
          inherit harness;
          default = harness;
        };

        checks = {
          inherit harness;
          harness-clippy = craneLib.cargoClippy (
            commonArgs
            // {
              inherit cargoArtifacts;
              cargoClippyExtraArgs = "--workspace --all-targets -- --deny warnings";
            }
          );
          # NOTE: no cargoFmt check — repo is not rustfmt-clean
          # (see `nix fmt` / `cargo fmt` to normalize). Keep `formatter`
          # output below for manual formatting.
          harness-test = craneLib.cargoTest (
            commonArgs
            // {
              inherit cargoArtifacts;
              # shell plugin tests spawn `python3` and `bash -c`.
              nativeBuildInputs = with pkgs; [
                python3
                bash
              ];
              # `explicit_env_gives_clean_subprocess` clears PATH then
              # respawns `bash` via PATH lookup: works on FHS (/bin/bash)
              # but not in the Nix sandbox (bash lives in /nix/store).
              # Pre-existing hermeticity assumption, unrelated to packaging.
              cargoTestExtraArgs = "--workspace -- --skip explicit_env_gives_clean_subprocess";
            }
          );
        };

        devShells.default = craneLib.devShell {
          checks = self.checks.${system};
          packages = with pkgs; [
            rustc
            cargo
            clippy
            rustfmt
            pkg-config
          ];
        };

        formatter = pkgs.alejandra;
      }
    );
}
