# tmux session cleanup — implementation plan

**Date:** 2026-05-26
**Author:** Kevin Kirchner (Claude)
**Spec:** [docs/superpowers/specs/2026-05-26-tmux-session-cleanup-design.md](../specs/2026-05-26-tmux-session-cleanup-design.md)
**Research:** [docs/superpowers/research/2026-05-26-tmux-session-cleanup.md](../research/2026-05-26-tmux-session-cleanup.md)
**Branch:** `claude-hooks` (continuing current branch — no separate feature branch)

## Overview

Implement the two changes from the approved design spec in three phases:

1. **Phase 1** — Auto-kill the tmux session on the `completed` print-mode return path. Smallest behavior change (~5 line insertion), independent of any helper.
2. **Phase 2** — Add `RunClassification`, `RunRecord`, the classification helpers, the `prune_selection` pure function, the `execute_prune` deletion helper, the `tmuxlet prune` subcommand (parser, handler, dispatch, help text), and all unit tests in one cohesive change.
3. **Phase 3** — Update `README.md` and run final `cargo fmt` / `cargo test` / `cargo build` verification.

`handle_status` is **not** refactored to use the new helpers. The spec called out the refactor as a "small, focused improvement on code we're already touching," but `handle_status` currently prints `meta.json` text plus heartbeat age (not a classification), and routing it through `discover_runs` would either change its observable output or require `RunRecord` to carry raw `meta.json` text — both expand scope. Per CLAUDE.md "don't refactor beyond what the task requires," `handle_status` is left untouched and `discover_runs` is used solely by `handle_prune`.

## Current State Analysis

The print-mode polling loop is inline within `run_print` at @src/main.rs:326-415 with four terminal return paths: `completed` (327-342), `blocked` (360-377), `timeout` (382-396), `exited` (399-411). Today, **only `handle_stop` calls `kill-session`** — every other terminal path leaves the tmux session alive, and even `handle_stop` does not delete the `~/.tmuxlet/runs/<id>/` directory.

Subcommand dispatch is a single match in `run()` at @src/main.rs:143-168, with no shared flag parser. `parse_options` (171-237) is the print-mode parser and is unused by handler subcommands. There is no existing classification helper for run directories; the four status strings are produced inline in the polling loop.

Tests live in `#[cfg(test)] mod tests` at @src/main.rs:1841-2349 — 44 tests, stdlib-only, no `tempfile` crate, two test helpers (`parse`, `has_pair`), one fs-touching test (`write_complete_once_is_idempotent`) that isolates via `env::temp_dir().join(make_run_id())`.

`Cargo.toml` has zero runtime and zero dev dependencies. New code must avoid pulling in `clap`, `tempfile`, etc.

## Desired End State

After all three phases:

- A successful `tmuxlet -p ...` run cleans up its tmux session before returning. The `~/.tmuxlet/runs/<id>/` directory remains (so callers can still inspect `complete.txt`, `pane.log`, etc.).
- `timeout` and `blocked` runs leave both the tmux session and run-dir intact (so the user can `attach`, `read`, `send`, or investigate before `prune`).
- `tmuxlet prune` cleans up everything that's terminal: kills any alive tmux sessions belonging to terminal runs, then `fs::remove_dir_all`s the run-dir. Refuses non-terminal IDs.
- The `prune` command supports: no-args (sweep all terminal), explicit IDs, `-y`/`--yes` to skip confirmation, `--dry-run` to preview. `--dry-run` overrides `-y`.
- `cargo test` passes including ~20 new tests covering classification, selection, deletion-with-dry-run, and prune argument parsing.
- `README.md` documents `prune` in the CLI section and mentions the auto-kill behavior.

### Verification:

- `cargo fmt -- --check` clean
- `cargo build` succeeds with no warnings
- `cargo test` all tests pass
- `cargo run -- --help` shows `prune` in the usage line
- `TMUXLET_HOME=$(mktemp -d) cargo run -- prune --dry-run` prints `Nothing to prune.` and exits 0

## Key Discoveries

- **No `wait_for_completion` function** — the polling loop is inline in `run_print`. The spec's reference to that function name is approximate; the actual insertion point for auto-kill is @src/main.rs:331 (between the existing `capture_pane_to_file().ok()` at 330 and the `return Ok(RunResult { ... })` at 331-342).
- **`tmux_session` is moved into `RunResult`** at line 338 via field shorthand. The auto-kill must reference `&tmux_session` BEFORE that move — i.e., between lines 330 and 331.
- **`handle_stop` already gates on `tmux_session_exists`** (lines 933-939) and the kill pattern there (1185-1189) is the model to follow.
- **`error.txt` first-line prefixes are exactly `"timeout "` and `"blocked "`** — see `"timeout waiting for explicit completion\n"` at @src/main.rs:385 and `blocked_error_message` first line at @src/main.rs:924-931 (`"blocked waiting for a confirmation or permission prompt"`).
- **Single fs-test pattern: `env::temp_dir().join(make_run_id())`** — see `write_complete_once_is_idempotent` at @src/main.rs:2336-2348. Used by all new tempdir tests in Phase 2.
- **`make_run_id()`** at @src/main.rs:1769-1776 produces unique nanos+pid slugs, so concurrent test runs don't collide on `env::temp_dir()`.
- **`print_help`** at @src/main.rs:1832-1839 inlines a `<status|read|send|attach|stop>` token in its usage line. Phase 2 updates this.
- **Testability requires seams**: `discover_runs` cannot be tested without a `discover_runs_in(runs_root: &Path)` parameterized variant; `handle_prune` cannot be tested as a whole, so the selection and deletion logic must be factored into pure (`prune_selection`) and root-parameterized (`execute_prune`) helpers.

