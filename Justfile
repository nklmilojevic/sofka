set unstable
set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

mod nix '.just/nix.just'
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

# Lint the benchmarks and probes, which need the bench feature.
clippy-bench: rust::clippy-bench

# Run the unit test suite.
test: rust::test

# Generate an HTML coverage report.
coverage: rust::coverage

# Fast local confidence check.
check: fmt-check clippy clippy-bench test

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

# Cut a release with vesnik from an up-to-date main.
release:
    @test "$(git branch --show-current)" = main || { echo "release from main" >&2; exit 1; }
    @test -z "$(git status --porcelain)" || { echo "release from a clean tree: vesnik commits untracked files it writes to, such as CHANGELOG.md" >&2; git status --short >&2; exit 1; }
    git pull --ff-only origin main
    cargo check --quiet
    cargo test --quiet
    vesnik release --yes
