set unstable
set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

root := justfile_directory()

[private]
default:
    @just --list release

# Release a patch version.
patch:
    just --justfile "{{ root }}/Justfile" release _release patch

# Release a minor version.
minor:
    just --justfile "{{ root }}/Justfile" release _release minor

# Release a major version.
major:
    just --justfile "{{ root }}/Justfile" release _release major

[private]
_release bump:
    #!/usr/bin/env bash
    set -euo pipefail
    cd "{{ root }}"
    bump="{{ bump }}"
    if command -v op >/dev/null 2>&1; then
      gh() { op plugin run -- gh "$@"; }
    else
      gh() { command gh "$@"; }
    fi
    just --justfile "{{ root }}/Justfile" release _ensure-release-ready
    latest="$(gh release view --json tagName -q .tagName 2>/dev/null || true)"
    if test -z "$latest"; then latest="v0.0.0"; fi
    version="$(just --justfile "{{ root }}/Justfile" release _next-version "$latest" "$bump")"
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

[private]
_ensure-release-ready:
    #!/usr/bin/env bash
    set -euo pipefail
    cd "{{ root }}"
    if command -v op >/dev/null 2>&1; then
      gh() { op plugin run -- gh "$@"; }
    else
      gh() { command gh "$@"; }
    fi
    gh auth status >/dev/null
    test -z "$(git status --porcelain --untracked-files=no)" || { echo "tracked files must be clean"; exit 1; }
    git fetch origin main --tags
    if test "$(git branch --show-current)" != "main"; then
      git switch main
    fi
    git pull --ff-only origin main
    test "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" || { echo "local main must match origin/main"; exit 1; }

[private]
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
