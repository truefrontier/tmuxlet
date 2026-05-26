# tmuxlet Test Patterns — Codebase Analysis

**Date:** 2026-05-26
**Scope:** `/Users/kk/Sites/CodefiLabs/tmuxlet/src/main.rs` and `/Users/kk/Sites/CodefiLabs/tmuxlet/Cargo.toml`
**Purpose:** Document HOW tests are structured today. No recommendations, no critique.

---

## 1. The `#[cfg(test)] mod tests` Block

### Location

- **Start:** line 1841 (`#[cfg(test)]` attribute), line 1842 (`mod tests {`)
- **End:** line 2349 (closing brace `}`)
- **Imports inside:** `use super::*;` at line 1843 — pulls every name from the parent module into scope.

The block is inline at the bottom of `src/main.rs` (single-file crate). There is no separate `tests/` integration-test directory (confirmed via `Glob` — only `src/main.rs` exists in the crate).

### Inventory of Test Functions (in source order)

| # | Line | Function Name | One-Sentence Description |
|---|------|---------------|--------------------------|
| 1 | 1856 | `parses_print_target_and_prompt` | Verifies `parse_options` extracts `print`, `target`, and positional `prompt_parts` from a flagged argv. |
| 2 | 1864 | `parses_cwd_aliases_and_bypass_alias` | Verifies that `--dir` aliases to `cwd` and `--yolo` aliases to `dangerously_skip_permissions`. |
| 3 | 1871 | `parses_session_controls` | Verifies parsing of `--continue`, `--resume` (with and without value), `--session`, and `--session-id`. |
| 4 | 1889 | `rejects_conflicting_session_controls` | Verifies that combining mutually-exclusive session selectors causes `target_command` to return `Err`. |
| 5 | 1901 | `claude_maps_session_controls_to_native_flags` | Verifies normalized session flags map to Claude's native `--continue`, `--resume`, `--session-id`. |
| 6 | 1916 | `gemini_maps_resume_latest` | Verifies that for Gemini, `--continue` / bare `--resume` map to `--resume latest`, and `--resume <id>` maps through. |
| 7 | 1931 | `codex_maps_session_id_to_resume` | Verifies Codex maps `--continue` to `resume --last`, and `--resume <id>` / `--session-id <id>` to `resume <id>`. |
| 8 | 1949 | `opencode_maps_explicit_session_ids` | Verifies OpenCode keeps `--continue`, and maps `--resume <id>` / `--session-id <id>` to `--session <id>`. |
| 9 | 1964 | `pi_maps_session_id_to_session` | Verifies Pi maps `--session-id` to `--session` and `--resume <id>` to `--resume <id>`. |
| 10 | 1975 | `cursor_maps_session_controls` | Verifies Cursor maps `--continue` to positional `resume` and `--session-id` to `--resume <id>`. |
| 11 | 1986 | `completion_prompt_keeps_user_prompt_first` | Verifies the completion-contract prompt begins with the user prompt and embeds the run id, answer path, and bridge command. |
| 12 | 1995 | `claude_launch_receives_final_prompt_argument` | Verifies Claude's launch command ends with the final prompt as last argv and preserves `--model`. |
| 13 | 2005 | `codex_launch_receives_final_prompt_argument` | Verifies Codex's launch command ends with the final prompt and includes the bypass flag. |
| 14 | 2018 | `unsupported_prompt_argument_targets_use_paste_fallback` | Verifies Gemini does not get the final prompt baked into argv (paste fallback path). |
| 15 | 2027 | `detects_startup_auto_enter_prompts` | Verifies `is_startup_auto_enter_prompt` matches confirmation phrases and rejects benign text. |
| 16 | 2035 | `detects_execution_confirmation_blockers` | Verifies `is_execution_confirmation_blocker` matches mid-run permission prompts and rejects success text. |
| 17 | 2049 | `blocked_status_is_failure` | Verifies `result_status_code("blocked")` is non-zero (treated as failure). |
| 18 | 2054 | `claude_maps_bypass_to_claude_flag` | Verifies `claude_command` emits `--dangerously-skip-permissions` and the model name when bypass + model are set. |
| 19 | 2071 | `gemini_maps_bypass_to_yolo` | Verifies `gemini_command` translates bypass to `--yolo`. |
| 20 | 2078 | `codex_maps_bypass_to_codex_flag` | Verifies `codex_command` translates bypass to `--dangerously-bypass-approvals-and-sandbox`. |
| 21 | 2088 | `rejects_unsupported_flags` | Verifies `gemini_command` rejects `--system-prompt` (an unsupported normalized flag for that target). |
| 22 | 2094 | `shell_quote_handles_single_quotes` | Verifies `shell_quote` properly escapes embedded single quotes. |
| 23 | 2099 | `claude_hooks_settings_is_deterministic` | Verifies two calls to `claude_hooks_settings` with identical inputs return identical strings. |
| 24 | 2107 | `claude_hooks_settings_handles_path_with_spaces` | Verifies space-containing paths are single-quoted in the generated hook JSON. |
| 25 | 2116 | `claude_hooks_settings_contains_both_events` | Verifies the generated JSON contains `Stop` and `PostToolUse` hooks with correct command formats. |
| 26 | 2128 | `needs_completion_contract_excludes_claude` | Verifies `needs_completion_contract` returns false only for Claude and true for every other target. |
| 27 | 2138 | `claude_target_rejects_user_settings_collision` | Verifies `validate_claude_settings_collision` errs when Claude + user `--settings` are combined; OK otherwise. |
| 28 | 2153 | `extract_top_level_string_basic` | Verifies the JSON scanner returns a top-level string value by key and `None` for missing keys. |
| 29 | 2163 | `extract_top_level_string_handles_escapes` | Verifies escaped quotes and backslashes are decoded inside the extracted string. |
| 30 | 2172 | `extract_final_assistant_text_basic` | Verifies the transcript parser returns the last-turn assistant text after the final user message. |
| 31 | 2187 | `extract_final_assistant_text_walks_past_tool_use` | Verifies the parser skips `tool_use` blocks and returns the subsequent text answer. |
| 32 | 2202 | `extract_final_assistant_text_ignores_sidechain` | Verifies sub-agent (`isSidechain: true`) assistant lines are ignored. |
| 33 | 2215 | `extract_final_assistant_text_skips_thinking` | Verifies `thinking` content blocks are skipped and only `text` blocks are returned. |
| 34 | 2226 | `extract_final_assistant_text_errors_when_no_text` | Verifies an error is returned when a user line is present with no assistant follow-up. |
| 35 | 2233 | `extract_final_assistant_text_skips_malformed_lines` | Verifies malformed JSON lines are skipped silently and surrounding valid lines still parse. |
| 36 | 2247 | `extract_final_assistant_text_handles_unicode_text` | Verifies multi-byte UTF-8 content (accented Latin, em dash, Chinese) is preserved. |
| 37 | 2258 | `extract_current_turn_skips_prior_turn_assistant` | Verifies only the assistant text after the LAST user message is returned, not earlier turns. |
| 38 | 2273 | `extract_current_turn_rejects_when_new_turn_assistant_missing` | Verifies an error is returned when the most recent user message has no following assistant text. |
| 39 | 2285 | `extract_current_turn_handles_tool_use_within_turn` | Verifies tool_use/tool_result within the current turn are tolerated; trailing text is returned. |
| 40 | 2300 | `is_main_thread_user_line_filters_sidechain` | Verifies the helper accepts main-thread `type:user` lines but rejects sidechain user lines and assistant lines. |
| 41 | 2311 | `max_retry_secs_default_when_unset` | Verifies `parse_max_retry_secs(None)` returns the 30s default. |
| 42 | 2316 | `max_retry_secs_env_override` | Verifies numeric env-var strings ("10", "120") are parsed to corresponding Durations. |
| 43 | 2322 | `max_retry_secs_clamps_invalid_input` | Verifies garbage / empty / out-of-range values fall back to default or are clamped to MIN/MAX. |
| 44 | 2336 | `write_complete_once_is_idempotent` | Verifies `write_complete_once` writes on first call, no-ops on second call (preserving the first content). |