## What We're NOT Doing

- **Refactoring `handle_status`** to use `discover_runs` (would force a behavior change to its output, or muddy the helper's signature). The spec mentioned this; the plan defers it. **The implementer agent for Phase 2 must NOT modify `handle_status` — this plan's "What We're NOT Doing" overrides the spec on this point.**
- **Refactoring or changing `handle_stop`'s** behavior (it still does not delete run dirs; that's `prune`'s job by design).
- **Adding `clap` or any other dep.** All flag parsing remains hand-rolled.
- **Adding integration tests against real tmux.** Matches the existing test posture (pure-function only). Manual verification covers tmux-coupled behavior.
- **Adding any time-based auto-reaper, TTL, or cron-style cleanup.** Long-running runs are first-class and explicitly preserved.
- **Adding a `--keep-session` opt-out** on print-mode for the auto-kill. The auto-kill on `completed` is unconditional.
- **Changing the `completed` exit code, status string, or `RunResult` fields.**
- **Changing the `~/.tmuxlet/runs/<id>/` directory layout or marker filenames.**

## Implementation Approach

Three phases, gate manual verification between each.

- **Phase 1** is the smallest behavior change (auto-kill) — independent, low-risk, can be merged first.
- **Phase 2** is the bulk of the work: all new types, helpers, the handler, dispatch, help text, and ~20 tests. Every helper that needs testability gets a parameterized seam (e.g., `discover_runs_in(runs_root)`), so every spec-required test is automated.
- **Phase 3** is docs + final sanity pass.

Within each phase, the implementer should commit per logical unit (types together, then a helper + its tests, then the handler, etc.) — never amass all changes into a single commit.

---

## Phase 1: Auto-kill on `completed`

### Overview

Insert the `kill-session` call into the `completed` return path of `run_print`'s polling loop. The single behavior change is: a successful run no longer leaks its tmux session.

### Changes Required:

#### 1. Insert `kill-session` after `capture_pane_to_file` on the completed path

**File**: `src/main.rs`
**Location**: @src/main.rs:330-331, between the existing `capture_pane_to_file(&tmux_session, &paths.pane_log).ok();` and the `return Ok(RunResult { ... })`.

**Before** (lines 327-342):
```rust
if paths.complete.exists() {
    let output = fs::read_to_string(&paths.complete)
        .map_err(|e| format!("failed to read completion file: {e}"))?;
    capture_pane_to_file(&tmux_session, &paths.pane_log).ok();
    return Ok(RunResult {
        id: run_id,
        target: opts.target,
        status: "completed".into(),
        output,
        output_format: opts.output_format,
        cwd: opts.cwd,
        tmux_session,
        completion_source: "explicit".into(),
        elapsed_ms: started.elapsed().as_millis(),
    });
}
```

**After**:
```rust
if paths.complete.exists() {
    let output = fs::read_to_string(&paths.complete)
        .map_err(|e| format!("failed to read completion file: {e}"))?;
    capture_pane_to_file(&tmux_session, &paths.pane_log).ok();
    let _ = run_tmux([
        OsStr::new("kill-session"),
        OsStr::new("-t"),
        OsStr::new(&tmux_session),
    ]);
    return Ok(RunResult {
        id: run_id,
        target: opts.target,
        status: "completed".into(),
        output,
        output_format: opts.output_format,
        cwd: opts.cwd,
        tmux_session,
        completion_source: "explicit".into(),
        elapsed_ms: started.elapsed().as_millis(),
    });
}
```

Note: `&tmux_session` is borrowed BEFORE the move into `RunResult { ..., tmux_session, ... }` at line 338, so ownership semantics are preserved.

### Success Criteria:

#### Automated Verification:

- [x] `cargo build` succeeds
- [x] `cargo fmt -- --check` clean
- [x] `cargo test` all 44 existing tests still pass (no new tests added in this phase — the change is too tmux-coupled for a pure unit test)

#### Manual Verification:

- [ ] `cargo run -- -p "say ready"` returns successfully with status "completed"
- [ ] **Immediately after** the run returns, `tmux ls 2>/dev/null | grep tmuxlet-` shows **no** session for that run (vs. before this phase, where the session would persist)
- [ ] The `~/.tmuxlet/runs/<id>/` directory is still present and contains `complete.txt`, `pane.log`, `meta.json`, `heartbeat` (run-dir is not deleted by auto-kill)
- [ ] If a print-mode run times out (`cargo run -- --timeout 5 -p "sleep 60"`), the tmux session is still alive afterward and `tmux ls` shows it (timeout/blocked paths are unchanged)

**Implementation Note**: After Phase 1 passes automated verification AND manual smoke (`-p "say ready"` then `tmux ls`), pause for manual confirmation before proceeding to Phase 2.

---

## Phase 2: Helpers + `prune` command (everything else, with tests)

### Overview

Add all the new types, classification helpers, selection and deletion seams, the `tmuxlet prune` subcommand, the dispatch + help-text updates, and all unit tests — in a single phase. Each piece is committed as a separate logical unit. Every spec-required test is automated.

**CRITICAL CONSTRAINT for the implementer**: Do NOT modify `handle_status`. This plan's "What We're NOT Doing" overrides the spec's "refactor handle_status to use the same helper" line. `discover_runs` is used by `handle_prune` only.

### Changes Required:

#### 1. Add `RunClassification` enum

**File**: `src/main.rs`
**Location**: After the `RunResult` struct (around line 118).

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunClassification {
    Completed,
    Timeout,
    Blocked,
    Exited,
    InProgress,
}

impl RunClassification {
    fn label(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Timeout => "timeout",
            Self::Blocked => "blocked",
            Self::Exited => "exited",
            Self::InProgress => "in-progress",
        }
    }

    fn is_terminal(self) -> bool {
        !matches!(self, Self::InProgress)
    }
}
```

#### 2. Add `RunRecord` struct

**File**: `src/main.rs`
**Location**: Immediately after the `RunClassification` block.

```rust
#[derive(Debug)]
struct RunRecord {
    id: String,
    classification: RunClassification,
    age_secs: Option<u64>,
    session_alive: bool,
}
```

#### 3. Add `classify_from_markers` (pure)

**File**: `src/main.rs`
**Location**: Just before `tmux_session_exists` at @src/main.rs:933 (so related functions are grouped).

```rust
fn classify_from_markers(
    complete_exists: bool,
    error_first_line: Option<&str>,
    session_alive: bool,
) -> RunClassification {
    if complete_exists {
        return RunClassification::Completed;
    }
    if let Some(line) = error_first_line {
        if line.starts_with("timeout ") {
            return RunClassification::Timeout;
        }
        if line.starts_with("blocked ") {
            return RunClassification::Blocked;
        }
        // Unknown error.txt content — treat as Blocked (still terminal).
        return RunClassification::Blocked;
    }
    if !session_alive {
        return RunClassification::Exited;
    }
    RunClassification::InProgress
}
```

#### 4. Add `classify_run` (I/O wrapper)

**File**: `src/main.rs`
**Location**: Immediately after `classify_from_markers`.

```rust
fn classify_run(run_dir: &Path, session_alive: bool) -> Option<RunClassification> {
    if !run_dir.join("meta.json").exists() {
        return None;
    }
    let complete = run_dir.join("complete.txt").exists();
    let error_path = run_dir.join("error.txt");
    let error_first_line = if error_path.exists() {
        fs::read_to_string(&error_path)
            .ok()
            .and_then(|s| s.lines().next().map(str::to_string))
    } else {
        None
    };
    Some(classify_from_markers(
        complete,
        error_first_line.as_deref(),
        session_alive,
    ))
}
```

#### 5. Add `discover_runs` + `discover_runs_in` (test seam)

**File**: `src/main.rs`
**Location**: Immediately after `classify_run`.

```rust
fn discover_runs() -> Vec<RunRecord> {
    discover_runs_in(&state_dir().join("runs"))
}

