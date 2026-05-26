# tmux session cleanup — implementation plan

**Date:** 2026-05-26
**Author:** Kevin Kirchner (Claude)
**Spec:** [docs/superpowers/specs/2026-05-26-tmux-session-cleanup-design.md](../specs/2026-05-26-tmux-session-cleanup-design.md)
**Research:** [docs/superpowers/research/2026-05-26-tmux-session-cleanup.md](../research/2026-05-26-tmux-session-cleanup.md)
**Branch:** `claude-hooks` (continuing current branch — no separate feature branch)

## Overview

Implement the two changes from the approved design spec in four phases:

1. **Phase 1** — Add a `RunClassification` enum, a `RunRecord` struct, and pure-function classification helpers. No call-site changes yet.
2. **Phase 2** — Auto-kill the tmux session on the `completed` print-mode return path. ~5 line insertion.
3. **Phase 3** — Add the `tmuxlet prune` subcommand: argument parser, handler, dispatch wire-up, help text update, and unit tests.
4. **Phase 4** — Update `README.md` to document `prune` and the auto-kill behavior. Final `cargo fmt` / `cargo test` / `cargo build` sanity.

`handle_status` is **not** refactored to use `discover_runs` in this plan. The spec called out the refactor as a "small, focused improvement on code we're already touching," but `handle_status` currently prints `meta.json` text plus heartbeat age (not a classification), and routing it through `discover_runs` would either change its observable output or require `RunRecord` to carry raw `meta.json` text — both expand scope. Per CLAUDE.md "don't refactor beyond what the task requires," `handle_status` is left untouched. `discover_runs` is added solely as `handle_prune`'s enumerator and is private to the module.

## Current State Analysis

The print-mode polling loop is inline within `run_print` at @src/main.rs:326-415 with four terminal return paths: `completed` (327-342), `blocked` (360-377), `timeout` (382-396), `exited` (399-411). Today, **only `handle_stop` calls `kill-session`** — every other terminal path leaves the tmux session alive, and even `handle_stop` does not delete the `~/.tmuxlet/runs/<id>/` directory.

Subcommand dispatch is a single match in `run()` at @src/main.rs:143-168, with no shared flag parser. `parse_options` (171-237) is the print-mode parser and is unused by handler subcommands. There is no existing classification helper for run directories; the four status strings are produced inline in the polling loop.

Tests live in `#[cfg(test)] mod tests` at @src/main.rs:1841-2349 — 44 tests, stdlib-only, no `tempfile` crate, two test helpers (`parse`, `has_pair`), one fs-touching test (`write_complete_once_is_idempotent`) that isolates via `env::temp_dir().join(make_run_id())`.

`Cargo.toml` has zero runtime and zero dev dependencies. New code must avoid pulling in `clap`, `tempfile`, etc.

## Desired End State

After all four phases:

- A successful `tmuxlet -p ...` run cleans up its tmux session before returning. The `~/.tmuxlet/runs/<id>/` directory remains (so callers can still inspect `complete.txt`, `pane.log`, etc.).
- `timeout` and `blocked` runs leave both the tmux session and run-dir intact (so the user can `attach`, `read`, `send`, or investigate before `prune`).
- `tmuxlet prune` cleans up everything that's terminal: kills any alive tmux sessions belonging to terminal runs, then `fs::remove_dir_all`s the run-dir. Refuses non-terminal IDs.
- The `prune` command supports: no-args (sweep all terminal), explicit IDs, `-y`/`--yes` to skip confirmation, `--dry-run` to preview. `--dry-run` overrides `-y`.
- `cargo test` passes including ~10 new tests covering classification logic, `prune` argument parsing, and tempdir-backed terminal-filter / dry-run behavior.
- `README.md` documents `prune` in the CLI section and mentions the auto-kill behavior.

### Verification:

- `cargo fmt -- --check` clean
- `cargo build` succeeds
- `cargo test` all tests pass
- `cargo run -- --help` shows `prune` in the usage line
- `cargo run -- prune --dry-run` works against an empty `~/.tmuxlet/runs/` (prints "Nothing to prune.")

## Key Discoveries