**Total:** 44 test functions.

---

## 2. Test Naming Conventions

Tests use snake_case and read like English declarative statements about the system under test. Observed patterns, grouped by verb stem:

- **`parses_*`** (2): assertions about argv parsing.
  - `parses_print_target_and_prompt`, `parses_cwd_aliases_and_bypass_alias`, `parses_session_controls`
- **`rejects_*`** (2): assertions that invalid configurations return `Err`.
  - `rejects_conflicting_session_controls`, `rejects_unsupported_flags`
- **`<target>_maps_*`** (8): assertions about per-target flag translation.
  - `claude_maps_session_controls_to_native_flags`, `gemini_maps_resume_latest`, `codex_maps_session_id_to_resume`, `opencode_maps_explicit_session_ids`, `pi_maps_session_id_to_session`, `cursor_maps_session_controls`, `claude_maps_bypass_to_claude_flag`, `gemini_maps_bypass_to_yolo`, `codex_maps_bypass_to_codex_flag`
- **`<target>_launch_receives_*`** (2): assertions about how the launch command is shaped.
  - `claude_launch_receives_final_prompt_argument`, `codex_launch_receives_final_prompt_argument`
- **`detects_*`** (2): assertions about pane-text classifier predicates.
  - `detects_startup_auto_enter_prompts`, `detects_execution_confirmation_blockers`
