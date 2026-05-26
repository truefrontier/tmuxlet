# tmuxlet CLI Dispatch and Subcommand Handler Analysis

Source file: `/Users/kk/Sites/CodefiLabs/tmuxlet/src/main.rs`
Total length: 2350 lines

This analysis documents the current state of the CLI dispatch logic, existing
subcommand handlers, flag parsing patterns, error handling conventions, path
resolution, and tmux session lifecycle helpers — strictly describing how the
code works today, without recommendations.

---

## 1. `main()` and `run()` — argv parsing and subcommand dispatch

### `main()` — `src/main.rs:126-134`

`main` is a thin wrapper that converts a `Result<(), String>` from `run` into
an `ExitCode`. On error it prepends the literal `"tmuxlet: "` prefix and writes
to stderr, then exits with code `1`.

```rust
126	fn main() -> ExitCode {
127	    match run() {
128	        Ok(()) => ExitCode::SUCCESS,
129	        Err(err) => {
130	            eprintln!("tmuxlet: {err}");
131	            ExitCode::from(1)
132	        }
133	    }
134	}
```

### `run()` — full dispatch block — `src/main.rs:136-169`

`run()` collects `env::args().skip(1)` into a `Vec<String>`. If empty, it
prints help and returns `Ok`. Otherwise it dispatches on `args[0]`:

```rust
136	fn run() -> Result<(), String> {
137	    let mut args: Vec<String> = env::args().skip(1).collect();
138	    if args.is_empty() {
139	        print_help();
140	        return Ok(());
141	    }
142	
143	    match args[0].as_str() {
144	        "-h" | "--help" => {
145	            print_help();
146	            Ok(())
147	        }
148	        "-v" | "--version" => {
149	            println!("tmuxlet {VERSION}");
150	            Ok(())
151	        }
152	        "bridge" => handle_bridge(&args[1..]),
153	        "status" => handle_status(),
154	        "read" => handle_read(&args[1..]),
155	        "send" => handle_send(&args[1..]),
156	        "attach" => handle_attach(&args[1..]),
157	        "stop" => handle_stop(&args[1..]),
158	        _ => {
159	            let opts = parse_options(&mut args)?;
160	            if opts.print {
161	                let result = run_print(opts)?;
162	                emit_result(&result)?;
163	            } else {
164	                return Err("tmuxlet currently supports print mode only; pass -p/--print".into());
165	            }
166	            Ok(())
167	        }
168	    }
169	}
```

Dispatch shape observations:

- Subcommand strings are matched as the first positional arg only — no
  subcommand abbreviation, no looking through the args list.
- `status` is the only subcommand that receives no slice (takes no args).
- `bridge`, `read`, `send`, `attach`, `stop` all receive `&args[1..]`.
- The `_` branch reparses the full `args` slice with `parse_options` (which
  takes `&mut [String]`) to handle `-p`/`--print` mode (the default run mode).
- `handle_bridge` (line 152) further dispatches into `complete`, `stop-hook`,
  and `tool-hook` sub-subcommands at `src/main.rs:988-998`.

---

## 2. `handle_status()` — `src/main.rs:1093-1117`

Verbatim:

```rust
1093	fn handle_status() -> Result<(), String> {
1094	    let root = state_dir().join("runs");
1095	    if !root.exists() {
1096	        println!("No runs.");
1097	        return Ok(());
1098	    }
1099	    for entry in fs::read_dir(root).map_err(|e| format!("failed to read runs: {e}"))? {
1100	        let entry = entry.map_err(|e| format!("failed to read run: {e}"))?;
1101	        let run_dir = entry.path();
1102	        let meta = run_dir.join("meta.json");
1103	        if !meta.exists() {
1804	            continue;
1105	        }
1106	        println!("{}", fs::read_to_string(&meta).unwrap_or_default().trim());
1107	        let heartbeat = run_dir.join(HEARTBEAT_FILENAME);
1108	        let complete = run_dir.join("complete.txt");
1109	        if heartbeat.exists()
1110	            && !complete.exists()
1111	            && let Some(ago) = heartbeat_age_secs(&heartbeat)
1112	        {
1113	            println!("last activity: {ago}s ago");
1114	        }
1115	    }
1116	    Ok(())
1117	}
```

