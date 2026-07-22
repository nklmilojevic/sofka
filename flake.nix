{
  description = "sofka - a Kubernetes TUI, reimagined in Rust";

  inputs = {
    # Current stable release. Unlike unstable (which dropped x86_64-darwin
    # after 26.05), this branch still covers all four platforms the release
    # workflow builds for, so a single input suffices.
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
  };

  outputs =
    { self, nixpkgs }:
    let
      inherit (nixpkgs) lib;

      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];

      overlay = final: prev: {
        sofka = final.callPackage ./package.nix { };
      };

      pkgsFor = lib.genAttrs systems (
        system:
        import nixpkgs {
          inherit system;
          overlays = [ overlay ];
        }
      );

      forAllSystems = f: lib.genAttrs systems (system: f pkgsFor.${system});
    in
    {
      overlays.default = overlay;

      packages = forAllSystems (pkgs: {
        default = pkgs.sofka;
        sofka = pkgs.sofka;
      });

      apps = forAllSystems (pkgs: {
        default = {
          type = "app";
          program = "${pkgs.sofka}/bin/sofka";
        };
        sofka = {
          type = "app";
          program = "${pkgs.sofka}/bin/sofka";
        };
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            clippy
            rustfmt
            rust-analyzer
            cargo-watch
            kubectl
            kind
            fluxcd
            cachix
            just
            nixpkgs-fmt
            lefthook
            zizmor
            oxfmt
          ];
        };
      });

      # Every package and devShell doubles as a check so `nix flake check`
      # builds all of them; fmt mirrors CI's format check. Clippy is left to
      # CI (nixpkgs' clippy version drifts from rustup stable and flags
      # different lints), and the cargo test suite is not run here — it
      # needs native root CA certs the build sandbox lacks (see package.nix).
      checks = lib.genAttrs systems (
        system:
        let
          pkgs = pkgsFor.${system};
        in
        self.packages.${system}
        // lib.mapAttrs' (name: drv: lib.nameValuePair "devshell-${name}" drv) self.devShells.${system}
        // {
          fmt =
            pkgs.runCommand "sofka-fmt-check"
              {
                nativeBuildInputs = [
                  pkgs.cargo
                  pkgs.rustfmt
                ];
              }
              ''
                export HOME=$TMPDIR
                cd ${self}
                cargo fmt --all --check
                touch $out
              '';
        }
      );
    };
}
