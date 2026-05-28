# Codebase Analyzer — Print-Run Completion Lifecycle

**File analyzed:** `/Users/kk/Sites/CodefiLabs/tmuxlet/src/main.rs`
**Date:** 2026-05-26
**Purpose:** Document how the print-run completion lifecycle works today, with exact line references, so the auto-kill insertion point and the error-prefix strings used by the cleanup plan are unambiguous.

---

## Important note up front: there is no `wait_for_completion` function

The task brief mentions a "`wait_for_completion` function." That function does not exist in the source. A grep across `src/main.rs` confirms zero matches for the identifier `wait_for_completion`. The polling loop that the brief refers to lives **inline within `run_print`** (`src/main.rs:326-415`). Everything the brief asks for about "wait_for_completion" maps to this inline loop. I document it under that lens below.

---

## 1. `run_print` — end-to-end flow

**Defined at:** `src/main.rs:279`
**Signature:** `fn run_print(mut opts: Options) -> Result<RunResult, String>`
**Called from:** `src/main.rs:161` (inside `run()` when `opts.print == true`)
**Return value:** `Ok(RunResult)` for every terminal status (`completed`, `blocked`, `timeout`, `exited`). The function never returns `Err` after the session has started — terminal failure modes are encoded into the `status` field of `RunResult`. The only `Err` returns are setup-time failures (settings collision, missing tmux, prompt I/O, run-dir creation, prompt write, settings write, meta write, tmux launch).

### Stage-by-stage breakdown

| Stage | Lines | What happens |
|---|---|---|
| Validate settings collision | 280 | `validate_claude_settings_collision(&opts)?` — rejects user `--settings` when target is `claude` (collides with auto-hooks) |
| Ensure tmux on PATH | 281 | `ensure_tmux()?` — runs `tmux -V` |
| Capture start time | 283 | `let started = Instant::now();` — used later for `elapsed_ms` and timeout |
| Generate run id | 284 | `let run_id = make_run_id();` (nanos + pid, hex) |
| Compute tmux session name | 285 | `let tmux_session = format!("tmuxlet-{run_id}");` |
| Compute paths | 286 | `let paths = run_paths(&run_id)?` — populates `RunPaths` (see section 6) |
| Create run dir | 287 | `fs::create_dir_all(&paths.root)` |
| Read/write prompt | 289-290 | `prompt_from(&opts)?` then write to `paths.prompt` |
| (claude only) Write hooks settings | 292-301 | Generates `claude_hooks_settings(...)`, writes to `paths.settings`, sets `opts.settings = Some(<path>)` so the Claude target command picks it up |
| Build final prompt | 303-307 | For non-claude targets, append the `TMUXLET COMPLETION CONTRACT` (`completion_prompt`). For `claude`, the prompt is verbatim (relies on Stop hook for completion) |
| Build target command | 308 | `target_command(&opts)?` |
| Wrap prompt into command if supported | 309-310 | `command_with_prompt(...)` — `claude` and `codex` accept the final prompt as their last positional argument; other targets paste it after launch |
| Write `meta.json` | 312 | `write_meta(&paths, &run_id, &tmux_session, &opts, &base_command)?` |
| Start tmux session | 313 | `start_tmux_session(&tmux_session, &opts.cwd)?` — detached `new-session -d -s <name> -x 220 -y 50 -c <cwd>` |
| Send launch command into pane | 314 | `send_launch_command(&tmux_session, &launch_command)?` — `send-keys ... Enter` |
| Paste prompt if not in launch | 315-317 | For targets that don't accept the prompt as an argument, `paste_text(...)` loads it via tmux paste-buffer and sends Enter |
| Initialize polling state | 319-324 | `startup_auto_enter_deadline`, `startup_enter_attempts`, `last_pane`, `last_pane_changed_at`, `next_pane_poll` |
| Polling loop | 326-415 | See section 2 |

### Where the inline poll loop is