- **No `wait_for_completion` function** — the polling loop is inline in `run_print`. The spec's reference to that function name is approximate; the actual insertion point for auto-kill is @src/main.rs:331 (between the existing `capture_pane_to_file().ok()` at 330 and the `return Ok(RunResult { ... })` at 331-342).
- **`tmux_session` is moved into `RunResult`** at line 338 via field shorthand. The auto-kill must reference `&tmux_session` BEFORE that move — i.e., between lines 330 and 331.
- **`handle_stop` already gates on `tmux_session_exists`** (lines 933-939) and the kill pattern there (1185-1189) is the model to follow.
- **`error.txt` first-line prefixes are exactly `"timeout "` and `"blocked "`** — see `"timeout waiting for explicit completion\n"` at @src/main.rs:385 and `blocked_error_message` first line at @src/main.rs:924-931 (`"blocked waiting for a confirmation or permission prompt"`).
- **Single fs-test pattern: `env::temp_dir().join(make_run_id())`** — see `write_complete_once_is_idempotent` at @src/main.rs:2336-2348. Used by new tempdir tests in Phases 1 and 3.
- **`make_run_id()`** at @src/main.rs:1769-1776 produces unique nanos+pid slugs, so concurrent test runs don't collide on `env::temp_dir()`.
- **Existing helpers `print_help`** at @src/main.rs:1832-1839 inlines a `<status|read|send|attach|stop>` token in its usage line. Phase 3 updates this.

## What We're NOT Doing