fn discover_runs_in(runs_root: &Path) -> Vec<RunRecord> {
    let entries = match fs::read_dir(runs_root) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    let mut records = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let id = match path.file_name().and_then(|n| n.to_str()) {
            Some(name) => name.to_string(),
            None => continue,
        };
        let session_alive = tmux_session_exists(&format!("tmuxlet-{id}"));
        let Some(classification) = classify_run(&path, session_alive) else {
            continue;
        };
        let age_secs = heartbeat_age_secs(&path.join(HEARTBEAT_FILENAME));
        records.push(RunRecord {
            id,
            classification,
            age_secs,
            session_alive,
        });
    }

    records.sort_by(|a, b| a.id.cmp(&b.id));
    records
}
```

Note: `discover_runs_in` still calls `tmux_session_exists` (a real `tmux has-session` shell-out). For tests, this means any test fixture's session check returns `false` (no real tmux session for the fake run id), so a fixture with only `meta.json` will classify as `Exited` rather than `InProgress`. That's acceptable — the `InProgress` classification path is exercised in pure unit tests of `classify_from_markers`. Discovery tests focus on enumeration and meta.json-presence filtering.

#### 6. Add `PruneArgs` struct and `parse_prune_args` (pure parser)

**File**: `src/main.rs`
**Location**: Near `parse_options` (around line 237).

```rust
#[derive(Debug, Default, PartialEq, Eq)]
struct PruneArgs {
    ids: Vec<String>,
    yes: bool,
    dry_run: bool,
}