The loop is opened at `src/main.rs:326` (`loop {`) and closed at `src/main.rs:415` (`}`). Every terminal return path inside the loop builds and returns an `Ok(RunResult { ... })`. After the loop body the thread sleeps 500 ms (`src/main.rs:414`) and the loop continues.

`run_print` itself returns the loop's `Ok(RunResult)` — there are no other return paths after the loop begins.

---

## 2. The inline polling loop (the "wait_for_completion" equivalent)

The loop checks four conditions in this fixed order on each iteration:

1. Completion marker present → `completed`
2. Pane poll cadence reached → startup auto-enter + blocked-on-confirmation detection → may return `blocked`
3. Timeout elapsed → `timeout`
4. Tmux session disappeared → `exited`
5. Sleep 500 ms

### Return path A — `completed`

**Lines:** `src/main.rs:327-342`

```rust
327         if paths.complete.exists() {
328             let output = fs::read_to_string(&paths.complete)
329                 .map_err(|e| format!("failed to read completion file: {e}"))?;
330             capture_pane_to_file(&tmux_session, &paths.pane_log).ok();
331             return Ok(RunResult {
332                 id: run_id,
333                 target: opts.target,
334                 status: "completed".into(),
335                 output,
336                 output_format: opts.output_format,
337                 cwd: opts.cwd,
338                 tmux_session,
339                 completion_source: "explicit".into(),
340                 elapsed_ms: started.elapsed().as_millis(),
341             });
342         }
```

- **Trigger:** the file at `paths.complete` exists (i.e. `<run_root>/complete.txt`).
- **Who writes `complete.txt`:**
  - For non-claude targets: the agent itself runs `tmuxlet bridge complete --run <id> --file <answer>` per the completion contract embedded in the prompt (`completion_prompt`, `src/main.rs:790-805`). That subcommand's handler is `handle_bridge_complete` (`src/main.rs:1000-1025`); it reads the answer file and writes its content to `paths.complete` at line 1023.
  - For the `claude` target: the registered `Stop` hook calls `tmuxlet bridge stop-hook --run <id>` (settings JSON built in `claude_hooks_settings`, `src/main.rs:772-788`). The handler `handle_bridge_stop_hook` (`src/main.rs:1027-1055`) parses `transcript_path` from stdin, extracts the final assistant text, then calls `write_complete_once(&paths.complete, &text)` at line 1054. `write_complete_once` (`src/main.rs:1079-1091`) uses `create_new(true)` so only the first writer wins (idempotent).
- **Status string:** `"completed"`
- **Completion source:** `"explicit"`
- **Output:** the full contents of `complete.txt`
- **`capture_pane_to_file` call position:** **before** the return, at line 330. It is invoked using `.ok()` so any failure to capture the pane log is silently swallowed and does not block the completion return.
- **Exit code mapping:** `result_status_code("completed")` returns `0` (`src/main.rs:1242-1244`). Only this path exits 0; all other statuses exit 1.

This is the **exact line where the auto-kill insertion will happen** per the task brief. The surrounding ~10 lines verbatim with line numbers are shown above.

### Return path B — `blocked`

**Lines:** `src/main.rs:357-378`

```rust
357             } else if !pane.trim().is_empty()
358                 && last_pane_changed_at.elapsed() >= Duration::from_secs(EXECUTION_STALL_SECS)
359                 && is_execution_confirmation_blocker(&pane)
360             {
361                 fs::write(&paths.pane_log, &pane).ok();
362                 fs::write(
363                     &paths.error,
364                     blocked_error_message(&opts.target, &run_id, startup_enter_attempts),
365                 )
366                 .ok();
367                 return Ok(RunResult {
368                     id: run_id,
369                     target: opts.target,
370                     status: "blocked".into(),
371                     output: pane,
372                     output_format: opts.output_format,
373                     cwd: opts.cwd,
374                     tmux_session,
375                     completion_source: "fallback".into(),
376                     elapsed_ms: started.elapsed().as_millis(),
377                 });
378             }
```

