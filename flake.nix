{
  description = "harness — single-binary agent harness (cargo workspace)";

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
  }: let
    # Home Manager module, importable as a single module:
    # `imports = [ inputs.harness.homeManagerModules.default ]`. It generates
    # `$XDG_CONFIG_HOME/harness/config.toml` from `programs.harness.settings`
    # and points the installed `harness` at it; see `modules/harness.nix` for
    # the caveats (the managed file is read-only, hence the `--config`
    # wrapper). Merged into the outputs below, since `eachSystem` only emits
    # the per-system ones.
    hm.homeManagerModules.default = ./modules/harness.nix;
  in
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
            # All 25 plugins are rlibs linked into it — "linked" here means
            # Rust-level: every plugin rlib is compiled into that one binary.
            # The C runtime is still dynamic (glibc on Linux, system libSystem
            # on Darwin); see "Linkage" in the README.
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
              # `explicit_env_gives_clean_subprocess` clears PATH, then
              # respawns a python probe that the test locates via hardcoded
              # FHS paths (/usr/bin/python3, /usr/local/bin/python3). On
              # NixOS none of those exist, so it fails with "python3: command
              # not found" no matter how this package is built. The test needs
              # an FHS-shaped PATH; unrelated to packaging.
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
            # `ring` (pulled in by rustls via reqwest's `rustls-tls` feature)
            # compiles C/asm through the `cc` crate and so needs a real
            # linker. stdenv.cc supplies this implicitly, but naming it here
            # makes the dev shell's contract explicit and keeps working if
            # stdenv's cc ever stops being on the initial PATH. Without it, a
            # bare `cargo build` inside the shell fails with
            # "linker `cc` not found".
            stdenv.cc
            # `cargo test` here mirrors checks.harness-test above: shell plugin
            # tests spawn `python3` and `bash -c`.
            python3
            bash
          ];
        };

        formatter = pkgs.alejandra;
      }
    )
    // {
      # Rightmost wins, and function application binds tighter than `//`, so
      # this merges onto the `eachSystem` result above.
      inherit (hm) homeManagerModules;
    };
}
