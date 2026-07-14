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
| `gemini` | `--continue` or value-less `--resume` (maps to `--resume latest`) | `--resume <id>` | `--session`, `--session-id` |
| `codex` | `--continue` or value-less `--resume` (maps to `codex resume --last`) | `--resume <id>` or `--session-id <id>` (maps to `codex resume <id>`) | `--session` |
| `opencode` | `--continue` only | `--resume <id>`, `--session <id>`, or `--session-id <id>` (all map to `--session <id>`) | value-less `--resume` (rejected) |
| `pi` | `--continue` or value-less `--resume` | `--resume <id>`, `--session <id>`, or `--session-id <id>` | — |
| `cursor` / `cursor-agent` | `--continue` (maps to `cursor-agent resume`) | `--resume <id>` or `--session-id <id>` (maps to `--resume <id>`) | `--session` |
| `grok` | `--continue` or value-less `--resume` | `--resume <id>` | `--session`; `--session-id` does NOT resume (see quirks) |
| `hermes` / `hermes-agent` | `--continue` or value-less `--resume` | `--resume <id>` | `--session`; `--session-id` maps to `--resume <id>` (see quirks) |

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
| `gemini` | `--yolo` (no graded mode available) | no — broad |
| `opencode` | `--dangerously-skip-permissions` | no — broad |
| `cursor` | `--force` | no — broad |
| `hermes` | `--yolo` (bypasses dangerous-command approval; no scoped tier) | no — broad |
| `pi` | none needed — auto-approves tool use by default | n/a |

Implication: on `claude`/`codex`/`grok`, work outside the cwd and run dir can
still hit permission prompts (→ `blocked` status). On `gemini`/`opencode`/
`cursor`/`hermes`, runs are effectively unrestricted already.

## Permission overrides

- `--permission-mode <mode>` (claude, grok): `default`, `acceptEdits`, `plan`,
  `auto`, `dontAsk`, `bypassPermissions`. Overrides claude's `auto` default.
- `TMUXLET_CLAUDE_PERMISSION_MODE`: change the claude default globally.
- `--allowed-tools` / `--disallowed-tools` (repeatable): allow/deny rule
  pass-through where supported.
- `--dangerously-skip-permissions` (aliases `--yolo`, `--force`,
  `--dangerously-bypass-approvals-and-sandbox`): full bypass, replaces the
  scoped grant. On grok maps to `--always-approve`; on hermes to `--yolo`.

## Target quirks

- **grok `--session-id <id>`** passes grok's create-new-UUID flag — it creates
  a session with that id rather than resuming one. To resume, use `--resume
  <id>` or `--continue`.
- **hermes takes no positional prompt.** Its headless one-shot prompt is a flag
  value (`hermes -z <prompt>` or `hermes chat -q <prompt>`), which tmuxlet does
  not use — tmuxlet drives hermes' line-oriented classic REPL (`--cli`) and types
  the prompt in. `--session-id <id>` maps to `--resume <id>` (hermes
  has no create-with-id flag); value-less `--resume` maps to `--continue`.
- **opencode** rejects value-less `--resume`; use `--continue` for latest.
- **Prompt delivery**: claude, codex, and grok receive the full prompt (user
  prompt first, tmuxlet's completion contract appended) in the launch command;
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
whatever `codex`/`gemini` accept natively).