- **`extract_*`** (12): assertions about JSON scanning / transcript parsing helpers.
  - `extract_top_level_string_basic`, `extract_top_level_string_handles_escapes`, `extract_final_assistant_text_basic`, `extract_final_assistant_text_walks_past_tool_use`, `extract_final_assistant_text_ignores_sidechain`, `extract_final_assistant_text_skips_thinking`, `extract_final_assistant_text_errors_when_no_text`, `extract_final_assistant_text_skips_malformed_lines`, `extract_final_assistant_text_handles_unicode_text`, `extract_current_turn_skips_prior_turn_assistant`, `extract_current_turn_rejects_when_new_turn_assistant_missing`, `extract_current_turn_handles_tool_use_within_turn`
- **`claude_hooks_settings_*`** (3): assertions about hook JSON generation.
  - `claude_hooks_settings_is_deterministic`, `claude_hooks_settings_handles_path_with_spaces`, `claude_hooks_settings_contains_both_events`
- **`max_retry_secs_*`** (3): assertions about retry-duration parsing.
  - `max_retry_secs_default_when_unset`, `max_retry_secs_env_override`, `max_retry_secs_clamps_invalid_input`
- **Single-purpose / descriptive sentence names** (10):
  - `completion_prompt_keeps_user_prompt_first`, `unsupported_prompt_argument_targets_use_paste_fallback`, `blocked_status_is_failure`, `shell_quote_handles_single_quotes`, `needs_completion_contract_excludes_claude`, `claude_target_rejects_user_settings_collision`, `is_main_thread_user_line_filters_sidechain`, `write_complete_once_is_idempotent`

**Common shape:** `<subject>_<verb>_<object/condition>`. Names embed expectations (e.g., `keeps_user_prompt_first`, `excludes_claude`, `is_idempotent`, `clamps_invalid_input`) — the assertion is in the name. No `test_` prefix; no `it_` prefix.

---

## 3. Test Helpers

Two helper functions are defined inside the `tests` module, before any `#[test]` item:

### Helper 1: `parse` (lines 1845-1848)

```rust
fn parse(argv: &[&str]) -> Options {
    let mut args = argv.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    parse_options(&mut args).unwrap()
}
```

- Takes a `&[&str]` literal-friendly argv slice, allocates a `Vec<String>`, and calls the production `parse_options` (defined at line 171 of the parent module).
- Unwraps on `Err` — tests that want to assert `Err` call `parse_options` directly, or call a downstream function like `target_command(&opts)` and assert on its `Result`.

### Helper 2: `has_pair` (lines 1850-1853)

```rust
fn has_pair(args: &[String], flag: &str, value: &str) -> bool {
    args.windows(2)
        .any(|window| window[0] == flag && window[1] == value)
}
```

- Scans a Vec<String> argv for adjacent `flag value` pairs.
- Used heavily in target-mapping tests to assert that `--resume abc`, `--model sonnet`, etc. appear as consecutive pair in the produced `TargetCommand.args`.

### Tempdir / fixture handling