fn parse_prune_args(args: &[String]) -> Result<PruneArgs, String> {
    let mut out = PruneArgs::default();
    for arg in args {
        match arg.as_str() {
            "-y" | "--yes" => out.yes = true,
            "--dry-run" => out.dry_run = true,
            other if other.starts_with('-') => {
                return Err(format!("unknown option for prune: {other}"));
            }
            other => out.ids.push(other.to_string()),
        }
    }
    Ok(out)
}
```

#### 7. Add `prune_selection` (pure selection logic — the seam for spec test #9)

**File**: `src/main.rs`
**Location**: Immediately after `parse_prune_args`.

```rust
fn prune_selection(
    records: Vec<RunRecord>,
    explicit_ids: &[String],
) -> Result<Vec<RunRecord>, String> {
    if explicit_ids.is_empty() {
        return Ok(records
            .into_iter()
            .filter(|r| r.classification.is_terminal())
            .collect());
    }

    use std::collections::HashMap;
    let mut by_id: HashMap<String, RunRecord> = HashMap::new();
    for r in records {
        by_id.insert(r.id.clone(), r);
    }

    let mut found: Vec<RunRecord> = Vec::new();
    let mut unknown: Vec<String> = Vec::new();
    let mut non_terminal: Vec<String> = Vec::new();
    for id in explicit_ids {
        match by_id.remove(id) {
            None => unknown.push(id.clone()),
            Some(r) if !r.classification.is_terminal() => non_terminal.push(r.id.clone()),
            Some(r) => found.push(r),
        }
    }
    if !unknown.is_empty() {
        return Err(format!("unknown run(s): {}", unknown.join(", ")));
    }
    if !non_terminal.is_empty() {
        return Err(format!(
            "run(s) still in progress; use `tmuxlet stop <id>` first: {}",
            non_terminal.join(", ")
        ));
    }
    Ok(found)
}
```

#### 8. Add `execute_prune` (deletion seam for spec test #10)

**File**: `src/main.rs`
**Location**: Immediately after `prune_selection`.

```rust
struct PruneOutcome {
    deleted: usize,
    failures: Vec<String>,
}

fn execute_prune(selected: &[RunRecord], runs_root: &Path, dry_run: bool) -> PruneOutcome {
    if dry_run {
        return PruneOutcome {
            deleted: 0,
            failures: Vec::new(),
        };
    }
    let mut deleted = 0usize;
    let mut failures = Vec::new();
    for rec in selected {
        if rec.session_alive {
            let session_name = format!("tmuxlet-{}", rec.id);
            let _ = run_tmux([
                OsStr::new("kill-session"),
                OsStr::new("-t"),
                OsStr::new(&session_name),
            ]);
        }
        let dir = runs_root.join(&rec.id);
        match fs::remove_dir_all(&dir) {
            Ok(()) => deleted += 1,
            Err(e) => {
                eprintln!("tmuxlet: failed to remove {}: {e}", dir.display());
                failures.push(rec.id.clone());
            }
        }
    }
    PruneOutcome { deleted, failures }
}
```

#### 9. Add `short_id` and `format_age_secs` helpers

**File**: `src/main.rs`
**Location**: Immediately after `execute_prune`.

```rust
fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

fn format_age_secs(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}
```

#### 10. Add `handle_prune`

**File**: `src/main.rs`
**Location**: Immediately after `handle_stop` at @src/main.rs:1194 (so all `handle_*` functions are clustered).

```rust
fn handle_prune(args: &[String]) -> Result<(), String> {
    let parsed = parse_prune_args(args)?;
    let runs = discover_runs();
    let selected = prune_selection(runs, &parsed.ids)?;

    if selected.is_empty() {
        println!("Nothing to prune.");
        return Ok(());
    }

    // List proposed prunes.
    let verb = if parsed.dry_run { "Would prune" } else { "Found" };
    println!("{verb} {} run(s):", selected.len());
    for rec in &selected {
        let age = rec
            .age_secs
            .map(format_age_secs)
            .unwrap_or_else(|| "-".to_string());
        let session = if rec.session_alive { "alive" } else { "gone" };
        println!(
            "  {:<10} {:<11} {:<10} (session: {})",
            short_id(&rec.id),
            rec.classification.label(),
            age,
            session
        );
    }

    if parsed.dry_run {
        return Ok(());
    }

    if !parsed.yes {
        print!("Proceed? [y/N] ");
        io::stdout().flush().ok();
        let mut response = String::new();
        io::stdin()
            .read_line(&mut response)
            .map_err(|e| format!("failed to read confirmation: {e}"))?;
        let trimmed = response.trim().to_lowercase();
        if !matches!(trimmed.as_str(), "y" | "yes") {
            println!("Aborted.");
            return Ok(());
        }
    }

    let runs_root = state_dir().join("runs");
    let outcome = execute_prune(&selected, &runs_root, false);

    println!("Pruned {} run(s).", outcome.deleted);
    if !outcome.failures.is_empty() {
        return Err(format!(
            "failed to remove {} run(s)",
            outcome.failures.len()
        ));
    }
    Ok(())
}
```

#### 11. Wire dispatch in `run()`

**File**: `src/main.rs`
**Location**: @src/main.rs:143-168, inside the existing `match args[0].as_str()`. Add `"prune"` next to `"stop"`.

**Before**:
```rust
"stop" => handle_stop(&args[1..]),
_ => {
```

**After**:
```rust
"stop" => handle_stop(&args[1..]),
"prune" => handle_prune(&args[1..]),
_ => {
```

#### 12. Update `print_help`

**File**: `src/main.rs`
**Location**: @src/main.rs:1832-1839. Change the usage line.

**Before**: `tmuxlet <status|read|send|attach|stop>`

**After**: `tmuxlet <status|read|send|attach|stop|prune>`

(Only that token changes — the rest of the help text stays put.)

#### 13. Add unit tests

**File**: `src/main.rs`
**Location**: Inside `mod tests`, grouped together near the end of the module.

```rust
fn prune_argv(args: &[&str]) -> PruneArgs {
    let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    parse_prune_args(&owned).unwrap()
}

fn record(id: &str, c: RunClassification, alive: bool) -> RunRecord {
    RunRecord {
        id: id.to_string(),
        classification: c,
        age_secs: None,
        session_alive: alive,
    }
}

// ---------- Classification ----------

#[test]
fn classify_completed_when_complete_txt_present() {
    assert_eq!(
        classify_from_markers(true, None, true),
        RunClassification::Completed
    );
    assert_eq!(
        classify_from_markers(true, Some("timeout ..."), false),
        RunClassification::Completed
    );
}

#[test]
fn classify_timeout_when_error_starts_with_timeout() {
    assert_eq!(
        classify_from_markers(
            false,
            Some("timeout waiting for explicit completion"),
            true,
        ),
        RunClassification::Timeout
    );
}

#[test]
fn classify_blocked_when_error_starts_with_blocked() {
    assert_eq!(
        classify_from_markers(
            false,
            Some("blocked waiting for a confirmation or permission prompt"),
            true,
        ),
        RunClassification::Blocked
    );
}

#[test]
fn classify_blocked_for_unknown_error_content() {
    assert_eq!(
        classify_from_markers(false, Some("some other error"), true),
        RunClassification::Blocked
    );
}

#[test]
fn classify_exited_when_session_dead_no_markers() {
    assert_eq!(
        classify_from_markers(false, None, false),
        RunClassification::Exited
    );
}

#[test]
fn classify_in_progress_when_session_alive_no_markers() {
    assert_eq!(
        classify_from_markers(false, None, true),
        RunClassification::InProgress
    );
}

#[test]
fn classification_is_terminal_for_all_except_in_progress() {
    assert!(RunClassification::Completed.is_terminal());
    assert!(RunClassification::Timeout.is_terminal());
    assert!(RunClassification::Blocked.is_terminal());
    assert!(RunClassification::Exited.is_terminal());
    assert!(!RunClassification::InProgress.is_terminal());
}

#[test]
fn classify_run_returns_none_without_meta_json() {
    let dir = env::temp_dir().join(make_run_id());
    fs::create_dir_all(&dir).unwrap();
    let result = classify_run(&dir, true);
    fs::remove_dir_all(&dir).ok();
    assert_eq!(result, None);
}

#[test]
fn classify_run_reads_error_first_line() {
    let dir = env::temp_dir().join(make_run_id());
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("meta.json"), "{}").unwrap();
    fs::write(
        dir.join("error.txt"),
        "timeout waiting for explicit completion\nextra context line\n",
    )
    .unwrap();
    let result = classify_run(&dir, false);
    fs::remove_dir_all(&dir).ok();
    assert_eq!(result, Some(RunClassification::Timeout));
}

#[test]
fn classify_run_completed_takes_priority_over_session_state() {
    let dir = env::temp_dir().join(make_run_id());
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("meta.json"), "{}").unwrap();
    fs::write(dir.join("complete.txt"), "done").unwrap();
    let result = classify_run(&dir, false);
    fs::remove_dir_all(&dir).ok();
    assert_eq!(result, Some(RunClassification::Completed));
}

