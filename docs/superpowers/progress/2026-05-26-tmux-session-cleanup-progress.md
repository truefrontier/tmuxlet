---
plan: docs/superpowers/plans/2026-05-26-tmux-session-cleanup.md
started: 2026-05-26
status: in_progress
---

# Implementation Progress: tmux session cleanup

**Plan**: `docs/superpowers/plans/2026-05-26-tmux-session-cleanup.md`
**Started**: 2026-05-26
**Branch**: `claude-hooks`
**Starting commit**: `3023181` (revised plan committed)

---

## Phase 1: Auto-kill on `completed`

**Completed**: 2026-05-26
**Status**: COMPLETE
**Commits**: `d685df5`
**Tests**: PASS (44/44 existing tests; no new tests this phase)

### Summary

Inserted a 5-line `run_tmux(["kill-session", "-t", &tmux_session])` call into `run_print`'s polling loop, placed between the existing `capture_pane_to_file(...)` call (line 330) and the `return Ok(RunResult { ... })` on the `completed` path. The kill is best-effort (`let _ = ...`), borrows `&tmux_session` before its move into `RunResult`, and leaves the `~/.tmuxlet/runs/<id>/` directory untouched. The `timeout`, `blocked`, and `exited` return paths are not modified, so investigation flows still work. `handle_status` was not touched, per the plan's explicit override of the design spec.

### Verification

- `cargo build` clean (no warnings)
- `cargo fmt -- --check` clean
- `cargo test` — 44/44 existing tests pass

### Manual smoke (deferred to human)

These require a live tmux + Claude CLI environment, which the implementation agent did not have. To verify at the end of the run:

1. `cargo run -- -p "say ready"` → exits with status "completed"
2. Immediately after: `tmux ls 2>/dev/null | grep tmuxlet-` → no output for this run's id
3. `~/.tmuxlet/runs/<id>/` still contains `complete.txt`, `pane.log`, `meta.json`, `heartbeat`
4. Timeout path: `cargo run -- --timeout 5 -p "sleep 60"` → `tmux ls` still shows the session

---

## Phase 2: Helpers + `prune` command

**Completed**: 2026-05-26
**Status**: COMPLETE
**Commits**: `d57b48d`, `28cba2d`, `7acdba3`, `6330311`, `96b73ca`, `da1f0fc`, `40563ac`, `fa8e5c3`, `9a01b04`
**Tests**: PASS (74 total: 44 existing + 30 new; all spec-required tests #8/#9/#10 automated)

### Summary

Added the full `prune` command and all supporting infrastructure: `RunClassification`/`RunRecord` types, `classify_from_markers`/`classify_run` helpers, `discover_runs`/`discover_runs_in` enumeration (with a test seam), `PruneArgs`/`parse_prune_args` parser, `prune_selection` pure selection, `execute_prune`/`PruneOutcome` deletion helper, `short_id`/`format_age_secs` formatters, the interactive `handle_prune` handler, dispatch wire-up, and the `print_help` usage-line update. All in `src/main.rs`. No new deps, no `#[allow(dead_code)]`, `handle_status` untouched. Two small extra tests (`classification_label_returns_expected_strings`, `run_record_can_be_constructed_and_inspected`) brought the new test count to 30.

### Verification

- `cargo build` clean (no warnings)
- `cargo fmt -- --check` clean
- `cargo test` — 74/74 pass (44 existing + 30 new)
- `cargo run -- --help | grep prune` shows `tmuxlet <status|read|send|attach|stop|prune>`
- `TMUXLET_HOME=$(mktemp -d) cargo run -- prune` → `Nothing to prune.` (exit 0)
- `TMUXLET_HOME=$(mktemp -d) cargo run -- prune --dry-run` → `Nothing to prune.` (exit 0)
- `cargo run -- prune --no-such-flag` → `tmuxlet: unknown option for prune: --no-such-flag` (exit 1)

### Manual fixture tests (deferred to human)

Set up `TMUXLET_HOME=/tmp/tmuxlet-test-prune` and create three fixture run-dirs, then exercise `prune --dry-run`, `prune -y`, `prune <id>` with `n` and `y` responses, and `prune nonexistent`. Full steps in the agent's report.

---
