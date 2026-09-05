# AGENTS.md

Rules for AI agents and humans working on sofka. `docs/architecture.md` explains
the module layout; this file is about how to change the repo without making a
mess.

## Formatting is owned by formatters

- Never hand-align anything a formatter owns. `cargo fmt` owns Rust, `oxfmt` owns
  Markdown and YAML, `nixpkgs-fmt` owns Nix.
- Run `just hooks` once after cloning. The lefthook pre-commit hook then runs the
  formatters, clippy, and the tests on every commit and stages what it reformats.
- Never commit with `--no-verify`. If the hook rewrites a file you touched, that
  rewrite is correct; commit it.
- Markdown tables in `docs/` are re-flowed by `oxfmt` to fit the widest cell. Do
  not pad cells by hand to keep a table narrow. Shorten the text instead.

## Local commands

Use the `Justfile`; it matches CI exactly.

```sh
just check          # fmt-check + clippy (-D warnings) + cargo test
just fmt            # cargo fmt --all
just clippy         # cargo clippy --all-targets -- -D warnings
just test           # cargo test
just run <resource> # cargo run -- <resource> against the current kube context
just smoke          # headless connectivity check (cargo run -- --check)
```

CI runs `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`,
and `cargo test`. All three must pass before a PR is ready.

## Code

- Rust edition 2024. `let` chains and `is_none_or`/`is_some_and` are fine and
  already used.
- Clippy runs with `-D warnings`. A new warning is a build failure.
- Tests live next to the code: `src/app/tests.rs` for application behaviour,
  `mod tests` at the bottom of other modules. Tests never need a cluster;
  `Cluster::fake()` provides the kind registry and `apply(&mut app, json!(...))`
  feeds objects through the same message path a watch would.
- Every user-visible behaviour change gets a test that drives it through
  `handle_key`, not by calling the internal method directly.
- Things that must stay in sync:
  - the `enter` match in `src/app/navigation.rs` and `views::BUILTIN_DRILLS`
  - a new key or built-in command and its rows in `docs/keys.md`, the `?` help
    text in `src/ui.rs`, and `docs/features.md`
  - a new config field and `docs/configuration.md`
- Do not edit `Cargo.lock` by hand. Renovate owns dependency bumps.
- Do not add comments or doc comments to code you are not otherwise changing.

## Git

- Base branch is `main`.
- Conventional commits: `feat:`, `fix:`, `docs:`, `refactor:`, `test:`, `chore:`.
  The subject says what changed; the body says why and what the root cause was.
  Do not list files in the body; the diff already does.
- One logical change per PR. Docs for a feature ship in the same PR as the
  feature.
- `Closes #N` in the commit body or PR description when the change resolves an
  issue.

## Releases

Releases are cut from a clean `main` with `just release patch|minor|major`. The
recipe bumps `Cargo.toml` and `Cargo.lock`, pushes, and creates the GitHub
Release; the release workflow builds the binaries, publishes to crates.io, and
warms the Nix cache. Never bump the version in a feature PR.
