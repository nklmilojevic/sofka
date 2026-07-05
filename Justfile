set unstable
set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

mod nix '.just/nix.just'
mod release '.just/release.just'
mod rust '.just/rust.just'

default:
    @just --list

# Install local git hooks.
hooks:
    lefthook install

# Format Rust sources.
fmt: rust::fmt

# Check Rust formatting without changing files.
fmt-check: rust::fmt-check

# Run clippy with the same policy as CI.
clippy: rust::clippy

# Run the unit test suite.
test: rust::test

# Fast local confidence check.
check: fmt-check clippy test

# Build the debug binary.
build: rust::build

# Build the release binary.
build-release: rust::build-release

# Run sofka against the current kube context.
run resource="pods":
    just rust run {{ resource }}

# Headless cluster connectivity check.
smoke: rust::smoke

# Render one headless UI snapshot.
snapshot resource="pods":
    just rust snapshot {{ resource }}

# Run Nix's flake checks.
nix-check: nix::check

# Build the Nix package.
nix-build: nix::build

# Build and smoke-test the Nix package.
nix-smoke: nix::smoke

# Release a patch version.
release-patch: release::patch

# Release a minor version.
release-minor: release::minor

# Release a major version.
release-major: release::major