// ---------- discover_runs_in (spec test #8) ----------

#[test]
fn discover_runs_filters_terminal() {
    let root = env::temp_dir().join(make_run_id());
    fs::create_dir_all(&root).unwrap();

    // Completed
    let c = root.join("completed-aaa");
    fs::create_dir_all(&c).unwrap();
    fs::write(c.join("meta.json"), "{}").unwrap();
    fs::write(c.join("complete.txt"), "done").unwrap();

    // Timeout
    let t = root.join("timeout-bbb");
    fs::create_dir_all(&t).unwrap();
    fs::write(t.join("meta.json"), "{}").unwrap();
    fs::write(
        t.join("error.txt"),
        "timeout waiting for explicit completion\n",
    )
    .unwrap();

    // No meta.json → skipped silently
    let skipped = root.join("no-meta-ccc");
    fs::create_dir_all(&skipped).unwrap();

    let records = discover_runs_in(&root);
    fs::remove_dir_all(&root).ok();

    assert_eq!(records.len(), 2, "got: {records:?}");
    let ids: Vec<&str> = records.iter().map(|r| r.id.as_str()).collect();
    assert!(ids.contains(&"completed-aaa"));
    assert!(ids.contains(&"timeout-bbb"));
    assert!(records.iter().all(|r| r.classification.is_terminal()));
}

#[test]
fn discover_runs_in_returns_empty_for_missing_root() {
    let root = env::temp_dir().join(make_run_id());
    // Don't create it
    let records = discover_runs_in(&root);
    assert!(records.is_empty());
}

// ---------- prune_selection (spec test #9) ----------

#[test]
fn prune_selection_no_ids_keeps_only_terminal() {
    let records = vec![
        record("a-completed", RunClassification::Completed, false),
        record("b-in-progress", RunClassification::InProgress, true),
        record("c-timeout", RunClassification::Timeout, true),
        record("d-blocked", RunClassification::Blocked, true),
        record("e-exited", RunClassification::Exited, false),
    ];
    let result = prune_selection(records, &[]).unwrap();
    let ids: Vec<&str> = result.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(result.len(), 4);
    assert!(ids.contains(&"a-completed"));
    assert!(ids.contains(&"c-timeout"));
    assert!(ids.contains(&"d-blocked"));
    assert!(ids.contains(&"e-exited"));
    assert!(!ids.contains(&"b-in-progress"));
}

