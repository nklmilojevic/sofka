set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

default:
    @just --list

# Install local git hooks.
hooks:
    lefthook install

# Format Rust sources.
fmt:
    cargo fmt --all

# Check Rust formatting without changing files.
fmt-check:
    cargo fmt --all -- --check

# Run clippy with the same policy as CI.
clippy:
    cargo clippy --all-targets -- -D warnings

# Run the unit test suite.
test:
    cargo test

# Fast local confidence check.
check: fmt-check clippy test

# Build the debug binary.
build:
    cargo build

# Build the release binary.
build-release:
    cargo build --release

# Run sofka against the current kube context.
run resource="pods":
    cargo run -- {{ resource }}

# Headless cluster connectivity check.
smoke:
    cargo run -- --check

# Render one headless UI snapshot.
snapshot resource="pods":
    cargo run -- {{ resource }} --snapshot

# Run Nix's flake checks.
nix-check:
    nix flake check --no-build

# Build the Nix package.
nix-build:
    nix build .#sofka --print-build-logs

# Build and smoke-test the Nix package.
nix-smoke: nix-build
    ./result/bin/sofka --version

# Release a patch version.
release-patch:
    just _release patch

# Release a minor version.
release-minor:
    just _release minor

# Release a major version.
release-major:
    just _release major

_release bump:
    #!/usr/bin/env bash
    set -euo pipefail
    bump="{{ bump }}"
    just _ensure-release-ready
    latest="$(gh release view --json tagName -q .tagName 2>/dev/null || true)"
    if test -z "$latest"; then latest="v0.0.0"; fi
    version="$(just _next-version "$latest" "$bump")"
    new_tag="v$version"
    echo "$latest -> $new_tag"
    tmp="$(mktemp)"
    awk -v version="$version" '
      /^version = "/ && !done {
        print "version = \"" version "\""
        done = 1
        next
      }
      { print }
      END { if (!done) exit 1 }
    ' Cargo.toml > "$tmp"
    mv "$tmp" Cargo.toml
    cargo check --quiet
    cargo test --quiet
    git add Cargo.toml Cargo.lock
    if git diff --cached --quiet; then
      echo "Cargo.toml is already at v$version"
    else
      git commit -m "chore(release): v$version"
      git push origin main
    fi
    target="$(git rev-parse HEAD)"
    gh release create "$new_tag" --target "$target" --generate-notes

_ensure-release-ready:
    #!/usr/bin/env bash
    set -euo pipefail
    gh auth status >/dev/null
    test -z "$(git status --porcelain)" || { echo "working tree must be clean"; exit 1; }
    git fetch origin main --tags
    if test "$(git branch --show-current)" != "main"; then
      git switch main
    fi
    git pull --ff-only origin main
    test "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" || { echo "local main must match origin/main"; exit 1; }

_next-version latest bump:
    #!/usr/bin/env bash
    set -euo pipefail
    latest="{{ latest }}"
    bump="{{ bump }}"
    latest="${latest#v}"
    IFS=. read -r major minor patch rest <<< "$latest"
    if [[ -n "${rest:-}" || ! "$major" =~ ^[0-9]+$ || ! "$minor" =~ ^[0-9]+$ || ! "$patch" =~ ^[0-9]+$ ]]; then
      echo "cannot parse version '$latest'" >&2
      exit 1
    fi
    case "$bump" in
      major)
        major=$((major + 1))
        minor=0
        patch=0
        ;;
      minor)
        minor=$((minor + 1))
        patch=0
        ;;
      patch)
        patch=$((patch + 1))
        ;;
      *)
        echo "unknown bump '$bump'" >&2
        exit 1
        ;;
    esac
    echo "$major.$minor.$patch"
