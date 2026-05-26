# tmux session cleanup — design

**Date:** 2026-05-26
**Status:** Approved, ready for implementation plan
**Scope:** Address stale tmux sessions that accumulate when `tmuxlet` runs finish, exit on timeout, or get blocked at a confirmation prompt.

## Problem

Each `tmuxlet -p ...` invocation creates a tmux session named `tmuxlet-<run-id>` and a run directory at `~/.tmuxlet/runs/<run-id>/`. Today the tmux session is only killed by an explicit `tmuxlet stop <id>` call. Every other terminal status leaves the session alive:

| Status | Path in `wait_for_completion` | Today |
|---|---|---|
| `completed` | `complete.txt` exists (line 327 in `src/main.rs`) | Session stays alive |
| `timeout` | Run exceeded `opts.timeout` (line 382) | Session stays alive |
| `blocked` | Pane stalled at a confirmation gate (line 360) | Session stays alive |
| `exited` | tmux session disappeared on its own (line 399) | N/A — already dead |

Each wrapped CLI (`claude`, `codex`, etc.) idling in a leftover session consumes hundreds of MB of resident memory. Run directories never get cleaned up either, so `tmuxlet status` output grows without bound.

## Goals

1. **Stop the most common leak.** When a run finishes cleanly (`completed`), kill the tmux session before returning.
2. **Give the user explicit cleanup.** Add `tmuxlet prune` so the user can sweep up everything in terminal state (including timeout/blocked sessions they've finished investigating) and reclaim the filesystem.
3. **Preserve long-running tasks.** No automatic time-based reaping, ever. Long jobs are a first-class use case.
4. **Preserve investigation workflow.** Timeout and blocked sessions stay alive after the run returns, so the user can `attach`/`read`/`send` to figure out what went wrong.

## Non-goals

- Auto-reaping on a timer, on a TTL, or on every `tmuxlet run` invocation.
- A `--keep-session` opt-out on `run` for the `completed`-status auto-kill. We're keeping the auto-kill unconditional for the clean-success path.
- Killing or pruning runs that are still in progress. `prune` refuses non-terminal IDs and tells the user to use `tmuxlet stop` instead.
- Integration tests against real tmux. Existing test style is pure-function only; we'll match it.

## Change 1 — auto-kill on `completed`

In `run_print` → `wait_for_completion`, when the loop returns the `RunResult` with status `"completed"`, kill the tmux session before returning. Best-effort: ignore any error from `kill-session` (the session might have died between the `complete.txt` check and our kill call).

Capture order matters: the existing code already calls `capture_pane_to_file` immediately before returning the `completed` result (line 330), so pane forensics are preserved.

`timeout`, `blocked`, and `exited` paths are unchanged.

### Implementation sketch

```rust
// after the existing capture_pane_to_file call, before building the RunResult
let _ = run_tmux([
    OsStr::new("kill-session"),
    OsStr::new("-t"),
    OsStr::new(&tmux_session),
]);
```

## Change 2 — `tmuxlet prune` command

### CLI surface

```
tmuxlet prune [<id>...] [-y|--yes] [--dry-run]
```

### Selection

- **No IDs** — every run in `~/.tmuxlet/runs/` whose status is terminal.
- **IDs given** — only those runs. Each must resolve to an existing run_dir AND be in terminal status. A non-terminal ID is a hard error (exit 1) listing which IDs failed and suggesting `tmuxlet stop`.

A run is **terminal** if its `meta.json` exists AND any of:
- `complete.txt` exists (→ `completed`)
- `error.txt` exists (→ `timeout` or `blocked`, distinguished by the first line: timeout starts with `"timeout "`, blocked starts with `"blocked "`)
- the tmux session `tmuxlet-<id>` no longer exists, AND neither `complete.txt` nor `error.txt` is present (→ `exited`)

A run with `meta.json` but none of those markers is **in-progress** and is left alone.

A run_dir without `meta.json` is skipped silently — treat it as not-yet-initialized, never user-visible.

### Action per pruned run

1. If `tmuxlet-<id>` tmux session exists (typical for `timeout`/`blocked`), `kill-session` it. Best-effort.
2. `fs::remove_dir_all` the run_dir.

### Confirmation flow

- **Default:** list the runs that will be pruned (id, status, age, session-alive?), prompt `Proceed? [y/N]`. Anything other than `y`/`Y`/`yes` aborts with exit 0.
- **`-y` / `--yes`:** skip the prompt.
- **`--dry-run`:** list what would be pruned, do nothing, exit 0. Never prompts.
- **`-y` + `--dry-run`:** `--dry-run` wins (no action). Document this in `--help`.

### Output

```
$ tmuxlet prune
Found 3 runs to prune:
  abc12345  completed  2h ago     (session: gone)
  def67890  blocked    15m ago    (session: alive)
  ghi54321  timeout    1d ago     (session: gone)
Proceed? [y/N] y
Pruned 3 runs.
```

- Dry-run last line: `Would prune 3 runs.`
- No matches: `Nothing to prune.` exit 0.
- Per-run delete failure: print warning, continue, exit 1 at the end with summary count.

### Edge cases

| Case | Behavior |
|---|---|
| `~/.tmuxlet/runs/` missing or empty | `Nothing to prune.` exit 0 |
| Run_dir without `meta.json` | Skip silently |
| Explicit ID with no run_dir | Exit 1, `unknown run: <id>` |
| Explicit ID, run is in-progress | Exit 1, message names the IDs and suggests `tmuxlet stop` |
| `kill-session` fails | Continue with `remove_dir_all`; tolerate |
| `remove_dir_all` fails | Print warning, continue with others, exit 1 at end |

## Code organization

Single file: `src/main.rs` (matches the project's existing one-file convention).

- Add `fn handle_prune(args: &[String]) -> Result<(), String>` near `handle_stop`.
- Dispatch from `run()` match: `"prune" => handle_prune(&args[1..])`.
- Extract a small helper, `fn discover_runs() -> Vec<RunRecord>`, returning per-run `{ id, classification, age_secs, session_alive }`. `classification` is an enum: `Completed | Timeout | Blocked | Exited | InProgress`.
- Refactor `handle_status` to use the same helper (small, focused improvement on code we're already touching — fits CLAUDE.md's guidance).

## Tests

Inline `#[cfg(test)]` in `src/main.rs`, following existing pattern. Pure-function only — no live tmux.

1. `parses_prune_no_args` — `prune` parses to "all-terminal" mode.
2. `parses_prune_with_ids` — IDs are captured.
3. `parses_prune_dry_run` — flag handled.
4. `parses_prune_yes` — flag handled.
5. `parses_prune_dry_run_overrides_yes` — when both are present, no action is taken.
6. `parses_prune_unknown_flag` — returns error.
7. `classify_run_status` — given fixture run_dirs with various marker files in a tempdir, helper returns the right classification (including `InProgress`).
8. `discover_runs_filters_terminal` — mixed tempdir, "no IDs" selection returns the terminal subset only.
9. `prune_rejects_non_terminal_id` — explicit ID for an in-progress run errors with the expected message.
10. `dry_run_does_not_delete` — tempdir fixtures survive `--dry-run`.

## Docs

- `README.md`: add `prune` to the subcommand list near `status`/`stop`. One example block. Brief note: `completed` runs auto-kill; `timeout`/`blocked` stay alive for `attach`/`read`/`send`; use `prune` to clean them up when you're done investigating.
- `CLAUDE.md`: no changes — agent rules don't reference specific subcommands.

## Open questions

None. Decisions captured in the brainstorming session on 2026-05-26.