- **No `tempfile` crate is used** (it is not in dependencies; see Section 6).
- The only fs-touching test, `write_complete_once_is_idempotent` (line 2336), uses **`std::env::temp_dir()`** combined with `make_run_id()` (the production helper at line 1797, which produces a unique `{nanos:x}-{pid:x}` string) to derive a unique directory:

  ```rust
  let dir = env::temp_dir().join(format!("tmuxlet-test-{}", make_run_id()));
  fs::create_dir_all(&dir).unwrap();
  // ... test body ...
  fs::remove_dir_all(&dir).ok();
  ```

- **No fixture-builder for run-dirs** exists. There are no helpers that create marker files (`heartbeat`, `complete.txt`, `meta.json`, `prompt.txt`) inside a synthetic `~/.tmuxlet/runs/<id>/`. Each test that needs file IO constructs paths manually.
- **No helper for `TMUXLET_HOME` overrides** — tests that exercise hook generation (e.g. `claude_hooks_settings_handles_path_with_spaces`) pass a `PathBuf` directly to `claude_hooks_settings`, bypassing `state_dir()` (line 1772) entirely. The production state_dir reads `TMUXLET_HOME` or falls back to `$HOME/.tmuxlet`; tests do not manipulate either env var.

---

## 4. Argument-Parsing Test Structure

All argv-parsing tests use the `parse` helper to convert a `&[&str]` into an `Options`, then assert directly on the `Options` struct fields. They do **not** check stdout or shell side-effects. Tests that want to verify an error use `parse_options(&mut args)` directly (without unwrap) or call a downstream validator like `target_command(&opts)`.

### Verbatim example: `parses_session_controls` (lines 1870-1886)

```rust
1870	    #[test]
1871	    fn parses_session_controls() {
1872	        let opts = parse(&["--continue", "-p", "x"]);
1873	        assert!(opts.continue_latest);
1874	
1875	        let opts = parse(&["--resume", "-p", "x"]);
1876	        assert_eq!(opts.resume, Some(None));
1877	
1878	        let opts = parse(&["--resume", "abc", "-p", "x"]);
1879	        assert_eq!(opts.resume, Some(Some("abc".into())));
1880	
1881	        let opts = parse(&["--session", "abc", "-p", "x"]);
1882	        assert_eq!(opts.session, Some("abc".into()));
1883	
1884	        let opts = parse(&["--session-id", "abc", "-p", "x"]);
1885	        assert_eq!(opts.session_id, Some("abc".into()));
1886	    }
1887	
```

**Pattern observed in this test:**
- Each test groups multiple related scenarios with `let opts = parse(&[...])` repeated; the variable is shadowed each time (no per-scenario sub-fn / table-driven loop).
- Assertions are `assert!` / `assert_eq!` on plain field access of the parsed `Options`.
- The `-p` flag and trailing prompt (`"x"`) appear in nearly every argv to anchor a print-mode-shaped command, even when the test does not care about those values.

### Verbatim example for an error path: `rejects_conflicting_session_controls` (lines 1888-1898)

```rust
1888	    #[test]
1889	    fn rejects_conflicting_session_controls() {
1890	        let opts = parse(&["--continue", "--resume", "abc", "-p", "x"]);
1891	        assert!(target_command(&opts).is_err());
1892	
1893	        let opts = parse(&["--resume", "abc", "--session-id", "def", "-p", "x"]);
1894	        assert!(target_command(&opts).is_err());
1895	
1896	        let opts = parse(&["--session", "abc", "--session-id", "def", "-p", "x"]);
1897	        assert!(target_command(&opts).is_err());
1898	    }
```

The error is surfaced via `target_command` (line 451), not `parse_options` itself; argv-parsing here is permissive, and validation happens downstream.

---

## 5. The Single fs-Touching Test

**`write_complete_once_is_idempotent` (lines 2335-2348)** is the only test that touches the filesystem.

### Verbatim source

