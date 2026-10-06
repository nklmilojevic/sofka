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
    if test -z "${GH_TOKEN:-}" && command -v op >/dev/null 2>&1; then
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
    notes="$(mktemp)"
    trap 'rm -f "$notes"' EXIT
    just --justfile "{{ root }}/Justfile" release _notes "$latest" "$new_tag" > "$notes"
    cat "$notes"
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
    gh release create "$new_tag" --target "$target" --notes-file "$notes"

# Print release notes for a tag, or for unreleased commits when no tag is given.
notes tag="":
    #!/usr/bin/env bash
    set -euo pipefail
    cd "{{ root }}"
    tag="{{ tag }}"
    if test -z "$tag"; then
      from="$(git describe --tags --abbrev=0 HEAD)"
      to="HEAD"
    else
      git rev-parse --verify --quiet "refs/tags/$tag" >/dev/null || { echo "unknown tag '$tag'" >&2; exit 1; }
      from="$(git describe --tags --abbrev=0 "$tag^" 2>/dev/null || echo v0.0.0)"
      to="$tag"
    fi
    just --justfile "{{ root }}/Justfile" release _notes "$from" "$to"

# Regenerate the notes of a published release and replace its description.
renote tag:
    #!/usr/bin/env bash
    set -euo pipefail
    cd "{{ root }}"
    if test -z "${GH_TOKEN:-}" && command -v op >/dev/null 2>&1; then
      gh() { op plugin run -- gh "$@"; }
    else
      gh() { command gh "$@"; }
    fi
    notes="$(mktemp)"
    trap 'rm -f "$notes"' EXIT
    just --justfile "{{ root }}/Justfile" release notes "{{ tag }}" > "$notes"
    cat "$notes"
    gh release edit "{{ tag }}" --notes-file "$notes"

[private]
_notes from to:
    #!/usr/bin/env bash
    set -euo pipefail
    cd "{{ root }}"
    from="{{ from }}"
    to="{{ to }}"
    if test -z "${GH_TOKEN:-}" && command -v op >/dev/null 2>&1; then
      gh() { op plugin run -- gh "$@"; }
    else
      gh() { command gh "$@"; }
    fi
    command -v claude >/dev/null || { echo "claude is required to write release notes" >&2; exit 1; }
    range="$from..HEAD"
    if git rev-parse --verify --quiet "$to^{commit}" >/dev/null; then range="$from..$to"; fi
    if test "$from" = "v0.0.0"; then range="${range#v0.0.0..}"; fi
    commits="$(git log --no-merges --invert-grep --grep='^chore(release):' --format='### %s%n%n%b%n' "$range")"
    test -n "$commits" || { echo "no commits in $range" >&2; exit 1; }
    repo="$(gh repo view --json nameWithOwner -q .nameWithOwner)"
    previous=()
    if test "$from" != "v0.0.0"; then previous=(-f previous_tag_name="$from"); fi
    changes="$(gh api "repos/$repo/releases/generate-notes" -f tag_name="$to" "${previous[@]}" -q .body)"
    system_prompt="You write release notes for sofka, a terminal UI for Kubernetes written in Rust.
    Write in ASD-STE100 Simplified Technical English:
    - Use only simple, approved words with one meaning. Use technical names (Kubernetes kinds, keys, commands, config fields) as they are.
    - Write descriptive sentences of no more than 25 words. Put one topic in each sentence.
    - Use the active voice and the simple present or simple past tense.
    - Use articles (a, an, the) and do not drop words to make sentences shorter.
    - Do not use -ing words as nouns or adjectives. Do not use slang, idioms, or marketing words.
    Output only Markdown, with no preamble and no closing remark.
    Be brief. Start with one or two sentences that tell what the release gives to the user.
    Then use the headings '## New features', '## Fixes', and '## Other changes'. Leave out a heading that has no items.
    Give each pull request one bullet with one or two short sentences: what changed and why it is important to the user.
    Do not describe edge cases, safeguards, or implementation details. Combine follow-up fixes into the change they fix. Leave out dependency updates, CI, and release chores, unless they change behavior for the user.
    Put inline code in backticks. Do not include links, PR numbers, or author names."
    summary="$(printf 'Write release notes for sofka %s. These are the commits since %s:\n\n%s\n' "$to" "$from" "$commits" \
      | claude -p --model claude-sonnet-5-5 --tools "" --disable-slash-commands --no-session-persistence --setting-sources "" --system-prompt "$system_prompt")"
    test -n "$summary" || { echo "claude returned empty release notes" >&2; exit 1; }
    printf '%s\n\n%s\n' "$summary" "$changes"

[private]
_ensure-release-ready:
    #!/usr/bin/env bash
    set -euo pipefail
    cd "{{ root }}"
    if test -z "${GH_TOKEN:-}" && command -v op >/dev/null 2>&1; then
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
