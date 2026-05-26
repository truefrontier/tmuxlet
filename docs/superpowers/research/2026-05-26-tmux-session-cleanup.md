---
date: 2026-05-26
researcher: Kevin Kirchner (Claude)
git_commit: 2c92558b5cf74cc81f2a05e651d35d8418515b89
branch: claude-hooks
repository: CodefiLabs/tmuxlet
topic: "tmux session cleanup — codebase state for auto-kill + prune"
tags: [research, codebase, tmux, run-lifecycle, cleanup, cli]
status: complete
last_updated: 2026-05-26
last_updated_by: Kevin Kirchner (Claude)
---

# Research: tmux session cleanup — codebase state

**Date**: 2026-05-26
**Researcher**: Kevin Kirchner (Claude)
**Git Commit**: 2c92558b5cf74cc81f2a05e651d35d8418515b89
**Branch**: claude-hooks
**Repository**: CodefiLabs/tmuxlet

## Research Question

What is the current state of `tmuxlet`'s run-completion lifecycle, CLI dispatch, run-directory layout, tmux helpers, test patterns, and README structure — at the level of precision needed to implement the two changes in `docs/superpowers/specs/2026-05-26-tmux-session-cleanup-design.md`:

1. Auto-kill the tmux session on a `completed` print-mode return.
2. Add a `tmuxlet prune` subcommand for explicit cleanup of terminal-state runs.

This document describes what exists today. It does not propose changes — the design spec covers the proposed end state, and the forthcoming implementation plan covers the step-by-step edits.

## Summary

`tmuxlet` is a single-file Rust CLI (`src/main.rs`, 2349 lines, no runtime or dev deps). The print-mode polling loop is inline in `run_print` (not a separate `wait_for_completion` function as the design spec implied), with four terminal return paths (`completed`, `blocked`, `timeout`, `exited`). The `completed` path at lines 327–342 already calls `capture_pane_to_file(&tmux_session, &paths.pane_log).ok()` immediately before constructing its `RunResult` — that line (330) is the anchor for the auto-kill insertion at line 331, before the `return Ok(...)`.

Subcommand dispatch is a single match in `run()` at lines 143–168. There is no shared flag parser for handlers like `stop`, `status`, `read`, `send`, `attach`, `bridge` — each rolls its own minimal arg loop. There is also no existing `--yes`/`--dry-run` style flag anywhere outside `parse_options` (the implicit print-mode branch), and no helper that classifies a run dir from its marker files. `handle_stop` kills the tmux session but does NOT delete the `~/.tmuxlet/runs/<id>/` directory — `prune` will be the first command to do filesystem cleanup.

Tests are inline (`#[cfg(test)] mod tests` at lines 1841–2349), use only the standard library, name themselves `<subject>_<verb>_<condition>` (no `test_` prefix), and have exactly one fs-touching test that isolates via `env::temp_dir().join(make_run_id())`. There is no fixture-builder helper for synthetic run-dirs — the new classification tests will need to build their own.

## Detailed Findings

### 1. Print-mode run completion lifecycle

The polling loop is **inline within `run_print`** at @src/main.rs:326-415. There is no separate `wait_for_completion` function — the design spec's "`run_print` → `wait_for_completion`" reference is approximate; the loop is part of `run_print` itself.

The four terminal return paths:

| Status | File | Lines | Trigger | Writes `error.txt`? |
|---|---|---|---|---|
| `completed` | @src/main.rs:327-342 | `complete.txt` exists | No |
| `blocked` | @src/main.rs:360-377 | `is_blocked_state(...)` on captured pane text | Yes (multi-line, prefix `"blocked "`) |
| `timeout` | @src/main.rs:382-396 | `Instant::now() >= deadline` | Yes (single line, prefix `"timeout "`) |
| `exited` | @src/main.rs:399-411 | `!tmux_session_exists(&tmux_session)` and no `complete.txt` | No |

**Auto-kill insertion point (`completed` path)**:
- The existing `capture_pane_to_file(&tmux_session, &paths.pane_log).ok();` is at @src/main.rs:330.
- The `return Ok(RunResult { ... })` block runs from @src/main.rs:332-342.
- The `tmux_session` variable (set at @src/main.rs:285 to `format!("tmuxlet-{run_id}")`) is in scope.
- Insertion happens at line 331, between the capture and the return.