How it works:

- **Root path**: `state_dir().join("runs")`. The `state_dir()` helper at
  `src/main.rs:1772-1777` resolves `~/.tmuxlet/runs/` (see section 4).
- **Empty-state branch (1095-1098)**: If the runs directory doesn't exist, it
  prints the literal string `"No runs."` and returns Ok. No error.
- **Iteration (1099-1115)**: Uses `fs::read_dir` directly. Each `entry?` is
  unwrapped via `map_err` into `Result<(), String>`. Order is whatever the OS
  returns (not explicitly sorted).
- **Per-run gating (1102-1105)**: Each entry must contain `meta.json` —
  otherwise the directory is silently skipped. There is no validation that the
  entry is itself a directory.
- **Meta print (1106)**: Reads `meta.json` with `unwrap_or_default()` (so a
  read error yields an empty string), then `.trim()` is printed via `println!`.
  The meta blob is the single-line JSON written by `write_meta`
  (`src/main.rs:1246-1268`) with fields:
  `id`, `target`, `tmuxSession`, `cwd`, `program`, `args`, `outputFormat`.
- **Heartbeat age (1107-1114)**: Reads `heartbeat` file (filename constant
  `HEARTBEAT_FILENAME = "heartbeat"` at `src/main.rs:17`). Only prints
  "last activity: Xs ago" when the heartbeat file exists AND no `complete.txt`
  exists AND `heartbeat_age_secs` returns a value. The `let Some(...) = ...`
  uses Rust's `let_chains` syntax.
- **Heartbeat age computation** — `heartbeat_age_secs` at lines 1119-1125:
  ```rust
  1119	fn heartbeat_age_secs(path: &Path) -> Option<u64> {
  1120	    let mtime = fs::metadata(path).ok()?.modified().ok()?;
  1121	    SystemTime::now()
  1122	        .duration_since(mtime)
  1123	        .ok()
  1124	        .map(|d| d.as_secs())
  1125	    }
  ```
  Reads the file's mtime via `fs::metadata().modified()`. The heartbeat
  contents are not read — only the file mtime is used. The heartbeat is
  written by `handle_bridge_tool_hook` (`src/main.rs:1057-1077`) which stores
  `SystemTime::now().duration_since(UNIX_EPOCH).as_millis()` as a string —
  but `handle_status` ignores the file body.

Fields printed in `handle_status`:

1. Trimmed meta.json line (one per active run).
2. `last activity: Xs ago` (when the heartbeat conditions are met).

No filtering by tmux liveness, no flag parsing — the function takes no
arguments and accepts no options.

---

## 3. `handle_stop()` — `src/main.rs:1171-1194`

Verbatim:

```rust
1171	fn handle_stop(args: &[String]) -> Result<(), String> {
1172	    let id = args.first().ok_or("usage: tmuxlet stop <id>")?;
1173	    let name = tmux_name_from_id(id);
1174	    if tmux_session_exists(&name) {
1175	        run_tmux([
1176	            OsStr::new("send-keys"),
1177	            OsStr::new("-t"),
1178	            OsStr::new(&name),
1179	            OsStr::new("/exit"),
1180	            OsStr::new("Enter"),
1181	        ])
1182	        .ok();
1183	        thread::sleep(Duration::from_secs(2));
1184	        if tmux_session_exists(&name) {
1185	            run_tmux([
1186	                OsStr::new("kill-session"),
1187	                OsStr::new("-t"),
1188	                OsStr::new(&name),
1189	            ])?;
1190	        }
1191	    }
1192	    println!("Stopped {id}");
1193	    Ok(())
1194	}
```

How it works:

