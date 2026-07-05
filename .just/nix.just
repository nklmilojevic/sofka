set unstable
set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

root := justfile_directory()

[private]
default:
    @just --list nix

# Run Nix's flake checks.
check:
    cd "{{ root }}" && nix flake check --no-build

# Build the Nix package.
build:
    cd "{{ root }}" && nix build .#sofka --print-build-logs

# Build and smoke-test the Nix package.
smoke: build
    cd "{{ root }}" && ./result/bin/sofka --version