```rust
2335	    #[test]
2336	    fn write_complete_once_is_idempotent() {
2337	        let dir = env::temp_dir().join(format!("tmuxlet-test-{}", make_run_id()));
2338	        fs::create_dir_all(&dir).unwrap();
2339	        let path = dir.join("complete.txt");
2340	
2341	        write_complete_once(&path, "first write").unwrap();
2342	        assert_eq!(fs::read_to_string(&path).unwrap(), "first write");
2343	
2344	        write_complete_once(&path, "second write").unwrap();
2345	        assert_eq!(fs::read_to_string(&path).unwrap(), "first write");
2346	
2347	        fs::remove_dir_all(&dir).ok();
2348	    }
```

### Isolation strategy (verbatim from the code)

1. **Unique directory per test invocation:** `env::temp_dir().join(format!("tmuxlet-test-{}", make_run_id()))`. The directory name embeds `make_run_id()` (a process-id + nanosecond-time hex string from line 1797), so concurrent or repeated runs do not collide.
2. **No use of `~/.tmuxlet/runs/`:** the test writes to `$TMPDIR/tmuxlet-test-<id>/complete.txt`, not to the production state dir. It therefore never reads `TMUXLET_HOME` and never calls `state_dir()` (line 1772) or `run_paths()` (line 1779).
3. **No marker files for a fixture run-dir:** only the single `complete.txt` is created — via the production function `write_complete_once` itself, not a helper.
4. **Manual teardown:** `fs::remove_dir_all(&dir).ok();` at the end. Uses `.ok()` to swallow errors (test failure earlier would skip teardown; the unique-id naming prevents collisions on retry).
5. **No `tempfile` crate, no `TempDir` RAII guard** — if the test panics before line 2347, the temp dir leaks. This is the only cleanup mechanism in the suite.

---

## 6. Cargo.toml `[dev-dependencies]` Section

**Verbatim contents of the entire `/Users/kk/Sites/CodefiLabs/tmuxlet/Cargo.toml`:**

```toml
[package]
name = "tmuxlet"
version = "0.3.0"
edition = "2024"
description = "Run interactive coding CLIs through tmux with a claude -p style interface"
license = "MIT"
repository = "https://github.com/CodefiLabs/tmuxlet"

[dependencies]
```

**There is no `[dev-dependencies]` section.** The `[dependencies]` section itself is empty (line 9 is the header, line 10 is blank, and the file ends at line 10). The crate ships zero runtime dependencies and zero dev-dependencies — tests rely entirely on the Rust standard library (`std::env`, `std::fs`, `std::path`, `std::time`).

---

## 7. Classify-Status / Meta-Reading Helpers

### What exists

The codebase has functions that **write** classification artifacts (status strings, meta.json, marker files), and one function that **reads** the runs directory and prints status, but **no single "classify-status" function** that reads a run-dir's marker files and returns a typed enum/struct.

#### `result_status_code` (lines 1242-1244)

```rust
fn result_status_code(status: &str) -> i32 {
    if status == "completed" { 0 } else { 1 }
}
```

- Pure string-to-int translator; the only "status classifier" function in the codebase.
- Tested by `blocked_status_is_failure` (line 2049) which asserts `result_status_code("blocked")` is non-zero.
- The set of status strings produced by `run_print` is hard-coded in that function: `"completed"` (line 334), `"blocked"` (line 370), `"timeout"` (line 389), `"exited"` (line 404). These strings are not centralized in an enum.

#### `handle_status` (lines 1093-1117)

```rust
fn handle_status() -> Result<(), String> {
    let root = state_dir().join("runs");
    if !root.exists() {
        println!("No runs.");
        return Ok(());
    }
    for entry in fs::read_dir(root).map_err(|e| format!("failed to read runs: {e}"))? {
        let entry = entry.map_err(|e| format!("failed to read run: {e}"))?;
        let run_dir = entry.path();
        let meta = run_dir.join("meta.json");
        if !meta.exists() {
            continue;
        }
        println!("{}", fs::read_to_string(&meta).unwrap_or_default().trim());
        let heartbeat = run_dir.join(HEARTBEAT_FILENAME);
        let complete = run_dir.join("complete.txt");
        if heartbeat.exists()
            && !complete.exists()
            && let Some(ago) = heartbeat_age_secs(&heartbeat)
        {
            println!("last activity: {ago}s ago");
        }
    }
    Ok(())
}
```