- **Arg parsing (1172)**: Takes only the first positional arg as `<id>`. No
  flag parsing, no `--force`, no `--dry-run`. If missing, returns
  `Err("usage: tmuxlet stop <id>")` — note this is also the literal error
  string returned via `Result<(), String>`.
- **Name resolution (1173)**: Calls `tmux_name_from_id` at
  `src/main.rs:1196-1202`, which is permissive — it prepends `"tmuxlet-"`
  unless the id already starts with it:
  ```rust
  1196	fn tmux_name_from_id(id: &str) -> String {
  1197	    if id.starts_with("tmuxlet-") {
  1198	        id.to_string()
  1199	    } else {
  1200	        format!("tmuxlet-{id}")
  1201	    }
  1202	}
  ```
  No validation against the runs directory — the id is not required to
  correspond to any `~/.tmuxlet/runs/<id>` entry.
- **Existence guard (1174)**: Uses `tmux_session_exists` (see section 7). If
  no session exists, the entire if-block is skipped and the function prints
  "Stopped {id}" and returns Ok — i.e. the operation is silently a no-op when
  there is no tmux session.
- **Graceful exit attempt (1175-1183)**: Sends the literal string `/exit`
  followed by `Enter` via `tmux send-keys`. The `run_tmux(...).ok()` discards
  the tmux command result — failure to send-keys does not propagate. Then
  `thread::sleep(Duration::from_secs(2))` is a fixed 2-second wait — there is
  no configurable grace period, no polling, no early exit if the session dies
  faster.
- **Hard kill (1184-1190)**: After the 2s sleep, checks existence again; if
  still alive, calls `tmux kill-session -t <name>` via `run_tmux(...)?`. The
  `?` here means a kill-session failure DOES propagate as an `Err`, unlike
  the graceful send-keys step.
- **No filesystem cleanup**: The function does not delete or otherwise touch
  `~/.tmuxlet/runs/<id>/` — `meta.json`, `complete.txt`, `heartbeat`,
  `pane.log`, etc. all remain after `handle_stop`.
- **Always prints "Stopped {id}"**: Line 1192 prints unconditionally to
  stdout regardless of whether a session was killed, even when the session
  never existed.

---

## 4. `~/.tmuxlet/runs/` root path resolution — `state_dir()` at `src/main.rs:1772-1777`

```rust
1772	fn state_dir() -> PathBuf {
1773	    env::var_os("TMUXLET_HOME")
1774	        .map(PathBuf::from)
1775	        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".tmuxlet")))
1776	        .unwrap_or_else(|| PathBuf::from(".tmuxlet"))
1777	    }
```

Resolution chain (in order):

1. `TMUXLET_HOME` env var — used as-is via `PathBuf::from`. No
   tilde-expansion, no validation, no canonicalization.
2. Otherwise `$HOME/.tmuxlet`.
3. Otherwise the literal relative path `.tmuxlet` (relative to current
   working directory).

The `runs` subdirectory is always derived by callers via
`state_dir().join("runs")` — there is no dedicated `runs_dir()` helper.