#[test]
fn prune_rejects_non_terminal_id() {
    let records = vec![
        record("aaa", RunClassification::InProgress, true),
        record("bbb", RunClassification::Completed, false),
    ];
    let result = prune_selection(records, &["aaa".to_string()]);
    let msg = result.unwrap_err();
    assert!(msg.contains("still in progress"), "got: {msg}");
    assert!(msg.contains("aaa"), "got: {msg}");
}

#[test]
fn prune_rejects_unknown_id() {
    let records = vec![record("bbb", RunClassification::Completed, false)];
    let result = prune_selection(records, &["does-not-exist".to_string()]);
    let msg = result.unwrap_err();
    assert!(msg.contains("unknown"), "got: {msg}");
    assert!(msg.contains("does-not-exist"), "got: {msg}");
}

#[test]
fn prune_selection_picks_explicit_terminal_ids() {
    let records = vec![
        record("a-completed", RunClassification::Completed, false),
        record("b-in-progress", RunClassification::InProgress, true),
        record("c-timeout", RunClassification::Timeout, true),
    ];
    let result = prune_selection(records, &["a-completed".to_string(), "c-timeout".to_string()])
        .unwrap();
    assert_eq!(result.len(), 2);
}

// ---------- execute_prune (spec test #10) ----------

#[test]
fn dry_run_does_not_delete() {
    let root = env::temp_dir().join(make_run_id());
    fs::create_dir_all(&root).unwrap();
    let run_a = root.join("aaa");
    fs::create_dir_all(&run_a).unwrap();
    fs::write(run_a.join("meta.json"), "{}").unwrap();
    fs::write(run_a.join("complete.txt"), "done").unwrap();

    let outcome = execute_prune(
        &[record("aaa", RunClassification::Completed, false)],
        &root,
        true, // dry_run
    );

    assert_eq!(outcome.deleted, 0);
    assert!(outcome.failures.is_empty());
    assert!(run_a.exists(), "dry-run must not delete the run dir");

    fs::remove_dir_all(&root).ok();
}

#[test]
fn execute_prune_deletes_when_not_dry_run() {
    let root = env::temp_dir().join(make_run_id());
    fs::create_dir_all(&root).unwrap();

    let a = root.join("aaa");
    fs::create_dir_all(&a).unwrap();
    fs::write(a.join("meta.json"), "{}").unwrap();
    fs::write(a.join("complete.txt"), "done").unwrap();

    let b = root.join("bbb");
    fs::create_dir_all(&b).unwrap();
    fs::write(b.join("meta.json"), "{}").unwrap();
    fs::write(b.join("error.txt"), "timeout waiting for explicit completion\n").unwrap();

    let outcome = execute_prune(
        &[
            record("aaa", RunClassification::Completed, false),
            record("bbb", RunClassification::Timeout, false),
        ],
        &root,
        false,
    );

    assert_eq!(outcome.deleted, 2);
    assert!(outcome.failures.is_empty());
    assert!(!a.exists());
    assert!(!b.exists());

    fs::remove_dir_all(&root).ok();
}

#[test]
fn execute_prune_reports_failures_for_missing_dirs() {
    let root = env::temp_dir().join(make_run_id());
    fs::create_dir_all(&root).unwrap();

    // ghost directory: record points to id that doesn't exist on disk
    let outcome = execute_prune(
        &[record("missing", RunClassification::Completed, false)],
        &root,
        false,
    );

    assert_eq!(outcome.deleted, 0);
    assert_eq!(outcome.failures, vec!["missing"]);

    fs::remove_dir_all(&root).ok();
}

// ---------- parse_prune_args ----------

#[test]
fn parses_prune_no_args() {
    let parsed = prune_argv(&[]);
    assert!(parsed.ids.is_empty());
    assert!(!parsed.yes);
    assert!(!parsed.dry_run);
}

#[test]
fn parses_prune_with_ids() {
    let parsed = prune_argv(&["abc123", "def456"]);
    assert_eq!(parsed.ids, vec!["abc123", "def456"]);
}

#[test]
fn parses_prune_dry_run() {
    let parsed = prune_argv(&["--dry-run"]);
    assert!(parsed.dry_run);
    assert!(!parsed.yes);
}

#[test]
fn parses_prune_yes_short_and_long() {
    assert!(prune_argv(&["-y"]).yes);
    assert!(prune_argv(&["--yes"]).yes);
}

#[test]
fn parses_prune_dry_run_and_yes_together() {
    let parsed = prune_argv(&["-y", "--dry-run"]);
    assert!(parsed.yes);
    assert!(parsed.dry_run);
}

#[test]
fn parses_prune_rejects_unknown_flag() {
    let owned = vec!["--no-such-flag".to_string()];
    let result = parse_prune_args(&owned);
    let msg = result.unwrap_err();
    assert!(msg.contains("--no-such-flag"), "got: {msg}");
}

#[test]
fn parses_prune_mixed_ids_and_flags() {
    let parsed = prune_argv(&["abc", "--dry-run", "def"]);
    assert_eq!(parsed.ids, vec!["abc", "def"]);
    assert!(parsed.dry_run);
}

// ---------- Formatting helpers ----------

#[test]
fn format_age_handles_seconds_minutes_hours_days() {
    assert_eq!(format_age_secs(5), "5s ago");
    assert_eq!(format_age_secs(120), "2m ago");
    assert_eq!(format_age_secs(7200), "2h ago");
    assert_eq!(format_age_secs(172800), "2d ago");
}

