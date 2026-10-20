# Repository Guidelines

## Project Structure & Module Organization

This is a small Rust CLI crate. The main implementation lives in
`src/main.rs`, including argument parsing, target command mapping, tmux
integration, runtime state handling, and unit tests. `Cargo.toml` defines crate
metadata and package version. `Cargo.lock` is committed for reproducible binary
builds. `README.md` is the user-facing install and usage guide.

Runtime files are created outside the repo under `~/.tmuxlet/runs/<run-id>/`
unless `TMUXLET_HOME` is set.

## Build, Test, and Development Commands

- `cargo test`: runs the unit test suite in `src/main.rs`.
- `cargo fmt -- --check`: verifies Rust formatting without changing files.
- `cargo build`: builds the local binary.
- `cargo run -- --version`: checks the compiled version string.
- `cargo run -- -p "say ready"`: smoke-tests print mode through the default
  target CLI.
- `cargo install --path . --force`: installs the local checkout as `tmuxlet`.

The CLI requires `tmux` on `PATH` and at least one supported target CLI such as
`claude` or `codex`.

## Coding Style & Naming Conventions

Use standard Rust formatting via `cargo fmt`. Keep functions small and direct;
this crate currently favors explicit helper functions over new abstractions.
Use `snake_case` for functions and variables, `PascalCase` for structs/enums,
and uppercase constants for tunables such as timeouts. Keep user-facing error
messages short and actionable.

## Testing Guidelines

Tests are inline in `src/main.rs` under `#[cfg(test)]`. Add unit tests for
argument parsing, target flag mapping, prompt construction, status handling,
and pane-text classification. Prefer deterministic pure-function tests over
tests that require live tmux sessions or external CLIs. Run `cargo test` before
committing behavior changes.

## Commit & Pull Request Guidelines

Recent commits use short imperative subject lines, for example
`Handle tmuxlet print-mode confirmations` and
`Add normalized session controls`. Keep commits focused and avoid mixing docs,
version bumps, and behavior changes unless they are part of the same release.

Pull requests should include a concise summary, the commands run for
verification, and notes about behavior visible to users, especially changes to
CLI flags, output status, runtime files, or target-specific mappings.

## Change Notes

- 0.13.0, Claude permission floor. Before: claude launches defaulted to
  `--permission-mode auto` but passed any caller mode through verbatim
  (`acceptEdits`, `plan`, `default`, or extra `--target-arg` flags), and
  `--dangerously-skip-permissions` used a different flag, so sessions came up in
  bypass, auto, accept-edits, or plan depending on the caller. After: every
  claude launch gets exactly one `--permission-mode`, `bypassPermissions` by
  default or `auto` on request (`--permission-mode`, `TMUXLET_CLAUDE_PERMISSION_MODE`);
  lower modes clamp to `auto` with a warning, `--target-arg` permission flags are
  folded in, a refused bypass relaunches once in `auto`, and the first-run bypass
  prompt is accepted instead of Enter-ing "No, exit". The same applies to the
  "trust this folder?" gate: before, the generic startup auto-Enter picked "No,
  exit" and claude quit within seconds in any untrusted folder (seen in
  ai-usage-tracker); after, tmuxlet selects "Yes, I trust this folder". Keep these invariants when
  touching `claude_command` or the run loop.

## Agent-Specific Instructions

Do not rewrite unrelated files. Preserve uncommitted user changes. When changing
CLI behavior, update `README.md` and add focused tests in the same pass.