`run_paths(run_id)` at `src/main.rs:1779-1795` is the central per-run path
builder. It validates that the run id does not contain `/`, `\`, or `..`:

```rust
1779	fn run_paths(run_id: &str) -> Result<RunPaths, String> {
1780	    if run_id.contains('/') || run_id.contains('\\') || run_id.contains("..") {
1781	        return Err("invalid run id".into());
1782	    }
1783	    let root = state_dir().join("runs").join(run_id);
1784	    Ok(RunPaths {
1785	        meta: root.join("meta.json"),
1786	        prompt: root.join("prompt.txt"),
1787	        answer: root.join("answer.txt"),
1788	        complete: root.join("complete.txt"),
1789	        pane_log: root.join("pane.log"),
1790	        error: root.join("error.txt"),
1791	        heartbeat: root.join(HEARTBEAT_FILENAME),
1792	        settings: root.join(SETTINGS_FILENAME),
1793	        root,
1794	    })
1795	}
```

The `RunPaths` struct at lines 94-105 contains: `root`, `meta`, `prompt`,
`answer`, `complete`, `pane_log`, `error`, `heartbeat`, `settings`.

The HOME tilde-expansion logic for cwd-style flags lives in
`expand_home` at lines 1805-1817 — but `state_dir()` itself does not
tilde-expand `TMUXLET_HOME`.

Important: `handle_status` calls `state_dir()` directly at line 1094 rather
than going through `run_paths`, so it does not perform the run-id validation
on subdirectory names; it simply iterates whatever entries `fs::read_dir`
returns.

---

## 5. Flag parsing patterns

### Primary parser — `parse_options` at `src/main.rs:171-237`

This is the rich parser used for the implicit print-mode dispatch (line 159).
It is a hand-rolled `while i < args.len()` loop with a giant `match` on each
arg. It uses helper functions `take_value` (line 239) and `optional_value`
(line 247):

```rust
239	fn take_value(args: &[String], i: &mut usize, flag: &str) -> Result<String, String> {
240	    *i += 1;
241	    args.get(*i)
242	        .cloned()
243	        .filter(|v| !v.is_empty())
244	        .ok_or_else(|| format!("{flag} requires a value"))
245	}
246	
247	fn optional_value(args: &[String], i: &mut usize) -> Option<String> {
248	    if let Some(next) = args.get(*i + 1)
249	        && !next.starts_with('-')
250	    {
251	        *i += 1;
252	        return Some(next.clone());
253	    }
254	    None
255	}
```

The `parse_options` loop also handles `--` as a remainder separator (line
226) and treats any unknown `-` prefix as an error (line 230).

`parse_options` is NOT used by any of the subcommand handlers
(`handle_status`, `handle_read`, `handle_send`, `handle_attach`,
`handle_stop`, `handle_bridge`); it is reserved for the print-mode "default"
branch.

### Per-subcommand flag handling

Each subcommand handler reimplements its own minimal parser inline. They do
NOT share a helper; they are independent ad-hoc loops:

**`handle_read`** — `src/main.rs:1127-1147` — parses one optional flag,
`--lines N`:

```rust
1127	fn handle_read(args: &[String]) -> Result<(), String> {
1128	    let id = args.first().ok_or("usage: tmuxlet read <id> [--lines N]")?;
1129	    let mut lines = 80usize;
1130	    let mut i = 1;
1131	    while i < args.len() {
1132	        match args[i].as_str() {
1133	            "--lines" => {
1134	                i += 1;
1135	                lines = args
1136	                    .get(i)
1137	                    .ok_or("--lines requires a value")?
1138	                    .parse()
1139	                    .map_err(|_| "--lines must be an integer".to_string())?;
1140	            }
1141	            other => return Err(format!("unknown read option: {other}")),
1142	        }
1143	        i += 1;
1144	    }
1145	    print!("{}", capture_pane(&tmux_name_from_id(id), lines)?);
1146	    Ok(())
1147	}
```

Pattern: `args.first()` for the positional `<id>`, then `while i < args.len()`
starting at index 1, increments `i` once after consuming each flag's value.

**`handle_bridge_complete`** — `src/main.rs:1000-1025` — parses `--run`
and `--file`:

```rust
1000	fn handle_bridge_complete(args: &[String]) -> Result<(), String> {
1001	    let mut run_id = None;
1002	    let mut file = None;
1003	    let mut i = 0;
1004	    while i < args.len() {
1005	        match args[i].as_str() {
1006	            "--run" => {
1007	                i += 1;
1008	                run_id = args.get(i).cloned();
1009	            }
1010	            "--file" => {
1011	                i += 1;
1012	                file = args.get(i).map(PathBuf::from);
1013	            }
1014	            other => return Err(format!("unknown bridge complete option: {other}")),
1015	        }
1016	        i += 1;
1017	    }
1018	    let run_id = run_id.ok_or("--run is required")?;
1019	    let file = file.ok_or("--file is required")?;
```

**`handle_bridge_stop_hook`** — `src/main.rs:1027-1055` — same pattern with
just `--run`.

**`handle_bridge_tool_hook`** — `src/main.rs:1057-1077` — looser; uses
`if args[i].as_str() == "--run"` (no else-error), so unknown args are
silently ignored.

### Flags like `-y`/`--yes` / `--dry-run`

Searched the entire file — there are NO occurrences of `--yes`, `-y`,
`--dry-run`, `--force` (for stop), `--orphans`, `--clean`, or `--all` flags
on any subcommand handler. The only `--force` is a value-less mapping inside
`parse_options` at line 205, aliased to `--dangerously-skip-permissions`:

```rust
203	            "--dangerously-skip-permissions"
204	            | "--yolo"
205	            | "--force"
206	            | "--dangerously-bypass-approvals-and-sandbox" => {
207	                opts.dangerously_skip_permissions = true;
208	            }
```

So `handle_stop`, `handle_status`, `handle_read`, `handle_send`,
`handle_attach`, and `handle_bridge` currently have no confirmation,
no-dry-run-mode, no force-flag scaffolding.

---

## 6. Error returns and stdout/stderr conventions

### Return type

Every CLI handler returns `Result<(), String>`. The pattern is consistent:

- `handle_status() -> Result<(), String>` (line 1093)
- `handle_read(args: &[String]) -> Result<(), String>` (line 1127)
- `handle_send(args: &[String]) -> Result<(), String>` (line 1149)
- `handle_attach(args: &[String]) -> Result<(), String>` (line 1158)
- `handle_stop(args: &[String]) -> Result<(), String>` (line 1171)
- `handle_bridge(args: &[String]) -> Result<(), String>` (line 988)
- `handle_bridge_complete` / `_stop_hook` / `_tool_hook` (lines 1000, 1027,
  1057)
- `run() -> Result<(), String>` (line 136)

### Error printing

Error printing is centralized in `main()` at lines 129-132. Handlers never
call `eprintln!` themselves — they return `Err(String)` and `main` formats it
as `tmuxlet: {err}` then sets exit code `1`.

Example error strings used in handlers (note: no trailing newlines, no
"Error:" prefix — `main()` adds the `tmuxlet:` prefix):

- `"usage: tmuxlet stop <id>"` (line 1172)
- `"usage: tmuxlet read <id> [--lines N]"` (line 1128)
- `"usage: tmuxlet send <id> <message>"` (line 1150, 1153)
- `"usage: tmuxlet attach <id>"` (line 1159)
- `"unknown read option: {other}"` (line 1141)
- `"--run is required"` (line 1018)
- `"--file is required"` (line 1019)
- `"failed to read runs: {e}"` (line 1099)
- `"invalid run id"` (line 1781)

### Stdout printing

Handlers print success output via plain `println!`/`print!`:

- `handle_status`: `println!("No runs.")`, then per-run `println!` of meta
  contents and `println!("last activity: {ago}s ago")`.
- `handle_stop`: `println!("Stopped {id}")`.
- `handle_read`: `print!("{}", capture_pane(...))` — no trailing newline
  forced.
- `handle_send` and `handle_attach`: no stdout on success.

### Exit codes

Only two codes exist: `ExitCode::SUCCESS` (0) on `Ok`, and `ExitCode::from(1)`
on any `Err`. There is no nuanced exit-code mapping for partial success or
"no-op" scenarios — `handle_stop` returns Ok and exit 0 even when the
session never existed.

The print-mode result mapping has its own helper `result_status_code` at
lines 1242-1244 that distinguishes "completed" (0) from everything else (1),
but that is only used by `emit_result`/`run_print`, not by the lifecycle
handlers analyzed above.

---

## 7. tmux liveness helpers

### `tmux_session_exists` — `src/main.rs:933-939`

This is the only "is the session alive" helper in the file:

```rust
933	fn tmux_session_exists(name: &str) -> bool {
934	    Command::new("tmux")
935	        .args(["has-session", "-t", name])
936	        .output()
937	        .map(|o| o.status.success())
938	        .unwrap_or(false)
939	    }
```

Behavior:

- Shells out to `tmux has-session -t <name>` directly (bypasses the
  `run_tmux` wrapper).
- Returns `true` only when the subprocess exits with success.
- Any error from `Command::output` (e.g. tmux not on PATH) is swallowed
  into `false` via `unwrap_or(false)`.
- Suppresses stderr capture only inadvertently (`output()` captures it but
  it's discarded).

### Callsites — full inventory

- `src/main.rs:399` — inside `run_print`'s poll loop. If the session
  disappears mid-run, `run_print` returns a `RunResult` with status
  `"exited"` and completion source `"fallback"`.
- `src/main.rs:1174` — `handle_stop` initial gate: only attempts shutdown
  when a session exists.
- `src/main.rs:1184` — `handle_stop` second check: after the 2s sleep and
  `/exit` keystrokes, decides whether to escalate to `kill-session`.

### Related tmux helpers (for completeness)

`run_tmux` at `src/main.rs:958-972` — the generic tmux invocation wrapper.
Returns `Result<String, String>` carrying stdout on success and trimmed
stderr on failure. Used by:

- `start_tmux_session` (line 807) — `new-session -d -s <name> -x 220 -y 50 -c <cwd>`.
- `send_launch_command` (line 823) — `send-keys -t <name> <line> Enter`.
- `paste_text` (line 840) — `load-buffer` then `paste-buffer` then
  `send-keys Enter`.
- `send_enter` (line 861) — `send-keys -t <name> Enter`.
- `capture_pane` (line 941) — `capture-pane -t <name> -p -S -<lines>`.
- `handle_stop` (lines 1175, 1185) — `send-keys` and `kill-session`.

`ensure_tmux` at lines 974-986 — invoked only once during `run_print`
(line 281) to verify tmux is installed by calling `tmux -V`. Not used by
lifecycle subcommands.

There is no helper for listing all live tmuxlet sessions (e.g. wrapping
`tmux list-sessions -F '#{session_name}'` and filtering by prefix). The
project has no current `tmux_list_sessions` or equivalent.

---

## Cross-cutting observations

- **Constants for filenames**: `HEARTBEAT_FILENAME = "heartbeat"` (line 17)
  and `SETTINGS_FILENAME = "settings.json"` (line 18). Other filenames
  (`meta.json`, `complete.txt`, `prompt.txt`, `answer.txt`, `pane.log`,
  `error.txt`) are not constants — they appear as string literals in
  `run_paths` (1785-1792) and at the callsite in `handle_status`
  (1102, 1108).
- **Session-name prefix**: The literal prefix `"tmuxlet-"` appears at
  `src/main.rs:285` (creation: `format!("tmuxlet-{run_id}")`) and at
  `src/main.rs:1196-1202` (`tmux_name_from_id`). There is no shared
  constant for the prefix.
- **No global `runs_dir()` helper**: Callers use `state_dir().join("runs")`
  inline (line 1094 in `handle_status`; line 1783 in `run_paths`).
- **No "is this run still active" composite check**: Nowhere in the file
  is there a function that combines `tmux_session_exists` with
  `complete.txt` presence and heartbeat staleness. `handle_status` reads
  meta and heartbeat but never checks `tmux_session_exists` against the
  `tmuxSession` field from the meta blob.
- **Test coverage for stop/status**: The `#[cfg(test)]` block
  (lines 1842-2349) covers `parse_options`, target command mappers,
  completion contract, pane-text heuristics, JSON helpers, and the
  transcript extractor. There are NO unit tests for `handle_status`,
  `handle_stop`, `handle_read`, `handle_send`, `handle_attach`, or
  `tmux_session_exists`.
- **Heartbeat file body vs mtime mismatch**: `handle_bridge_tool_hook`
  (line 1074) writes the current millisecond epoch into the heartbeat file
  body. `handle_status` (line 1120) reads the file's mtime, not the body.
  The body is currently informational but unused.
