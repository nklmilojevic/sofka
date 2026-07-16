{
  description = "sofka - a Kubernetes TUI, reimagined in Rust";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    # nixpkgs >= 26.11 dropped x86_64-darwin; keep Intel macs on the 26.05 release
    nixpkgs-darwin-intel.url = "github:NixOS/nixpkgs/nixpkgs-26.05-darwin";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    { self, nixpkgs, nixpkgs-darwin-intel, flake-utils }:
    let
      overlay = final: prev: {
        sofka = final.callPackage ./package.nix { };
      };
    in
    flake-utils.lib.eachSystem
      [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ]
      (
        system:
        let
          pkgs = import (if system == "x86_64-darwin" then nixpkgs-darwin-intel else nixpkgs) {
            inherit system;
            overlays = [ overlay ];
          };
          # On Linux, ship the fully-static musl build: the same binary works
          # inside Nix and on any distro, so the release tarballs are just
          # this package's output (one build per platform, no separate cargo
          # matrix). macOS can't link static executables; its regular build
          # links system libraries plus Nix's libiconv (via Rust std) — see
          # `sofka-dist` below for the portable variant.
          sofka = if pkgs.stdenv.hostPlatform.isLinux then pkgs.pkgsStatic.sofka else pkgs.sofka;
          # The distributable binary for release tarballs. On Linux it's the
          # static build unchanged. On macOS, rewrite the one Nix store
          # reference (libiconv) to the copy every macOS ships in /usr/lib,
          # then re-sign (install_name_tool invalidates the ad-hoc signature,
          # which arm64 macOS refuses to run). Impure by Nix standards, so
          # it's a separate output — flake consumers keep the pure `sofka`.
          sofka-dist =
            if pkgs.stdenv.hostPlatform.isLinux then
              sofka
            else
              pkgs.runCommand "sofka-dist-${sofka.version}"
                {
                  nativeBuildInputs = with pkgs; [
                    darwin.cctools
                    darwin.sigtool
                  ];
                }
                ''
                  mkdir -p $out/bin
                  cp ${sofka}/bin/sofka $out/bin/sofka
                  chmod +w $out/bin/sofka
                  old="$(otool -L $out/bin/sofka | awk '/\/nix\/store\/.*libiconv/ { print $1 }')"
                  [ -n "$old" ] || { echo "no store libiconv reference found"; exit 1; }
                  install_name_tool -change "$old" /usr/lib/libiconv.2.dylib $out/bin/sofka
                  codesign -f -s - $out/bin/sofka
                  chmod -w $out/bin/sofka
                  # otool's first line is the binary's own (store) path — only
                  # the dylib lines after it must be store-free.
                  if otool -L $out/bin/sofka | tail -n +2 | grep -q /nix/store; then
                    echo "binary still references /nix/store:"; otool -L $out/bin/sofka; exit 1
                  fi
                '';
        in
        {
          packages = {
            default = sofka;
            inherit sofka sofka-dist;
          };

          apps = {
            default = {
              type = "app";
              program = "${sofka}/bin/sofka";
            };
            sofka = {
              type = "app";
              program = "${sofka}/bin/sofka";
            };
          };

          devShells.default = pkgs.mkShell {
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
        }
      )
    // {
      overlays.default = overlay;
    };
}