#[test]
fn short_id_truncates_long_ids() {
    assert_eq!(short_id("abcdef0123456789"), "abcdef01");
    assert_eq!(short_id("short"), "short");
}
```

### Suggested commit boundaries within Phase 2

The implementer should commit per logical unit, roughly in this order:

1. `RunClassification` enum + `RunRecord` struct + classification tests
2. `classify_from_markers` + `classify_run` + their tests
3. `discover_runs` + `discover_runs_in` + discovery tests
4. `parse_prune_args` + `PruneArgs` + parser tests
5. `prune_selection` + selection tests
6. `execute_prune` + `PruneOutcome` + execution tests
7. `short_id` + `format_age_secs` + helper tests
8. `handle_prune` + dispatch wire-up in `run()` + `print_help` update

This produces ~8 small, reviewable commits with each one passing `cargo test` on its own (later commits can rely on earlier ones in the chain).

### Success Criteria:

#### Automated Verification:

- [ ] `cargo build` succeeds with **no warnings** (no `dead_code`, no `unused`)
- [ ] `cargo fmt -- --check` clean
- [ ] `cargo test` all tests pass (44 existing + ~22 new)
- [ ] `cargo test classify_` runs the classification tests
- [ ] `cargo test prune_selection_` runs the selection tests
- [ ] `cargo test execute_prune` runs the deletion tests
- [ ] `cargo test parses_prune_` runs the parser tests
- [ ] `cargo run -- --help | grep -F prune` shows `prune` in the usage line
- [ ] `TMUXLET_HOME=$(mktemp -d) cargo run -- prune` exits 0 with `Nothing to prune.`
- [ ] `TMUXLET_HOME=$(mktemp -d) cargo run -- prune --dry-run` exits 0 with `Nothing to prune.`
- [ ] `cargo run -- prune --no-such-flag` exits 1 with `tmuxlet: unknown option for prune: --no-such-flag`

#### Manual Verification:

- [ ] Set up `TMUXLET_HOME=/tmp/tmuxlet-test-prune` and create three fixture run-dirs: `aaa/` with `meta.json + complete.txt`, `bbb/` with `meta.json + error.txt` starting with `timeout ...`, `ccc/` with just `meta.json`.
- [ ] `TMUXLET_HOME=/tmp/tmuxlet-test-prune cargo run -- prune --dry-run` lists `aaa` and `bbb` but NOT `ccc` (assuming no live tmux session for any of them; `ccc` will classify as Exited and DO appear as terminal — but that's expected behavior, not a regression).
- [ ] Verify all three dirs still exist after the dry-run.
- [ ] `TMUXLET_HOME=/tmp/tmuxlet-test-prune cargo run -- prune -y` deletes the terminal dirs.
- [ ] Re-create the fixtures, then `TMUXLET_HOME=/tmp/tmuxlet-test-prune cargo run -- prune bbb` (without -y). Confirm the prompt, type `n`, verify `bbb/` is still on disk.
- [ ] Re-run with `y` at the prompt — confirm `bbb/` is deleted.
- [ ] `TMUXLET_HOME=/tmp/tmuxlet-test-prune cargo run -- prune nonexistent` exits 1 with `tmuxlet: unknown run(s): nonexistent`.
- [ ] Real end-to-end: `cargo run -- -p "say ready"` (uses real `~/.tmuxlet`), then `cargo run -- prune -y`, verify the run-dir is gone.

**Implementation Note**: After Phase 2 passes all automated and manual verification, pause for manual confirmation before proceeding to Phase 3 (docs).

---

## Phase 3: README + final verification

### Overview

Document the `prune` subcommand and the auto-kill behavior in `README.md`. Run final sanity checks.

### Changes Required:

#### 1. Add `prune` to the CLI section

**File**: `README.md`
**Location**: @README.md:111-118 — the existing fenced block listing subcommands. Append a `prune` line in the same style as `stop`, plus a brief example.

The implementer should:
- Read the existing block to match its exact formatting (column alignment, prose style).
- Add `tmuxlet prune [<id>...] [-y] [--dry-run]` alongside the other commands.
- Add a one-liner description: "delete terminal runs (completed, timeout, blocked, exited) and their tmux sessions".

#### 2. Add `prune` notes to "Runtime State"

**File**: `README.md`
**Location**: @README.md:216-232.

Add one or two sentences at the end of the section noting:
- A `completed` print-mode run auto-kills its tmux session but leaves the run-dir on disk.
- `tmuxlet prune` is how those run-dirs (and any leftover tmux sessions from `timeout`/`blocked` runs) get cleaned up.

#### 3. Add `prune` flow example (recommended)

**File**: `README.md`
**Location**: Either between "Output And Failures" (ends line 264) and "Development" (starts line 265), or extending "Output And Failures" with a brief cleanup paragraph.

Show a typical flow:
```
$ tmuxlet -p "say ready"          # auto-kills tmux session on success
$ tmuxlet --timeout 5 -p "..."    # times out — tmux session stays alive
$ tmuxlet attach <id>             # investigate
$ tmuxlet prune                   # cleans up the timed-out run + dir
```

Keep the addition under ~15 lines. The implementer chooses the exact wording.

### Success Criteria:

#### Automated Verification:

- [ ] `cargo fmt -- --check` clean
- [ ] `cargo build` succeeds with no warnings
- [ ] `cargo test` all tests pass
- [ ] `cargo run -- --version` matches `Cargo.toml`'s version
- [ ] `cargo run -- --help` shows `prune` in usage line
- [ ] `grep -c "prune" README.md` returns ≥ 3 (CLI listing + Runtime State + flow example)

#### Manual Verification:

- [ ] Read the modified README end-to-end. The `prune` documentation reads naturally next to `stop`/`status`/`attach`.
- [ ] The auto-kill note in "Runtime State" doesn't contradict the existing description of the runs-dir layout.
- [ ] No section heading was inadvertently renamed.
- [ ] One final smoke test: `cargo run -- -p "say ready"` → check `tmux ls` (no leftover session) → `cargo run -- prune` (run-dir cleanup works) → all green.

**Implementation Note**: This phase is the final one. After it passes, the implementation is done. Confirm manually and create a commit covering README + any final formatting fixes, then close out.

---

## Testing Strategy

### Unit Tests (added by this plan):

**Classification (10 tests)**:
- `classify_completed_when_complete_txt_present`
- `classify_timeout_when_error_starts_with_timeout`
- `classify_blocked_when_error_starts_with_blocked`
- `classify_blocked_for_unknown_error_content`
- `classify_exited_when_session_dead_no_markers`
- `classify_in_progress_when_session_alive_no_markers`
- `classification_is_terminal_for_all_except_in_progress`
- `classify_run_returns_none_without_meta_json` (tempdir)
- `classify_run_reads_error_first_line` (tempdir)
- `classify_run_completed_takes_priority_over_session_state` (tempdir)

**Discovery (2 tests, spec test #8)**:
- `discover_runs_filters_terminal` (tempdir)
- `discover_runs_in_returns_empty_for_missing_root`

**Selection (4 tests, includes spec test #9)**:
- `prune_selection_no_ids_keeps_only_terminal`
- `prune_rejects_non_terminal_id`
- `prune_rejects_unknown_id`
- `prune_selection_picks_explicit_terminal_ids`

**Execution (3 tests, includes spec test #10)**:
- `dry_run_does_not_delete` (tempdir)
- `execute_prune_deletes_when_not_dry_run` (tempdir)
- `execute_prune_reports_failures_for_missing_dirs` (tempdir)

**Parser (7 tests)**:
- `parses_prune_no_args`
- `parses_prune_with_ids`
- `parses_prune_dry_run`
- `parses_prune_yes_short_and_long`
- `parses_prune_dry_run_and_yes_together`
- `parses_prune_rejects_unknown_flag`
- `parses_prune_mixed_ids_and_flags`

**Formatting (2 tests)**:
- `format_age_handles_seconds_minutes_hours_days`
- `short_id_truncates_long_ids`

Total: ~28 new automated tests. All three spec-required tests (#8, #9, #10) are automated.

### Integration Tests:

Not added — matches the existing test posture (pure stdlib, no tmux). The interactive prompt and `tmux kill-session` shell-out are covered by manual verification rather than tests.

### Manual Testing Steps:

After Phase 1:
1. `cargo run -- -p "say ready"` → returns successfully
2. `tmux ls | grep tmuxlet-` → should produce no output for this run's id

After Phase 2:
1. Build tempdir fixtures with synthetic `meta.json`, `complete.txt`, `error.txt` files.
2. `prune --dry-run` shows correct selection.
3. `prune -y` deletes terminal run-dirs; leaves in-progress ones.
4. Explicit-id mode errors correctly for unknown and in-progress IDs.
5. Interactive prompt accepts/rejects correctly.

After Phase 3:
1. Re-run the Phase 1 and Phase 2 smoke tests to confirm no regression from README-adjacent edits.

## Performance Considerations

- `discover_runs` is O(N) over the runs root, with one `tmux has-session` shell-out per directory. For typical N (a handful to a few dozen), this is fine — the user only invokes it via `prune`, not in a hot path. No caching is needed.
- `kill-session` and `fs::remove_dir_all` are both O(1) per run from the caller's perspective. Total work scales linearly with selected runs.

## Migration Notes

No data migration needed. Existing `~/.tmuxlet/runs/<id>/` directories from before this change continue to work — `prune` classifies them based on their marker files exactly as for new runs.

## References

- Design spec: @docs/superpowers/specs/2026-05-26-tmux-session-cleanup-design.md (commit `2c92558`)
- Research doc: @docs/superpowers/research/2026-05-26-tmux-session-cleanup.md (commit `8a5da81`)
- Existing tests (style template): @src/main.rs:1841-2349
- Existing `kill-session` pattern: @src/main.rs:1185-1189 (in `handle_stop`)
- `tmux_session_exists` helper: @src/main.rs:933-939
- `heartbeat_age_secs` helper: @src/main.rs:1119-1125
- `state_dir` + `run_paths`: @src/main.rs:1772-1795
- `make_run_id`: @src/main.rs:1769-1776

## Research Findings

Full agent findings from the research phase:
- @docs/superpowers/research/findings/2026-05-26-tmux-session-cleanup/codebase-analyzer-run-completion.md — completion lifecycle
- @docs/superpowers/research/findings/2026-05-26-tmux-session-cleanup/codebase-analyzer-cli-dispatch.md — CLI dispatch + handlers
- @docs/superpowers/research/findings/2026-05-26-tmux-session-cleanup/codebase-analyzer-test-patterns.md — test conventions
- @docs/superpowers/research/findings/2026-05-26-tmux-session-cleanup/codebase-analyzer-readme-structure.md — README structure