- **Refactoring `handle_status`** to use `discover_runs` (would force a behavior change to its output, or muddy the helper's signature). The spec mentioned this; the plan defers it.
- **Refactoring or changing `handle_stop`'s** behavior (it still does not delete run dirs; that's `prune`'s job by design).
- **Adding `clap` or any other dep.** All flag parsing remains hand-rolled.
- **Adding integration tests against real tmux.** Matches the existing test posture (pure-function only). Manual verification covers tmux-coupled behavior.
- **Adding any time-based auto-reaper, TTL, or cron-style cleanup.** Long-running runs are first-class and explicitly preserved.
- **Adding a `--keep-session` opt-out** on print-mode for the auto-kill. The auto-kill on `completed` is unconditional.
- **Changing the `completed` exit code, status string, or `RunResult` fields.**
- **Changing the `~/.tmuxlet/runs/<id>/` directory layout or marker filenames.**

## Implementation Approach

Four phases, gate manual verification between each. Phase 1 is pure helper code with tests — zero behavior change to existing flows. Phase 2 is the smallest behavior change (auto-kill). Phase 3 is the largest piece (the new subcommand) and depends on Phase 1's helpers. Phase 4 is docs and a final sanity pass.

Within each phase, the implementer should commit per logical unit (new types together, then helper, then tests, etc.) — never amass all changes into a single commit.

---

## Phase 1: Helper foundation

### Overview

Add the type definitions and pure functions that `prune` will rely on, plus their unit tests. No call-site wires anywhere — this phase is invisible to the running CLI.

### Changes Required:

#### 1. Add the `RunClassification` enum

**File**: `src/main.rs`
**Location**: After the `RunResult` struct (around line 118), before the `OutputFormat` impl block resumes.

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

#### 2. Add the `RunRecord` struct

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

#### 3. Add `classify_from_markers` (pure helper)

**File**: `src/main.rs`
**Location**: Near the other small helpers — suggest placing just before `tmux_session_exists` at @src/main.rs:933, so the related functions are grouped.

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
        // Unknown error.txt content — treat as Blocked (conservative: leaves
        // session info available for investigation, still classified terminal).
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

Note: returns `Option<RunClassification>` so callers can distinguish "run dir not initialized" (no `meta.json`) from any classified state. `discover_runs` filters those out.

#### 5. Add `discover_runs`

**File**: `src/main.rs`
**Location**: Immediately after `classify_run`. Uses `state_dir()` (at @src/main.rs:1772-1777) and `heartbeat_age_secs` (at @src/main.rs:1119-1125).

```rust
fn discover_runs() -> Vec<RunRecord> {
    let runs_root = state_dir().join("runs");
    let entries = match fs::read_dir(&runs_root) {
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

#### 6. Add unit tests for classification

**File**: `src/main.rs`
**Location**: Inside `mod tests`, near the end. Group all new tests together for easy review.

Test names (following the existing `<subject>_<verb>_<condition>` convention):

```rust
#[test]
fn classify_completed_when_complete_txt_present() {
    assert_eq!(
        classify_from_markers(true, None, true),
        RunClassification::Completed
    );
    // complete.txt wins even if error.txt is also present
    assert_eq!(
        classify_from_markers(true, Some("timeout ..."), false),
        RunClassification::Completed
    );
}

#[test]
fn classify_timeout_when_error_starts_with_timeout() {
    assert_eq!(
        classify_from_markers(false, Some("timeout waiting for explicit completion"), true),
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
    // No meta.json written
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
```

### Success Criteria:

#### Automated Verification:

- [ ] `cargo build` succeeds
- [ ] `cargo fmt -- --check` clean
- [ ] `cargo test` all tests pass (including 10 new ones)
- [ ] `cargo test classify_` runs the new tests
- [ ] No call sites of `discover_runs`, `classify_run`, or `classify_from_markers` exist yet (this phase is silent — compiler will warn `dead_code`; add `#[allow(dead_code)]` to `discover_runs`, `classify_run`, `classify_from_markers`, and `RunClassification::label` / `is_terminal` / `RunRecord` if the unused-warnings hurt CI; Phase 3 removes the warnings naturally)

#### Manual Verification:

- [ ] `cargo run -- --help` still prints the existing help text (unchanged)
- [ ] `cargo run -- status` still works on an existing `~/.tmuxlet/runs/` (output unchanged)
- [ ] `cargo run -- -p "say ready"` still works end-to-end (unchanged)

**Implementation Note**: After Phase 1 passes automated verification, pause for manual confirmation that nothing has regressed before proceeding to Phase 2.

---

## Phase 2: Auto-kill on `completed`

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

- [ ] `cargo build` succeeds
- [ ] `cargo fmt -- --check` clean
- [ ] `cargo test` all tests pass (existing + Phase 1 additions; no new tests added in this phase — the change is too tmux-coupled for a pure unit test)

#### Manual Verification:

- [ ] `cargo run -- -p "say ready"` returns successfully with status "completed"
- [ ] **Immediately after** the run returns, `tmux ls 2>/dev/null | grep tmuxlet-` shows **no** session for that run (vs. before this phase, where the session would persist)
- [ ] The `~/.tmuxlet/runs/<id>/` directory is still present and contains `complete.txt`, `pane.log`, `meta.json`, `heartbeat` (run-dir is not deleted by auto-kill)
- [ ] If a print-mode run times out (`cargo run -- --timeout 5 -p "sleep 60"`), the tmux session is still alive afterward and `tmux ls` shows it (timeout/blocked paths are unchanged)

**Implementation Note**: After Phase 2 passes automated verification AND manual smoke (`-p "say ready"` then `tmux ls`), pause for manual confirmation before proceeding to Phase 3.

---

## Phase 3: `prune` command

### Overview

Add a new `tmuxlet prune` subcommand with the following surface:

```
tmuxlet prune [<id>...] [-y|--yes] [--dry-run]
```

Selection semantics:
- **No IDs** — sweep every run in `~/.tmuxlet/runs/` whose classification is terminal.
- **IDs given** — only those runs. Each must resolve to an existing run-dir AND be in terminal status; otherwise exit 1.

Action per pruned run: `kill-session` (best-effort) if alive, then `fs::remove_dir_all` the run-dir.

Confirmation flow:
- **Default**: prompts `Proceed? [y/N]`. Anything not `y`/`Y`/`yes` aborts with exit 0.
- **`-y`/`--yes`**: skip the prompt.
- **`--dry-run`**: list what would be pruned, do nothing, exit 0. Never prompts.
- **`-y` + `--dry-run`**: `--dry-run` wins.

Output: see the design spec § "Output" for the table format.

### Changes Required:

#### 1. Add the `PruneArgs` struct and pure parser

**File**: `src/main.rs`
**Location**: Place the parser near `parse_options` (around line 237), but as a separate function. The struct goes alongside it.

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

#### 2. Add the `handle_prune` function

**File**: `src/main.rs`
**Location**: Immediately after `handle_stop` at @src/main.rs:1194 (so all `handle_*` functions are clustered).

Skeleton:

```rust
fn handle_prune(args: &[String]) -> Result<(), String> {
    let parsed = parse_prune_args(args)?;
    let runs = discover_runs();

    // Selection
    let selected: Vec<RunRecord> = if parsed.ids.is_empty() {
        runs.into_iter().filter(|r| r.classification.is_terminal()).collect()
    } else {
        // Validate each explicit id: must exist AND be terminal.
        let mut by_id = std::collections::HashMap::new();
        for record in runs {
            by_id.insert(record.id.clone(), record);
        }
        let mut found: Vec<RunRecord> = Vec::new();
        let mut unknown: Vec<String> = Vec::new();
        let mut non_terminal: Vec<String> = Vec::new();
        for id in &parsed.ids {
            match by_id.remove(id) {
                None => unknown.push(id.clone()),
                Some(rec) if !rec.classification.is_terminal() => {
                    non_terminal.push(rec.id.clone());
                }
                Some(rec) => found.push(rec),
            }
        }
        if !unknown.is_empty() {
            return Err(format!(
                "unknown run(s): {}",
                unknown.join(", ")
            ));
        }
        if !non_terminal.is_empty() {
            return Err(format!(
                "run(s) still in progress; use `tmuxlet stop <id>` first: {}",
                non_terminal.join(", ")
            ));
        }
        found
    };

    if selected.is_empty() {
        println!("Nothing to prune.");
        return Ok(());
    }

    // Display the proposed table.
    let action_verb = if parsed.dry_run { "Would prune" } else { "Found" };
    println!("{action_verb} {} run(s):", selected.len());
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

    // Execute deletions, tracking failures.
    let mut deleted = 0usize;
    let mut failures: Vec<String> = Vec::new();
    for rec in &selected {
        if rec.session_alive {
            let session_name = format!("tmuxlet-{}", rec.id);
            let _ = run_tmux([
                OsStr::new("kill-session"),
                OsStr::new("-t"),
                OsStr::new(&session_name),
            ]);
        }
        let run_dir = state_dir().join("runs").join(&rec.id);
        match fs::remove_dir_all(&run_dir) {
            Ok(()) => deleted += 1,
            Err(e) => {
                eprintln!("tmuxlet: failed to remove {}: {e}", run_dir.display());
                failures.push(rec.id.clone());
            }
        }
    }

    println!("Pruned {deleted} run(s).");
    if !failures.is_empty() {
        return Err(format!("failed to remove {} run(s)", failures.len()));
    }
    Ok(())
}

fn short_id(id: &str) -> String {
    // First 8 chars of the run id, or the whole thing if shorter
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

#### 3. Wire dispatch in `run()`

**File**: `src/main.rs`
**Location**: @src/main.rs:143-168, inside the existing `match args[0].as_str()`. Add the `"prune"` arm alphabetically (or grouped with other lifecycle commands).

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

#### 4. Update `print_help`

**File**: `src/main.rs`
**Location**: @src/main.rs:1832-1839. The usage line currently reads `tmuxlet <status|read|send|attach|stop>` — add `prune`.

**Before**:
```
Usage:\n  tmuxlet -p [options] [prompt]\n  tmuxlet <status|read|send|attach|stop>\n  tmuxlet bridge ...
```

**After**:
```
Usage:\n  tmuxlet -p [options] [prompt]\n  tmuxlet <status|read|send|attach|stop|prune>\n  tmuxlet bridge ...
```

#### 5. Remove `#[allow(dead_code)]` annotations added in Phase 1

If Phase 1 added `#[allow(dead_code)]` to keep the build clean while helpers were unused, remove those annotations now — Phase 3's `handle_prune` exercises them.

#### 6. Add unit tests

**File**: `src/main.rs`
**Location**: Inside `mod tests`, grouped with the Phase 1 classification tests.

```rust
fn prune_argv(args: &[&str]) -> PruneArgs {
    let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    parse_prune_args(&owned).unwrap()
}

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
    let short = prune_argv(&["-y"]);
    let long = prune_argv(&["--yes"]);
    assert!(short.yes);
    assert!(long.yes);
}

#[test]
fn parses_prune_dry_run_and_yes_together() {
    // Both flags accepted; --dry-run's "wins" semantics is enforced in handle_prune,
    // not in the parser. This test pins parser behavior only.
    let parsed = prune_argv(&["-y", "--dry-run"]);
    assert!(parsed.yes);
    assert!(parsed.dry_run);
}

#[test]
fn parses_prune_rejects_unknown_flag() {
    let owned = vec!["--no-such-flag".to_string()];
    let result = parse_prune_args(&owned);
    assert!(result.is_err());
    let msg = result.unwrap_err();
    assert!(msg.contains("--no-such-flag"), "got: {msg}");
}

#[test]
fn parses_prune_mixed_ids_and_flags() {
    let parsed = prune_argv(&["abc", "--dry-run", "def"]);
    assert_eq!(parsed.ids, vec!["abc", "def"]);
    assert!(parsed.dry_run);
}

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

### Success Criteria:

#### Automated Verification:

- [ ] `cargo build` succeeds (no warnings about unused helpers)
- [ ] `cargo fmt -- --check` clean
- [ ] `cargo test` all tests pass (existing + Phase 1 + new Phase 3 tests)
- [ ] `cargo test parses_prune_` runs the new parser tests
- [ ] `cargo run -- --help | grep prune` shows `prune` in the usage line
- [ ] `cargo run -- prune --help-no-such-flag` returns exit 1 with `unknown option for prune: --help-no-such-flag` (loose parser semantics — any unknown `--X` is an error)
- [ ] `TMUXLET_HOME=$(mktemp -d) cargo run -- prune` prints `Nothing to prune.` and exits 0
- [ ] `TMUXLET_HOME=$(mktemp -d) cargo run -- prune --dry-run` prints `Nothing to prune.` and exits 0

#### Manual Verification:

- [ ] Set up a fresh `TMUXLET_HOME=/tmp/tmuxlet-test-prune` and create a few fixture run-dirs by hand: one with `meta.json + complete.txt` (Completed), one with `meta.json + error.txt` containing `timeout ...` (Timeout), one with `meta.json` only (InProgress).
- [ ] Run `TMUXLET_HOME=/tmp/tmuxlet-test-prune cargo run -- prune --dry-run`. Verify the output lists the two terminal runs (completed + timeout) but NOT the in-progress run.
- [ ] Verify run-dirs are still present after the dry-run.
- [ ] Run `TMUXLET_HOME=/tmp/tmuxlet-test-prune cargo run -- prune -y`. Verify the two terminal run-dirs are deleted; the in-progress one remains.
- [ ] Run `TMUXLET_HOME=/tmp/tmuxlet-test-prune cargo run -- prune <in-progress-id>`. Verify exit code 1 with a "still in progress; use `tmuxlet stop`" error.
- [ ] Run `TMUXLET_HOME=/tmp/tmuxlet-test-prune cargo run -- prune nonexistent-id`. Verify exit code 1 with "unknown run(s)" error.
- [ ] Run a real `cargo run -- -p "say ready"` (status: completed, auto-killed by Phase 2). Verify the run-dir is still on disk after the run returns. Then `cargo run -- prune -y` and verify the run-dir is gone.

**Implementation Note**: After Phase 3 passes automated verification AND manual prune flow against fixtures, pause for manual confirmation before proceeding to Phase 4 (docs).

---

## Phase 4: README + final verification

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

#### 3. Add `prune` flow example (optional but recommended)

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
- [ ] `cargo build` succeeds
- [ ] `cargo test` all tests pass
- [ ] `cargo run -- --version` matches `Cargo.toml`'s version
- [ ] `cargo run -- --help` shows `prune` in usage line
- [ ] `grep -n "^## " README.md` shows expected section structure (no duplicate headings)
- [ ] `grep -c "prune" README.md` returns ≥ 3 (CLI listing + Runtime State + flow example)

#### Manual Verification:

- [ ] Read the modified README end-to-end. The `prune` documentation reads naturally next to `stop`/`status`/`attach`.
- [ ] The auto-kill note in "Runtime State" doesn't contradict the existing description of the runs dir layout.
- [ ] No section heading was inadvertently renamed.
- [ ] One final smoke test: `cargo run -- -p "say ready"` → check `tmux ls` (no leftover session) → `cargo run -- prune` (run-dir cleanup works) → all green.

**Implementation Note**: This phase is the final one. After it passes, the implementation is done. Confirm manually and create a commit covering README + any final formatting fixes, then close out.

---

## Testing Strategy

### Unit Tests (added by this plan):

**Phase 1** (classification):
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

**Phase 3** (parser + formatting helpers):
- `parses_prune_no_args`
- `parses_prune_with_ids`
- `parses_prune_dry_run`
- `parses_prune_yes_short_and_long`
- `parses_prune_dry_run_and_yes_together`
- `parses_prune_rejects_unknown_flag`
- `parses_prune_mixed_ids_and_flags`
- `format_age_handles_seconds_minutes_hours_days`
- `short_id_truncates_long_ids`

### Integration Tests:

Not added — matches the existing test posture (pure stdlib, no tmux). The Phase 3 `handle_prune` body has fan-out (selection logic, validation errors, dry-run, confirmation, deletion) but its non-pure parts (interactive prompt, fs::remove_dir_all, tmux kill) are covered by manual verification rather than tests.

### Manual Testing Steps:

After Phase 2:
1. `cargo run -- -p "say ready"` → returns successfully
2. `tmux ls | grep tmuxlet-` → should produce no output for this run's id

After Phase 3:
1. Build fixtures in `$TMUXLET_HOME` (set to a tempdir) with synthetic `meta.json`, `complete.txt`, `error.txt` files representing each classification state.
2. `prune --dry-run` shows correct selection.
3. `prune -y` deletes terminal run-dirs; leaves in-progress ones.
4. Explicit-id mode errors correctly for unknown and in-progress IDs.

After Phase 4:
1. Re-run the Phase 2 and Phase 3 smoke tests to confirm no regression from README-adjacent edits.

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
