set unstable
set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

root := justfile_directory()

[private]
default:
    @just --list rust

# Format Rust sources.
fmt:
    cd "{{ root }}" && cargo fmt --all

# Check Rust formatting without changing files.
fmt-check:
    cd "{{ root }}" && cargo fmt --all -- --check

# Run clippy with the same policy as CI.
clippy:
    cd "{{ root }}" && cargo clippy --all-targets -- -D warnings

# Run the unit test suite.
test:
    cd "{{ root }}" && cargo test

# Build the debug binary.
build:
    cd "{{ root }}" && cargo build

# Build the release binary.
build-release:
    cd "{{ root }}" && cargo build --release

# Run sofka against the current kube context.
run resource="pods":
    cd "{{ root }}" && cargo run -- {{ resource }}

# Headless cluster connectivity check.
smoke:
    cd "{{ root }}" && cargo run -- --check

# Render one headless UI snapshot.
snapshot resource="pods":
    cd "{{ root }}" && cargo run -- {{ resource }} --snapshot
