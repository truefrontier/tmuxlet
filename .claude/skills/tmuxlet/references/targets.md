# Per-target reference

Details that vary by `--target`: session resume mapping, default permission
grants, and quirks. tmuxlet fails fast on unsupported flag/target combinations
instead of silently dropping them.

## Contents

- [Session resume mapping](#session-resume-mapping)
- [Default permission grants](#default-permission-grants)
- [Permission overrides](#permission-overrides)
- [Target quirks](#target-quirks)
- [Model flag support](#model-flag-support)

## Session resume mapping

Session flags select the **target CLI's own conversation**, not the tmuxlet run
id. Only one of `--continue` / `--resume` / `--session` / `--session-id` per
launch.

| Target | Latest session | Explicit session id | Not supported |
| --- | --- | --- | --- |
| `claude` | `--continue` or value-less `--resume` | `--resume <id>` or `--session-id <id>` | `--session` |
| `agy` / `gemini` | `--continue` or value-less `--resume` (both map to `--continue`) | `--resume <id>` or `--session-id <id>` (map to `--conversation <id>`) | `--session` |
| `codex` | `--continue` or value-less `--resume` (maps to `codex resume --last`) | `--resume <id>` or `--session-id <id>` (maps to `codex resume <id>`) | `--session` |
| `opencode` | `--continue` only | `--resume <id>`, `--session <id>`, or `--session-id <id>` (all map to `--session <id>`) | value-less `--resume` (rejected) |
| `pi` | `--continue` or value-less `--resume` | `--resume <id>`, `--session <id>`, or `--session-id <id>` | — |
| `cursor` / `cursor-agent` | `--continue` (maps to `cursor-agent resume`) | `--resume <id>` or `--session-id <id>` (maps to `--resume <id>`) | `--session` |
| `grok` | `--continue` or value-less `--resume` | `--resume <id>` | `--session`; `--session-id` does NOT resume (see quirks) |
| `hermes` / `hermes-agent` | `--continue` or value-less `--resume` | `--resume <id>` | `--session`; `--session-id` maps to `--resume <id>` (see quirks) |
| `muse` / `musecli` | `--continue` or value-less `--resume` (map to `-c`) | `--resume <id>`, `--session <id>`, or `--session-id <id>` (map to `--chat <id>`; `main` = main chat) | — |
| `gweb` / `gemini-web` | `--continue` or value-less `--resume` (map to `-c`) | `--resume <id>`, `--session <id>`, or `--session-id <id>` (map to `--chat <id>`) | — |

## Default permission grants

Print-mode runs auto-grant the two completion-contract operations (write
`answer.txt` under the run dir + run `tmuxlet bridge complete`) so headless runs
don't hang on their own completion step. Each target gets the narrowest grant
it supports:

| Target | Default grant | Scoped? |
| --- | --- | --- |
| `claude` | `--permission-mode auto` + allow-rules `Write(//<run-dir>/**)`, `Edit(//<run-dir>/**)`, `Bash(<tmuxlet> bridge complete:*)` + `--add-dir <run-dir>` | yes |
| `codex` | `--ask-for-approval never --sandbox workspace-write` with run dir as `writable_root` | yes |
| `grok` | Claude-compatible allow-rules via `--allow` (sandbox off; no `--add-dir`) | yes |
| `agy` | `--dangerously-skip-permissions` (no graded mode available) | no — broad |
| `opencode` | `--auto` | no — broad |
| `cursor` | `--force` | no — broad |
| `hermes` | `--yolo` (bypasses dangerous-command approval; no scoped tier) | no — broad |
| `pi` | none needed — auto-approves tool use by default | n/a |
| `muse` / `gweb` | none — chat backends run no tools and no completion contract | n/a |

Implication: on `claude`/`codex`/`grok`, work outside the cwd and run dir can
still hit permission prompts (→ `blocked` status). On `agy`/`opencode`/
`cursor`/`hermes`, runs are effectively unrestricted already.

## Permission overrides

- `--permission-mode <mode>` (claude, grok): `default`, `acceptEdits`, `plan`,
  `auto`, `dontAsk`, `bypassPermissions`. Overrides claude's `auto` default.
- `TMUXLET_CLAUDE_PERMISSION_MODE`: change the claude default globally.
- `--allowed-tools` / `--disallowed-tools` (repeatable): allow/deny rule
  pass-through where supported.
- `--dangerously-skip-permissions` (aliases `--yolo`, `--force`,
  `--dangerously-bypass-approvals-and-sandbox`): full bypass, replaces the
  scoped grant. On grok maps to `--always-approve`; on hermes to `--yolo`; on agy
  to `--dangerously-skip-permissions`.

## Target quirks

- **grok `--session-id <id>`** passes grok's create-new-UUID flag — it creates
  a session with that id rather than resuming one. To resume, use `--resume
  <id>` or `--continue`.
- **hermes takes no positional prompt.** Its headless one-shot prompt is a flag
  value (`hermes -z <prompt>` or `hermes chat -q <prompt>`), which tmuxlet does
  not use — tmuxlet drives hermes' line-oriented classic REPL (`--cli`) and types
  the prompt in. `--session-id <id>` maps to `--resume <id>` (hermes
  has no create-with-id flag); value-less `--resume` maps to `--continue`.
- **agy (Antigravity CLI)** replaces the deprecated `gemini` CLI; `--target
  gemini` is kept as an alias that now drives the `agy` binary. agy shows a
  first-run "trust this folder?" gate (cleared by the startup auto-enter), so the
  prompt rides in via `--prompt-interactive` rather than a post-startup paste
  (which the gate would consume). `--resume <id>` and `--session-id <id>` both
  map to `--conversation <id>`; value-less `--resume`/`--continue` map to
  `--continue`.
- **opencode** rejects value-less `--resume`; use `--continue` for latest.
- **muse / gweb are chat backends**, not coding agents: Meta Muse via `musecli`
  and Gemini web via `gweb` (override with `TMUXLET_MUSECLI_BIN` /
  `TMUXLET_GWEB_BIN`). They get the raw prompt (no completion contract) via
  `--prompt-file`, and a `sh` wrapper calls `bridge complete` itself. On failure
  (not signed in, profile lock busy, CLI timeout) the run ends `exited` and the
  output is the CLI's error message. Agent-only flags (system prompt, tools,
  permissions, `--add-dir`, `--mcp-config`, `--settings`, bypass) fail fast.
  Runs on the same backend queue on that CLI's Chrome profile lock; tmuxlet
  passes `--lock-wait <timeout>`. An explicit `--timeout` is also forwarded to
  the CLI. No startup auto-Enter or `blocked` detection.
- **Prompt delivery**: claude, codex, and grok receive the full prompt (user
  prompt first, tmuxlet's completion contract appended) as a positional launch
  argument; agy receives it via `--prompt-interactive` in the launch command;
  other targets get it pasted into the TUI after startup.
- **Startup confirmation gates** (theme pickers, trust prompts, etc.): tmuxlet
  watches the pane during the startup window and presses Enter up to three
  times automatically. Later prompts outside the grant produce `blocked`.
- **`--output-format stream-json` is not supported** for any target — only
  `text` and `json`.

## Model flag support

`-m/--model` is passed through where the target supports it; unsupported
combinations fail fast with an error naming the flag, so just try it and read
the error. Model names are target-native (e.g. `sonnet`/`opus` for claude,
whatever `codex`/`agy` accept natively). `gweb` takes a Gemini model menu name
(e.g. `-m "3.8 Flash"`); `muse` rejects `-m` (Muse has no model picker).
`--thinking low|medium|high` (or `l`/`m`/`h`) sets Gemini's Thinking level and
is `gweb`-only; every other target rejects it.