- **This is the only function that walks `~/.tmuxlet/runs/` and reads marker files** (`meta.json`, `heartbeat`, `complete.txt`). It iterates entries under `state_dir().join("runs")`, prints raw `meta.json` content, and emits a "last activity" line for heartbeats without completion markers.
- **Returns nothing structured** — it prints to stdout and returns `Ok(())`.
- **No tests reference it.** Grepping the test block for `handle_status` returns zero hits.

#### `heartbeat_age_secs` (lines 1119-1125)

```rust
fn heartbeat_age_secs(path: &Path) -> Option<u64> {
    let mtime = fs::metadata(path).ok()?.modified().ok()?;
    SystemTime::now()
        .duration_since(mtime)
        .ok()
        .map(|d| d.as_secs())
}
```

- Pure helper that returns the seconds since a file's mtime, or `None` on error.
- **No tests reference it** (would require mtime fixture manipulation).

#### `run_paths` (lines 1779-1795)

```rust
fn run_paths(run_id: &str) -> Result<RunPaths, String> {
    if run_id.contains('/') || run_id.contains('\\') || run_id.contains("..") {
        return Err("invalid run id".into());
    }
    let root = state_dir().join("runs").join(run_id);
    Ok(RunPaths {
        meta: root.join("meta.json"),
        prompt: root.join("prompt.txt"),
        answer: root.join("answer.txt"),
        complete: root.join("complete.txt"),
        pane_log: root.join("pane.log"),
        error: root.join("error.txt"),
        heartbeat: root.join(HEARTBEAT_FILENAME),
        settings: root.join(SETTINGS_FILENAME),
        root,
    })
}
```

- Builds the canonical `RunPaths` struct (declared at line 94) — every file path inside a run-dir is defined here.
- **No tests reference it.**

### Functions that read or write `meta.json`

- **Writes:** `write_meta` (lines 1246-1268) constructs and writes the JSON string. No tests reference it.
- **Reads:** only `handle_status` (line 1106) reads `meta.json`, and only as raw text passed through to stdout — never parsed.

### Summary

There is **no existing helper that maps a run-dir's marker-file state to a typed status enum** (e.g., one that reads `complete.txt` / `heartbeat` / `error.txt` and returns `Completed` / `Stalled` / `Blocked` / `Exited`). The closest precedents are:

1. **`result_status_code`** (line 1242) — pure `&str -> i32` mapping, the only function in the codebase that "classifies" a status string.
2. **`handle_status`** (line 1093) — the only walker over `~/.tmuxlet/runs/` that reads marker files; emits text, not structured data; untested.
3. **`heartbeat_age_secs`** (line 1119) — utility for reading mtime; untested.
4. **`run_paths`** (line 1779) — utility for building canonical marker-file paths; untested.

The status strings themselves (`"completed"`, `"blocked"`, `"timeout"`, `"exited"`) are produced inline inside `run_print` (lines 334, 370, 389, 404) and are not centralized in an enum or constants block.

---

## Appendix: Additional Observations on Test Style

- **Inline test data:** transcript-parsing tests embed multi-line JSONL via `concat!()` and raw string literals (`r#"..."#`). Example: `extract_final_assistant_text_walks_past_tool_use` at line 2187 builds a 4-line JSONL stream with `concat!(..., "\n", ..., "\n", ...)`.
- **No mocking / no external doubles:** tests exercise pure functions only. Functions that shell out (`run_tmux`, `ensure_tmux`, `tmux_session_exists`, etc.) are not tested. Functions that read transcript files (`extract_final_assistant_text`) are not tested directly — instead, the inner pure function `extract_current_turn_assistant_text_from_str` (line 1578) is tested with in-memory strings.
- **Multi-scenario tests:** most tests pack several related scenarios into one function via shadowed `let opts = parse(&[...])` rebindings rather than splitting into multiple `#[test]` items.
- **Assertions:** `assert!`, `assert_eq!`, `assert_ne!` only. No custom matchers, no `pretty_assertions`, no `insta` snapshots.
- **No `#[should_panic]`, no `#[ignore]`, no `#[cfg(...)]` test gating, no async tests.**
- **Builders:** `Options::default()` (line 65) is used as a fluent base in `claude_target_rejects_user_settings_collision` (line 2138) — the only test that constructs an `Options` directly rather than going through the `parse` helper.