**Error.txt content for classification**:

- Timeout writes the literal `"timeout waiting for explicit completion\n"` at @src/main.rs:385 (via `fs::write(&paths.error, "timeout waiting for explicit completion\n").ok();`).
- Blocked writes via `blocked_error_message(...)` at @src/main.rs:924-931. The first line is `"blocked waiting for a confirmation or permission prompt"` (followed by `\n\n` and additional context lines).

So classifying from `error.txt` requires reading the first line and prefix-matching:
- starts with `"timeout "` → Timeout
- starts with `"blocked "` → Blocked

**`RunResult` struct**: defined at @src/main.rs:107-115 with fields `status: String`, `exit_code: i32`, `run_id: String`, plus pane/error/heartbeat paths. The `status` string is the four lowercase tokens above. The `result_status_code(&str) -> i32` helper at @src/main.rs:1242-1244 maps `"completed"` → 0 and all others → 2.

### 2. CLI dispatch and existing subcommands

The full dispatch lives at @src/main.rs:143-168 in `run()`. It's a single-level match over `argv[1]`:

```
match cmd.as_str() {
    "--help" | "-h" => print_help(),
    "--version" | "-V" => print_version(),
    "status" => handle_status(&args[2..]),
    "read"   => handle_read(&args[2..]),
    "send"   => handle_send(&args[2..]),
    "attach" => handle_attach(&args[2..]),
    "stop"   => handle_stop(&args[2..]),
    "bridge" => handle_bridge(&args[2..]),
    _ => /* implicit print-mode via parse_options */
}
```

`main()` at @src/main.rs:126-134 formats `Result<(), String>` errors as `tmuxlet: {err}` to stderr and exits with code 1.

**`handle_status`** at @src/main.rs:1093-1117:
- Iterates `read_dir(state_dir().join("runs"))`.
- For each entry, reads `meta.json` (skips dirs without it), prints the file's text trimmed, then computes the heartbeat-file mtime and prints `last activity: Xs ago`.
- Does NOT cross-check `tmux_session_exists`.
- Does NOT classify status (completed/blocked/timeout/exited) — just dumps meta.json.
- Untested.

**`handle_stop`** at @src/main.rs:1171-1194:
- Requires an `<id>` arg (errors with "stop requires <id>" if missing).
- Resolves `tmux_name_from_id` (helper at @src/main.rs:1196-1202).
- Gates on `tmux_session_exists(&name)` — if absent, prints `Session {name} is not running` and returns Ok.
- If alive: sends `/exit` + `Enter` via `run_tmux(...).ok()` (errors swallowed), sleeps `Duration::from_secs(2)`, then escalates to `kill-session` with `?` (propagates error).
- Always prints `Stopped {id}` on Ok.
- Does NOT touch `~/.tmuxlet/runs/<id>/` — leaves marker files in place.

**Flag-parsing patterns**: Each handler does ad-hoc parsing. There is no `clap`, no shared option struct outside `parse_options` (the print-mode parser). No existing `-y`/`--yes`/`--dry-run` precedent anywhere in the file.

### 3. Run directory layout and `state_dir` resolution

**`state_dir()`** at @src/main.rs:1772-1777 resolves in this order:
1. `TMUXLET_HOME` env var (if set, used directly)
2. `$HOME/.tmuxlet`
3. `.tmuxlet` (in CWD, fallback)

The runs root is `state_dir().join("runs")`.

**`RunPaths`** struct at @src/main.rs:94-105:
```
struct RunPaths {
    root: PathBuf,
    meta: PathBuf,        // meta.json
    complete: PathBuf,    // complete.txt
    error: PathBuf,       // error.txt
    pane_log: PathBuf,
    heartbeat: PathBuf,   // heartbeat
    /* plus a couple of other PathBuf fields for Claude transcript handling */
}
```

Built by `run_paths(run_id)` at @src/main.rs:1779-1795 — joins each filename onto `state_dir().join("runs").join(run_id)`.

### 4. tmux helpers

