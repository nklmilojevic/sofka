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
        in
        {
          packages = {
            default = pkgs.sofka;
            sofka = pkgs.sofka;
          };

          apps = {
            default = {
              type = "app";
              program = "${pkgs.sofka}/bin/sofka";
            };
            sofka = {
              type = "app";
              program = "${pkgs.sofka}/bin/sofka";
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
