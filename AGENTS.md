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

- 0.15.0, notebooklm chat backend. Before: tmuxlet's chat backends were `muse`,
  `gweb`, and `chatgpt` (`llmcli <site>`); NotebookLM's ask lived only in
  `webcli notebooklm ask`. After: `--target notebooklm` (aliases `nblm`,
  `llmcli:notebooklm`) runs `llmcli notebooklm --lock-wait N -p --quiet -n
  <notebook>` through the same chat wrapper (`is_chat_target`,
  `chat_launch_command`). New `--notebook <id|url|title>` is required for it and
  refused by every other target; `-m` fails fast (no model picker); sessions map
  to `--chat` / `-c` as for the other backends. `--output-format json` adds
  `--output-format json` so the run result carries `citations` (`chat_json`).
  Errors are forced to JSON with `LLMCLI_FORMAT=json` and exit 6 ends the run
  `rate_limited`. Text `--file`s inline like muse.
- 0.14.1, reap/prune safety follow-ups. Before: manual `tmuxlet reap -y` let
  runs with no owner through as "legacy" with only a warning; `reap --dry-run`
  appended a `skipped`/`dry run` line to reap.log for every candidate; `prune`
  deleted any terminal-looking run, including an `exited`-looking run whose
  launching tmuxlet was still alive (queued for a slot); and over a lowered cap
  the warning read `24/22 slots in use`. After: `select_reap_targets` runs
  `manual_reap_owner_gate` first and skips and lists (stdout) runs with no owner,
  `unknown owner`, an `unset:` owner, or `ownerExplicit=false` unless
  `--force-foreign`; the gate is rechecked before each kill. Reap logging goes
  through `ReapLog`, which has no path for `--dry-run`, so a preview never writes
  reap.log, and `reap_target` refuses to kill without a writable log. `prune`
  skips terminal runs whose launcher PID (`run_id_pid`, second run-id field) is
  alive or can't be checked (`prune_launcher_skip`). `slot_usage` prints
  `20/24` at or under the cap and `24 live, cap 22` over it, so the warning is
  `tmuxlet: 24 live, cap 22` and the exit-8 message is `pool full: 24 live, cap
  22; ...`. M4 Admin's `--force-foreign` prune by explicit id is unchanged.
- 0.14.0, slot guard. Before: a launch that found the pool full (24 live
  sessions) waited 60s and then auto-reaped any run 10+ minutes old with an
  unchanged pane, and `check_owner` let ownerless runs through as the caller's
  own. On 2026-10-16 at 16:05 CT, three minutes after 0.8.0 added auto-reaping,
  one pass killed 24 legacy `unknown owner` codex sessions belonging to other
  agents. A full pool then exited 1 with `at capacity`. After: `acquire_run_slot`
  warns once at 80% of the cap (`tmuxlet: 20/24 slots in use`); a full pool queues
  until `TMUXLET_ACQUIRE_TIMEOUT`, then exits 8 (`EXIT_POOL_FULL`) with `pool full
  N/N; refusing to start; nothing was killed; ...`. Self-heal runs only with
  `TMUXLET_AUTO_REAP=1`, and then only for runs whose `meta.json` owner equals the
  caller's explicit owner with `ownerExplicit: true` and whose launching tmuxlet
  PID (second run-id field) is dead (`self_heal_eligibility`). Ownerless, `unknown owner`, and
  `unset:` runs are never auto-reaped. Every new run records an owner; with no
  `--owner`/`TMUXLET_OWNER` it is `unset:<parent>@<cwd>` with `ownerExplicit:
  false` plus a stderr warning, and `status` shows `ownerExplicit` for every run.
  `is_foreign` ignores the `unset:` prefix so 0.13 fallback-owned runs stay
  manageable. Manual `tmuxlet reap` was unchanged in 0.14.0 and still matched ownerless
  sessions without `--force-foreign` (fixed in 0.14.1). Keep a full pool non-destructive by default.
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