- **Trigger conditions (all must hold):**
  1. The pane is non-empty (`!pane.trim().is_empty()`).
  2. The pane content has not changed for `EXECUTION_STALL_SECS` (60 s — `src/main.rs:15`).
  3. The current pane text matches `is_execution_confirmation_blocker(...)` (`src/main.rs:896-918` — looks for confirmation/permission/y-n phrases).
- **Status string:** `"blocked"`
- **Completion source:** `"fallback"`
- **Output:** the captured pane text (not a file).
- **Pane log:** written **before** the return at line 361 (`fs::write(&paths.pane_log, &pane).ok();`). Note that this writes the cached `pane` variable directly — it does **not** invoke `capture_pane_to_file` (which would capture 500 lines fresh; here only 200 lines from the cached poll are written).
- **`error.txt`:** written at lines 362-366 using the string produced by `blocked_error_message(...)` (`src/main.rs:924-931`). See section 5 for the verbatim error text.
- **`capture_pane_to_file` call position:** not called on this path. Only the cached pane string is persisted via direct `fs::write`.
- **Exit code mapping:** `result_status_code("blocked")` returns `1` (test at `src/main.rs:2049-2051` asserts this).

The full block from the loop body that contains path B is the `if Instant::now() >= next_pane_poll { ... }` gate at `src/main.rs:344-380`. Inside that gate the loop also handles startup auto-enter behaviour (`src/main.rs:346-352`) and updates change-tracking state (`src/main.rs:354-356`). `next_pane_poll` is advanced by `PANE_POLL_SECS` (1 s) at line 379.

### Return path C — `timeout`

**Lines:** `src/main.rs:382-397`

```rust
382         if started.elapsed() >= opts.timeout {
383             let pane = capture_pane(&tmux_session, 200).unwrap_or_default();
384             fs::write(&paths.pane_log, &pane).ok();
385             fs::write(&paths.error, "timeout waiting for explicit completion\n").ok();
386             return Ok(RunResult {
387                 id: run_id,
388                 target: opts.target,
389                 status: "timeout".into(),
390                 output: pane,
391                 output_format: opts.output_format,
392                 cwd: opts.cwd,
393                 tmux_session,
394                 completion_source: "fallback".into(),
395                 elapsed_ms: started.elapsed().as_millis(),
396             });
397         }
```

- **Trigger:** elapsed time since `started` is at least `opts.timeout` (defaults to `DEFAULT_TIMEOUT_SECS` = 1800 s; `src/main.rs:12`).
- **Status string:** `"timeout"`
- **Completion source:** `"fallback"`
- **Output:** result of `capture_pane(&tmux_session, 200)` (200 lines of pane history) or empty string on failure.
- **Pane log:** written at line 384 from the captured pane string via `fs::write` (again, not `capture_pane_to_file`).
- **`error.txt`:** written at line 385 with the literal string `"timeout waiting for explicit completion\n"` (see section 5).
- **`capture_pane_to_file` call position:** not called on this path. `capture_pane` (not `_to_file`) is used at line 383, and the result is written to `paths.pane_log` at line 384.
- **Exit code mapping:** `result_status_code("timeout")` returns `1`.

### Return path D — `exited`

**Lines:** `src/main.rs:399-412`

```rust
399         if !tmux_session_exists(&tmux_session) {
400             let pane = fs::read_to_string(&paths.pane_log).unwrap_or_default();
401             return Ok(RunResult {
402                 id: run_id,
403                 target: opts.target,
404                 status: "exited".into(),
405                 output: pane,
406                 output_format: opts.output_format,
407                 cwd: opts.cwd,
408                 tmux_session,
409                 completion_source: "fallback".into(),
410                 elapsed_ms: started.elapsed().as_millis(),
411             });
412         }
```