**`run_tmux<I, S>(args: I) -> Result<Output, String>`** at @src/main.rs:958-972:
- Generic over `IntoIterator<Item = AsRef<OsStr>>`.
- Shells out to `tmux` with the provided args.
- Returns `Output` on success (regardless of non-zero exit), or `Err(String)` if the child process couldn't be spawned.
- Existing call patterns:
  - `run_tmux([OsStr::new("kill-session"), OsStr::new("-t"), OsStr::new(&name)])` — used in `handle_stop` @src/main.rs:1185-1189 (with `?` propagation).
  - `run_tmux([OsStr::new("send-keys"), ...]).ok()` — fire-and-forget pattern in multiple places.

**`tmux_session_exists(name: &str) -> bool`** at @src/main.rs:933-939:
- Calls `tmux has-session -t <name>` and checks `status.success()`.
- The standard "is this session alive?" probe used in `run_print` (exited check) and `handle_stop` (gate).

**Tmux session naming**: `format!("tmuxlet-{run_id}")` at @src/main.rs:285. The `tmux_name_from_id` helper at @src/main.rs:1196-1202 accepts both bare `<run_id>` and the full `tmuxlet-<run_id>` form for `handle_stop`'s convenience — useful as a guideline if `prune` accepts user-supplied IDs.

### 5. Test conventions

**Test block** lives at @src/main.rs:1841-2349 in a single `#[cfg(test)] mod tests`. 44 test functions total covering: argv parsing, per-target flag mapping, pane-text classifiers, Claude hook JSON, transcript parsing, retry/duration helpers.

**Naming convention**: `<subject>_<verb>_<condition>` — no `test_` prefix. Examples: `parses_target_with_short_flag`, `claude_target_maps_dangerous_skip`, `classifies_blocked_prompt`, `extract_transcript_anchors_on_latest_user_message`, `write_complete_once_is_idempotent`.

**Test helpers (only two)**:
- `parse(args: &[&str]) -> Options` at @src/main.rs:1845-1848 — convenience wrapper that calls `parse_options(args.iter().map(|s| s.to_string()).collect())` and unwraps. Used for argv-parsing tests.
- `has_pair(argv: &[String], flag: &str, value: &str) -> bool` at @src/main.rs:1850-1853 — asserts an adjacent flag+value pair appears in argv.

**Dependencies**: `Cargo.toml` has an empty `[dependencies]` section and no `[dev-dependencies]` section at all. Everything uses `std`.

**Filesystem isolation pattern**: The only fs-touching test is `write_complete_once_is_idempotent` at @src/main.rs:2336-2348. It builds an isolated dir via `env::temp_dir().join(make_run_id())` (where `make_run_id()` at @src/main.rs:1769-1776 produces a `tmuxlet-test-<nanos>-<pid>` style unique slug), exercises the code under test, then `fs::remove_dir_all(&dir).ok()` at the end. No RAII guard, no `tempfile` crate, no `TMUXLET_HOME` env juggling, no synthetic marker-file fixture builder.

**No existing classify-status helper**. The four status strings `"completed"`, `"blocked"`, `"timeout"`, `"exited"` are produced inline at @src/main.rs:334 / 370 / 389 / 404 in `run_print`. The only related helpers are:
- `result_status_code(&str) -> i32` at @src/main.rs:1242-1244 (pure string-to-int, tested).
- `heartbeat_age_secs(path: &Path) -> Option<u64>` at @src/main.rs:1119-1125 (untested).
- `run_paths(run_id)` at @src/main.rs:1779-1795 (canonical-path builder, untested).

### 6. README structure

`README.md` is 271 lines with 13 top-level sections.