- **Trigger:** `tmux_session_exists(&tmux_session)` returns false (`src/main.rs:933-939`, which runs `tmux has-session -t <name>`).
- **Status string:** `"exited"`
- **Completion source:** `"fallback"`
- **Output:** read back from `paths.pane_log` on disk (whatever was last captured); empty on failure.
- **`error.txt`:** **not** written on this path.
- **`capture_pane_to_file` call position:** not called on this path. The pane is read from the pre-existing `paths.pane_log` file at line 400 — meaning whatever the most recent prior writer (if any) left there.
- **Exit code mapping:** `result_status_code("exited")` returns `1`.

### Loop sleep

After all four conditions are checked and none returned, the loop sleeps:

```
414         thread::sleep(Duration::from_millis(500));
```

---

## 3. `RunResult` struct — fields and how they're used

**Defined at:** `src/main.rs:107-118`

```rust
107 #[derive(Debug)]
108 struct RunResult {
109     id: String,
110     target: String,
111     status: String,
112     output: String,
113     output_format: OutputFormat,
114     cwd: PathBuf,
115     tmux_session: String,
116     completion_source: String,
117     elapsed_ms: u128,
118 }
```

### Field usage

| Field | Source | Used by |
|---|---|---|
| `id` | `make_run_id()` (`src/main.rs:284`) | `result_json` (`src/main.rs:1273`), `emit_result` error message (`src/main.rs:1225`) |
| `target` | `opts.target` | `result_json` (`src/main.rs:1274`) |
| `status` | One of `"completed"`, `"blocked"`, `"timeout"`, `"exited"` | `emit_result` branch logic (`src/main.rs:1205-1228`), `result_status_code` (`src/main.rs:1242-1244`), `result_json` (`src/main.rs:1275`), error message (`src/main.rs:1226`) |
| `output` | `complete.txt` contents (completed), pane text (blocked/timeout), pane log file (exited) | `print_result` text branch prints it (`src/main.rs:1235-1238`), `result_json` (`src/main.rs:1276`) |
| `output_format` | `opts.output_format` | `print_result` chooses JSON vs text (`src/main.rs:1232`) |
| `cwd` | `opts.cwd` | `result_json` (`src/main.rs:1277`) |
| `tmux_session` | `format!("tmuxlet-{run_id}")` (`src/main.rs:285`) | `result_json` (`src/main.rs:1278`) — **this is the field that holds the session name needed for auto-kill** |
| `completion_source` | `"explicit"` (completed) or `"fallback"` (blocked/timeout/exited) | `emit_result` (`src/main.rs:1205-1213`), `result_json` (`src/main.rs:1279`) |
| `elapsed_ms` | `started.elapsed().as_millis()` | `result_json` (`src/main.rs:1280`) |

`RunResult` flows into `emit_result(&result)?` (called from `src/main.rs:162`), which calls `print_result` and then returns either `Ok(())` for status `"completed"` (process exit 0) or `Err(format!(...))` for everything else (process exit 1, via `main()` at lines 126-134).

---

## 4. Auto-kill insertion point — exact location

The auto-kill on successful completion needs to be inserted **inside** the `completed` return path, before the `return Ok(...)`. The literal lines today are reproduced verbatim above in section 2 (lines 327-342). Repeating just the critical window:

```rust
327         if paths.complete.exists() {
328             let output = fs::read_to_string(&paths.complete)
329                 .map_err(|e| format!("failed to read completion file: {e}"))?;
330             capture_pane_to_file(&tmux_session, &paths.pane_log).ok();
331             return Ok(RunResult {
332                 id: run_id,
333                 target: opts.target,
334                 status: "completed".into(),
335                 output,
336                 output_format: opts.output_format,
337                 cwd: opts.cwd,
338                 tmux_session,
339                 completion_source: "explicit".into(),
340                 elapsed_ms: started.elapsed().as_millis(),
341             });
342         }
```

The most natural insertion is after the `capture_pane_to_file` call at line 330 and before the `return Ok(RunResult { ... })` at line 331. `tmux_session` is still owned by the function at that point (it's moved into the `RunResult` at line 338, so any read of it has to happen before line 331). The variable holds the value `"tmuxlet-<run_id>"`.

Existing kill-session pattern to mirror is in `handle_stop` (`src/main.rs:1185-1190`):

```rust
1185             run_tmux([
1186                 OsStr::new("kill-session"),
1187                 OsStr::new("-t"),
1188                 OsStr::new(&name),
1189             ])?;
```

---

## 5. Verbatim `error.txt` contents for `timeout` and `blocked`

### `timeout` — `src/main.rs:385`

The literal string written to `paths.error` is exactly:

```
timeout waiting for explicit completion\n
```

In source (line 385):

```rust
385             fs::write(&paths.error, "timeout waiting for explicit completion\n").ok();
```

First-line prefix for classification: `timeout waiting for`.

### `blocked` — produced by `blocked_error_message` at `src/main.rs:924-931`

Definition:

```rust
924 fn blocked_error_message(target: &str, run_id: &str, startup_enter_attempts: usize) -> String {
925     format!(
926         "blocked waiting for a confirmation or permission prompt\n\
927 target: {target}\n\
928 startup auto-enter attempts: {startup_enter_attempts}\n\
929 hint: rerun with --dangerously-skip-permissions if supported by this target, or attach with tmuxlet attach {run_id} to resolve manually\n"
930     )
931 }
```

Rendered shape (with substitution placeholders left as-is for the plan):

```
blocked waiting for a confirmation or permission prompt
target: <target>
startup auto-enter attempts: <startup_enter_attempts>
hint: rerun with --dangerously-skip-permissions if supported by this target, or attach with tmuxlet attach <run_id> to resolve manually
```

First-line prefix for classification: `blocked waiting for`.

The write call is at `src/main.rs:362-366`:

```rust
362                 fs::write(
363                     &paths.error,
364                     blocked_error_message(&opts.target, &run_id, startup_enter_attempts),
365                 )
366                 .ok();
```

### Summary of error-prefix classification table

| Status | First-line prefix | Source line |
|---|---|---|
| `timeout` | `timeout waiting for` | `src/main.rs:385` |
| `blocked` | `blocked waiting for` | `src/main.rs:926` (inside `blocked_error_message`) |
| `exited` | (no `error.txt` written) | — |
| `completed` | (no `error.txt` written) | — |

---

## 6. `RunPaths` struct — filenames confirmed

**Struct definition (`src/main.rs:94-105`):**

```rust
 94 #[derive(Debug)]
 95 struct RunPaths {
 96     root: PathBuf,
 97     meta: PathBuf,
 98     prompt: PathBuf,
 99     answer: PathBuf,
100     complete: PathBuf,
101     pane_log: PathBuf,
102     error: PathBuf,
103     heartbeat: PathBuf,
104     settings: PathBuf,
105 }
```

**Path construction (`src/main.rs:1779-1795`):**

```rust
1779 fn run_paths(run_id: &str) -> Result<RunPaths, String> {
1780     if run_id.contains('/') || run_id.contains('\\') || run_id.contains("..") {
1781         return Err("invalid run id".into());
1782     }
1783     let root = state_dir().join("runs").join(run_id);
1784     Ok(RunPaths {
1785         meta: root.join("meta.json"),
1786         prompt: root.join("prompt.txt"),
1787         answer: root.join("answer.txt"),
1788         complete: root.join("complete.txt"),
1789         pane_log: root.join("pane.log"),
1790         error: root.join("error.txt"),
1791         heartbeat: root.join(HEARTBEAT_FILENAME),
1792         settings: root.join(SETTINGS_FILENAME),
1793         root,
1794     })
1795 }
```

Where:

- `state_dir()` (`src/main.rs:1772-1777`) returns `$TMUXLET_HOME` if set, else `$HOME/.tmuxlet`, else `./.tmuxlet`.
- `HEARTBEAT_FILENAME` (`src/main.rs:17`) is `"heartbeat"` — confirmed bare filename, no extension.
- `SETTINGS_FILENAME` (`src/main.rs:18`) is `"settings.json"`.

**Confirmation of brief's expectations:**

| Brief expectation | Actual filename | Line |
|---|---|---|
| `meta.json` | `meta.json` | 1785 |
| `complete.txt` | `complete.txt` | 1788 |
| `error.txt` | `error.txt` | 1790 |
| `heartbeat` | `heartbeat` (via `HEARTBEAT_FILENAME`) | 17, 1791 |

All four match. Additionally present in the struct: `root`, `prompt` (`prompt.txt`), `answer` (`answer.txt`), `pane_log` (`pane.log`), `settings` (`settings.json`).

---

## 7. Tmux helpers — what they are, how they're called, what they accept

### `run_tmux` — the single tmux invocation primitive

**Defined at:** `src/main.rs:958-972`

```rust
958 fn run_tmux<I, S>(args: I) -> Result<String, String>
959 where
960     I: IntoIterator<Item = S>,
961     S: AsRef<OsStr>,
962 {
963     let output = Command::new("tmux")
964         .args(args)
965         .output()
966         .map_err(|e| format!("failed to execute tmux: {e}"))?;
967     if output.status.success() {
968         Ok(String::from_utf8_lossy(&output.stdout).to_string())
969     } else {
970         Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
971     }
972 }
```

- Accepts any iterable of `OsStr`-like values.
- Spawns `tmux` once and waits for it.
- Returns stdout as `Ok(String)` on success, stderr as `Err(String)` on failure.

### Callers of `run_tmux` (all the tmux operations in the codebase)

| Caller | Lines | Args passed |
|---|---|---|
| `start_tmux_session(name, cwd)` | 807-821 | `new-session -d -s <name> -x 220 -y 50 -c <cwd>` |
| `send_launch_command(tmux_session, command)` | 823-838 | `send-keys -t <session> <shell-quoted line> Enter` |
| `paste_text(tmux_session, text)` | 840-859 | `load-buffer <tmpfile>`, then `paste-buffer -t <session>`, sleep 200 ms, then `send-keys -t <session> Enter` |
| `send_enter(tmux_session)` | 861-869 | `send-keys -t <session> Enter` |
| `capture_pane(tmux_session, lines)` | 941-951 | `capture-pane -t <session> -p -S -<lines>` (returns the pane buffer as String) |
| `capture_pane_to_file(tmux_session, path)` | 953-956 | Calls `capture_pane(tmux_session, 500)` then `fs::write(path, output)` |
| `handle_stop(args)` — graceful path | 1175-1182 | `send-keys -t <name> /exit Enter` |
| `handle_stop(args)` — kill path | 1185-1189 | `kill-session -t <name>` |

### Direct `Command::new("tmux")` invocations (bypassing `run_tmux`)

| Caller | Lines | Why direct |
|---|---|---|
| `tmux_session_exists(name)` | 933-939 | Needs only the exit status, not stdout/stderr — `tmux has-session -t <name>`, returns `bool` |
| `handle_attach(args)` | 1158-1169 | Needs to inherit the user's terminal (`status()` rather than `output()`) — `tmux attach-session -t <name>` |
| `ensure_tmux()` | 974-986 | Probes for tmux on PATH — `tmux -V` |

### Existing kill-session helper

There is no standalone `kill_session` helper. The only kill-session call in the codebase lives in `handle_stop` (`src/main.rs:1185-1190`):

```rust
1185             run_tmux([
1186                 OsStr::new("kill-session"),
1187                 OsStr::new("-t"),
1188                 OsStr::new(&name),
1189             ])?;
```

The control flow in `handle_stop` is:
1. Check `tmux_session_exists(&name)` (line 1174).
2. If yes, attempt graceful shutdown by sending `/exit` + Enter (lines 1175-1182). The error is `.ok()`-discarded.
3. Sleep 2 seconds (line 1183).
4. If the session still exists, hard-kill it via `kill-session` (lines 1184-1190).

This is the pattern that the auto-kill on the completed path can mirror, though for the `completed` case the agent has already finished so a direct `kill-session` is sufficient — there is no need for the `/exit`-then-wait dance.

---

## 8. `tmux_session` variable — naming and lifetime

**Where set:** `src/main.rs:285`

```rust
285     let tmux_session = format!("tmuxlet-{run_id}");
```

- `run_id` comes from `make_run_id()` at line 284, which produces a string of the form `<nanos-hex>-<pid-hex>` (`src/main.rs:1797-1803`).
- So the resulting tmux session name is always `tmuxlet-<nanos-hex>-<pid-hex>`, e.g. `tmuxlet-17e3a9f2c1234abc-1f2`.

**How it flows through `run_print`:**

- Used as the `-s` value when starting the session (line 313 → `start_tmux_session`).
- Used as the `-t` target for every send/paste/capture operation inside the loop.
- Passed to `tmux_session_exists` for the `exited` check (line 399).
- Embedded in `meta.json` via `write_meta(..., &tmux_session, ...)` at line 312 (which writes the `"tmuxSession"` key, `src/main.rs:1254-1265`).
- Moved into each `RunResult` via the `tmux_session` field at the return sites (lines 338, 374, 393, 408).

**The `tmuxlet-<run-id>` convention is also enforced symmetrically by `tmux_name_from_id`** (`src/main.rs:1196-1202`), which `handle_read`, `handle_send`, `handle_attach`, and `handle_stop` all use to translate a user-supplied run id back into a tmux session name:

```rust
1196 fn tmux_name_from_id(id: &str) -> String {
1197     if id.starts_with("tmuxlet-") {
1198         id.to_string()
1199     } else {
1200         format!("tmuxlet-{id}")
1201     }
1202 }
```

This means anywhere downstream the kill code needs to reconstitute a session name from a run id, it can use either the live `tmux_session` variable inside `run_print` or `tmux_name_from_id(run_id)` for symmetry with the other handlers.

---

## 9. Cross-references useful for the cleanup plan

- **Insertion target line:** `src/main.rs:331` (the line that starts the `completed` `return Ok(RunResult { ... })`). The kill call goes immediately above it, after the existing `capture_pane_to_file(...).ok();` at line 330.
- **Variable to pass to `kill-session`:** the still-owned `tmux_session: String` at that point in scope.
- **Helper to use:** `run_tmux([OsStr::new("kill-session"), OsStr::new("-t"), OsStr::new(&tmux_session)])` matching the pattern at `src/main.rs:1185-1189`.
- **Error handling pattern to mirror:** the existing helper at line 330 (`capture_pane_to_file(...).ok();`) silently drops errors so completion isn't delayed by a failing tmux call — the same `.ok()` is the right shape for an auto-kill that shouldn't break the success path.
- **error.txt prefixes for status classification (when needed elsewhere):**
  - `"timeout waiting for"` → `timeout`
  - `"blocked waiting for"` → `blocked`
  - absence of `error.txt` + `complete.txt` present → `completed`
  - absence of `error.txt` + tmux session gone → `exited`
- **Files that should remain after a completed-and-killed run** (so audits and `tmuxlet status` continue to work): `meta.json`, `prompt.txt`, `answer.txt` (if non-claude), `complete.txt`, `pane.log`, `heartbeat`, `settings.json` (if claude). The cleanup plan should kill only the tmux session, not the run directory.
- **The `exited` path is the one most affected by auto-kill semantics:** because step 4 of the loop checks whether the session exists, an auto-kill that races a slower iteration of the loop will not change the outcome (the `completed` return happens before the auto-kill so the session-gone check never runs again in this `run_print` call). But any external caller polling the same tmux session name after `completed` returns will see it absent, which matches the explicit-kill semantics already used in `handle_stop`.