**CLI section** at @README.md:111-118 — a single fenced block lists user-facing subcommands in `tmuxlet <verb> <id>` style. This is the insertion point for a `prune` line. `bridge` is intentionally absent from this list (it's documented only as an internal helper in "How Print Mode Works").

**Runtime State section** at @README.md:216-232 — documents `~/.tmuxlet/runs/<run-id>/` directory contents and lifetime. This is where `prune`'s "deletes the run dir" semantics naturally fit.

**Output And Failures section** at @README.md:254-260 — lists only the three nonzero statuses (`blocked`, `timeout`, `exited`). `completed` is NOT currently named as a status in README anywhere. Any mention of "completed runs auto-kill the session" needs to introduce the term, not just edit an existing bullet.

**No "Cleanup" section exists.** The plan can choose to add one between "Output And Failures" (ends line 264) and "Development" (starts line 265), or weave the new info into the three sections above.

## Code References

- @src/main.rs:94-105 — `RunPaths` struct (marker file paths)
- @src/main.rs:107-115 — `RunResult` struct
- @src/main.rs:126-134 — `main()` error-formatting wrapper
- @src/main.rs:143-168 — `run()` subcommand dispatch
- @src/main.rs:285 — `tmux_session = format!("tmuxlet-{run_id}")`
- @src/main.rs:326-415 — inline polling loop in `run_print` (the four terminal paths)
- @src/main.rs:330 — `capture_pane_to_file(...).ok()` on the completed path (auto-kill anchor)
- @src/main.rs:332-342 — `RunResult` construction for completed status
- @src/main.rs:360-377 — blocked path
- @src/main.rs:382-396 — timeout path
- @src/main.rs:385 — `"timeout waiting for explicit completion\n"` literal
- @src/main.rs:399-411 — exited path
- @src/main.rs:924-931 — `blocked_error_message(...)` (first line: `"blocked waiting for a confirmation or permission prompt"`)
- @src/main.rs:933-939 — `tmux_session_exists(name)`
- @src/main.rs:958-972 — `run_tmux<I, S>(args)`
- @src/main.rs:1093-1117 — `handle_status`
- @src/main.rs:1119-1125 — `heartbeat_age_secs`
- @src/main.rs:1171-1194 — `handle_stop` (existing `kill-session` pattern at 1185-1189)
- @src/main.rs:1196-1202 — `tmux_name_from_id`
- @src/main.rs:1242-1244 — `result_status_code`
- @src/main.rs:1769-1776 — `make_run_id`
- @src/main.rs:1772-1777 — `state_dir()` (TMUXLET_HOME resolution)
- @src/main.rs:1779-1795 — `run_paths(run_id)`
- @src/main.rs:1841-2349 — `#[cfg(test)] mod tests`
- @src/main.rs:1845-1848 — `parse` test helper
- @src/main.rs:1850-1853 — `has_pair` test helper
- @src/main.rs:2336-2348 — `write_complete_once_is_idempotent` (only fs-touching test)
- @README.md:111-118 — CLI subcommand list
- @README.md:216-232 — Runtime State
- @README.md:254-260 — Output And Failures
- @Cargo.toml — empty `[dependencies]`, no `[dev-dependencies]`

## Architecture Documentation

- **Single-file convention**: All code (including tests) lives in `src/main.rs`. No `lib.rs`, no modules. The implementation plan stays in this file.
- **No external crates**: Stdlib only. New code must avoid pulling in `tempfile`, `clap`, `serde`, etc. — they would be the first deps.
- **Result type**: Handlers return `Result<(), String>`. Errors propagate to `main()` which prints `tmuxlet: {err}` to stderr with exit code 1.
- **Tmux call style**: `run_tmux` is the single primitive. Fire-and-forget uses `.ok()`; required success uses `?`.
- **Status path ergonomics**: `handle_status` currently dumps `meta.json` raw — it does NOT print the classified status (completed/blocked/timeout/exited). Refactoring it to use a shared `discover_runs` helper would add classification as a side benefit, but the spec doesn't require any output change to `status` — keep behavior identical to avoid scope creep.

## Historical Context (from thoughts/)

No `thoughts/` directory exists in this repository. The only prior design context for this work is the design spec itself at `docs/superpowers/specs/2026-05-26-tmux-session-cleanup-design.md` (commit `2c92558`).

## Findings Files

Full agent findings are preserved for deeper reference:
- @docs/superpowers/research/findings/2026-05-26-tmux-session-cleanup/codebase-analyzer-run-completion.md — print-mode run completion lifecycle
- @docs/superpowers/research/findings/2026-05-26-tmux-session-cleanup/codebase-analyzer-cli-dispatch.md — CLI dispatch and existing subcommand handlers
- @docs/superpowers/research/findings/2026-05-26-tmux-session-cleanup/codebase-analyzer-test-patterns.md — test conventions and helpers
- @docs/superpowers/research/findings/2026-05-26-tmux-session-cleanup/codebase-analyzer-readme-structure.md — README structure

## Related Research

- @docs/superpowers/specs/2026-05-26-tmux-session-cleanup-design.md — design spec being implemented

## Open Questions

None. All facts needed for the implementation plan are documented above.
