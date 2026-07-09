# tmuxlet

`tmuxlet` runs coding CLIs inside tmux while exposing a small print-mode
programmatic interface.

```bash
tmuxlet -p "say ready"
tmuxlet -p --target claude --output-format json "say ready"
tmuxlet -p --target codex --cwd /tmp "say ready"
printf "say ready" | tmuxlet -p -
```

## Install

Requirements:

- [Rust](https://www.rust-lang.org/tools/install) and
  [Cargo](https://doc.rust-lang.org/cargo/getting-started/installation.html)
- [`tmux`](https://github.com/tmux/tmux/wiki/Installing) on `PATH`
- At least one supported target CLI, such as `claude` or `codex`

Rust's recommended installer is `rustup`, which installs Cargo with the Rust
toolchain. See the [rustup installation guide](https://rust-lang.github.io/rustup/installation/index.html)
for advanced install options.

Install from GitHub:

```bash
cargo install --git https://github.com/CodefiLabs/tmuxlet --force
tmuxlet --version
tmuxlet -p "say ready"
```

Install from a local checkout:

```bash
cargo install --path . --force
tmuxlet --version
```

During development, run without installing:

```bash
cargo run -- -p "say ready"
```

## Why

On June 15, 2026, [Anthropic split Claude subscriptions into two billing pools](https://support.claude.com/en/articles/15036540-use-the-claude-agent-sdk-with-your-claude-plan): interactive Claude Code keeps drawing from your regular subscription usage limits, but `claude -p` and the Claude Agent SDK now drain from a separate monthly Agent SDK credit ($20 on Pro, $100 on Max 5x, $200 on Max 20x).

In practice, the regular subscription bucket is worth many times the Agent SDK credit in equivalent API-rate tokens. A $200 Max plan that comfortably runs Claude Code interactively all day under the subscription limit caps out far sooner under the $200 Agent SDK credit.

`tmuxlet` runs the target CLI in its normal interactive mode inside tmux, pastes the prompt as if a user typed it, and waits for a structured completion signal. From Claude's perspective it's a normal interactive session. From your script's perspective it's a `claude -p`-style blocking call returning text or JSON.

The same wrapper works against Codex, Gemini, opencode, pi, Cursor, and Grok — one normalized print-mode interface across seven coding CLIs. Useful for OpenClaw, Paperclip, scheduled scripts, and any other local automation that wants a blocking `-p` call against the user's local, official CLI without extracting OAuth tokens.

## Why not just `claude -p`?

Five reasons, in roughly the order most people care:

**1. Billing pool.** After June 15, 2026, `claude -p` and Agent SDK calls drain from the separate Agent SDK credit ($20 / $100 / $200 by plan). `claude` interactive draws from the regular subscription usage limits, which on a Max plan are worth substantially more than that credit in equivalent API spend. `tmuxlet` runs Claude in interactive mode and drives it from the outside, so programmatic workflows can use the larger subscription bucket. ([Anthropic's announcement](https://support.claude.com/en/articles/15036540-use-the-claude-agent-sdk-with-your-claude-plan).)

**2. Multi-CLI normalization.** `claude -p` is Claude-only. `tmuxlet` wraps Claude, Codex, Gemini, opencode, pi, Cursor, and Grok with one normalized flag set (`--continue`, `--resume`, `--session-id`, `--cwd`, `--model`, `--dangerously-skip-permissions`). Unsupported combinations fail fast instead of being silently dropped.

**3. Reliable completion signal.** Parsing stdout from `claude -p` is fragile when the target streams partial output or hits a confirmation prompt. `tmuxlet` uses an explicit completion contract — the target writes `answer.txt` and calls `tmuxlet bridge complete`. You get a clean `text` / `json` payload, or a structured `blocked` / `timeout` / `exited` status with a `pane.log` for debugging.

**4. Full terminal context.** tmux gives the target a real TTY, real environment, real MCP server config, real allowed-tools settings, and real auth state. Some CLIs behave differently when they detect they're not interactive — running them in tmux removes the ambiguity.

**5. Permission + confirmation handling.** By default `tmuxlet` grants the target scoped auto-approval for the completion-contract operations, so headless runs don't hang on their own write/bridge step (see [Permissions](#permissions)). It also watches the pane for startup confirmation gates during the startup window and sends Enter up to three times. If a run still stalls on a permission prompt outside the grant, you get a `blocked` status with the captured pane content instead of an indefinite hang.

**Caveat:** reason #1 depends on Anthropic continuing to price interactive Claude Code and `claude -p` from separate pools. If they unify those buckets, that argument goes away — but the other four still hold, and the multi-CLI piece becomes the main reason.

## Origin

This started from the mistaken assumption that `claude -p` could not be used as
the local execution bridge for OpenClaw-style workflows. That assumption came
during the early-2026 panic around using Claude subscription OAuth tokens in
third-party harnesses: [community timelines](https://metricnexus.ai/blog/is-openclaw-allowed-in-claude-code)
point to January/February 2026 token blocks and ToS clarification, and by
[April 4, 2026](https://www.techradar.com/pro/bad-news-claude-users-anthropic-says-youll-need-to-pay-to-use-openclaw-now)
Anthropic had said Claude subscriptions would no longer cover third-party
harnesses such as OpenClaw without separate usage billing. TechCrunch also
[reported on April 10, 2026](https://techcrunch.com/2026/04/10/anthropic-temporarily-banned-openclaws-creator-from-accessing-claude/)
that OpenClaw's creator was temporarily suspended, while noting Anthropic said
it had not banned people simply for using OpenClaw.

The useful distinction was: do not extract or reuse subscription OAuth inside a
third-party service; instead, drive the user's local, official CLI in tmux. The
first experiment was [`CodefiLabs/tq`](https://github.com/CodefiLabs/tq), short
for "tmux queue." `tmuxlet` is a narrower print-mode CLI built from those
lessons, not a deprecation notice for `tq`.

On May 14, 2026, Anthropic [reversed the April policy](https://support.claude.com/en/articles/15036540-use-the-claude-agent-sdk-with-your-claude-plan) — third-party agent harnesses are now officially supported with the new Agent SDK credit starting June 15. `tmuxlet` is the open-source bridge that works either way: against the regular subscription pool via interactive tmux drive (this README's main path), or against the Agent SDK credit if you prefer to use `claude -p` directly. Same harness, your billing call.

## Targets

Initial adapters:

- `claude` (default)
- `gemini`
- `codex`
- `opencode`
- `pi`
- `cursor` / `cursor-agent`
- `grok` (xAI Grok Build CLI)

Unsupported normalized flags fail fast instead of being ignored. Use
`--target-arg <arg>` for rare target-specific escape hatches.

## CLI

```bash
tmuxlet -p [options] [prompt]
tmuxlet status
tmuxlet read <id> [--lines N]
tmuxlet send <id> <message>
tmuxlet send-all [message] [--interval <secs>] [--enter]
tmuxlet attach <id>
tmuxlet stop <id>
tmuxlet reap [--blocked] [--older-than <secs>] [--dry-run] [-y]
tmuxlet prune [<id>...] [-y] [--dry-run]
```

`tmuxlet prune` deletes terminal runs (`completed`, `timeout`, `blocked`,
`exited`) and any leftover tmux sessions they own. With no ids it sweeps every
terminal run; with explicit ids it refuses non-terminal ones. Use `--dry-run`
to preview and `-y` / `--yes` to skip the confirmation prompt.

`tmuxlet reap` kills stuck/leaked live sessions so their concurrency slot frees
up (see [Concurrency & Throttling](#concurrency--throttling)). With no flags it
is conservative — only sessions sitting at a confirmation/permission prompt
**and** older than the 60s grace window. `--blocked` reaps any session at such a
prompt regardless of age; `--older-than <secs>` reaps by run-dir age. Like
`prune`, it lists candidates and asks before killing; pass `-y` for cron,
`--dry-run` to preview. It only kills sessions — `prune` cleans the dirs.

`tmuxlet send-all` walks every live tmuxlet session and delivers a message (or
just Enter, with `--enter` or an empty message) with a pause between sessions
(`--interval`, default 1s) — the safe way to nudge many sessions at once instead
of a tight loop firing keystrokes at every session simultaneously.

Important options:

- `-p, --print`: blocking programmatic mode
- `--target <name>`: target CLI, default `claude`
- `--output-format text|json`: output format
- `-C, --cwd <dir>`: working directory; aliases `--cd`, `--dir`
- `--model <model>`: target model where supported
- `-c, --continue`: continue the latest session where supported
- `-r, --resume [id]`: resume a session by id, or latest/picker where supported
- `--session-id <id>`: explicit normalized session id selector
- `--permission-mode <mode>`: Claude permission mode (`default`, `acceptEdits`,
  `plan`, `auto`, `dontAsk`, `bypassPermissions`); defaults to `auto`. Claude
  only — see [Permissions](#permissions)
- `--dangerously-skip-permissions`: normalized full-bypass flag; aliases
  `--yolo`, `--force`, `--dangerously-bypass-approvals-and-sandbox`
- `--timeout <seconds>`: print-mode timeout, default 1800
- `--max-concurrency <n>`: machine-wide live-session cap; `0` disables. Env
  `TMUXLET_MAX_CONCURRENCY`. Default 24 (a runaway guardrail) — see
  [Concurrency & Throttling](#concurrency--throttling)
- `--memory-pressure-guard`: queue launches while the host is low on free
  memory (opt-in; env `TMUXLET_MEMORY_GUARD`)

Non-print prompt launches are intentionally unsupported; pass `-p` for runs.

Session controls are mutually exclusive. Use one of `--continue`, `--resume`,
`--session`, or `--session-id` per launch.

Examples:

```bash
tmuxlet -p --target claude --continue "summarize the last task"
tmuxlet -p --target codex --session-id 01984d2f-... "continue the fix"
tmuxlet -p --target opencode --resume ses_abc123 "check status"
```

## Resuming Target Sessions

`tmuxlet` creates a fresh tmux run for each print-mode invocation, but it can
ask the target CLI inside that run to continue one of the target's own prior
conversation sessions. The normalized controls are:

- `--continue`: continue the latest session where the target supports that
  concept.
- `--resume`: resume the latest session or open the target's picker where the
  target supports a value-less resume mode.
- `--resume <id>`: resume a specific target session id.
- `--session <id>`: pass a target-native session selector for CLIs that expose
  `--session`.
- `--session-id <id>`: pass an explicit normalized session id; tmuxlet maps it
  to the target's native resume/session flag.

These flags select the target CLI conversation, not the `tmuxlet` run id under
`~/.tmuxlet/runs/<run-id>/`. They are mutually exclusive, so use only one per
launch.

Target mappings:

| Target | Latest session | Explicit session id | Notes |
| --- | --- | --- | --- |
| `claude` | `--continue` or `--resume` | `--resume <id>` or `--session-id <id>` | `--session` is not supported for Claude. |
| `gemini` | `--continue` or value-less `--resume` maps to `--resume latest` | `--resume <id>` | `--session` and `--session-id` are not supported for Gemini. |
| `codex` | `--continue` or value-less `--resume` maps to `codex resume --last` | `--resume <id>` or `--session-id <id>` maps to `codex resume <id>` | `--session` is not supported for Codex. |
| `opencode` | `--continue` | `--resume <id>`, `--session <id>`, or `--session-id <id>` maps to `--session <id>` | Value-less `--resume` is rejected; use `--continue` for latest. |
| `pi` | `--continue` or value-less `--resume` | `--resume <id>`, `--session <id>`, or `--session-id <id>` | `--session-id` maps to `--session <id>`. |
| `cursor` / `cursor-agent` | `--continue` maps to `cursor-agent resume` | `--resume <id>` or `--session-id <id>` maps to `--resume <id>` | `--session` is not supported for Cursor. |
| `grok` | `--continue` or value-less `--resume` | `--resume <id>` | `--session` is not supported. `--session-id` maps to grok's create-new UUID flag (does **not** resume; use `--resume` / `--continue`). |

When running through Cargo during development, Cargo's build/status lines are
separate from tmuxlet output:

```bash
cargo run -- -p "hello"
```

The model response is tmuxlet's stdout. Lines such as `Compiling` and `Running`
come from Cargo.

## How Print Mode Works

For Claude, Codex, and Grok, tmuxlet passes one full prompt in the launch command. The
target-visible prompt is ordered as:

```text
<user prompt>

TMUXLET COMPLETION CONTRACT:
...
```

The completion contract tells the target CLI to write its final response to
`answer.txt` and then run `tmuxlet bridge complete`. Other targets currently use
a paste fallback when tmuxlet cannot safely assume positional prompt support.

## Permissions

Print-mode runs auto-approve safe operations and explicitly grant the two
completion-contract operations — writing `answer.txt` and calling `tmuxlet bridge
complete` — so a headless run does not hang on a permission prompt for its own
completion step. Those files live under `~/.tmuxlet/runs/<run-id>/`, outside the
target's working directory, so the grant is what lets the target write there
without prompting. The classifier mode (`--permission-mode auto`, or the other
targets' equivalents) alone is not enough: `auto` is a background safety
classifier, not a blanket approval, and it only auto-approves in-cwd edits.

Each target gets the narrowest grant it supports:

| Target | Default grant |
| --- | --- |
| `claude` | `--permission-mode auto` (classifier) **plus** explicit allow-rules `Write(//<run-dir>/**)`, `Edit(//<run-dir>/**)`, `Bash(<tmuxlet> bridge complete:*)` + `--add-dir <run-dir>` — scoped |
| `codex` | `--ask-for-approval never --sandbox workspace-write` with the run dir carved in as a `writable_root` — scoped |
| `grok` | explicit Claude-compatible allow-rules `Write(//<run-dir>/**)`, `Edit(//<run-dir>/**)`, `Bash(<tmuxlet> bridge complete:*)` via `--allow` — scoped (sandbox off by default; no `--add-dir`) |
| `gemini` | `--yolo` (no graded mode; `auto_edit` does not cover shell) |
| `opencode` | `--dangerously-skip-permissions` (no confirmed headless allow-list flag) |
| `cursor` | `--force` (enables print-mode writes) |
| `pi` | none — auto-approves tool use by default |

Policy: if a target has an auto-classifier (only `claude`'s `auto`), use it **and**
still pin the exact contract operations with explicit allow-rules rather than
trusting the classifier; otherwise use that target's yolo/bypass. `claude`,
`codex`, and `grok` end up genuinely scoped to the contract; `gemini`, `opencode`, and
`cursor` fall back to their broad auto-approve because those CLIs offer no graded
permission tier. If pinning the contract ops under a classifier is ever not
possible or stops working, the bypass flag is the documented fallback.

Overrides:

- `--permission-mode <mode>` (Claude and Grok): set the mode explicitly. On Claude
  this overrides the `auto` default; on Grok it is passed through as
  `--permission-mode`.
- `TMUXLET_CLAUDE_PERMISSION_MODE`: change the Claude default mode globally
  without passing the flag each time.
- `--dangerously-skip-permissions` (aliases `--yolo`, `--force`,
  `--dangerously-bypass-approvals-and-sandbox`): full bypass. Takes precedence
  over and replaces the scoped grant. On Grok this maps to `--always-approve`.

## Concurrency & Throttling

Each `tmuxlet -p` run launches a full interactive coding-CLI TUI inside its own
tmux session, and those are heavy (hundreds of MB each). To stop an unthrottled
fan-out from exhausting the machine, tmuxlet enforces a **live-session cap** that
counts what is actually alive, not how many launches a caller has fired.

- **Live-session semaphore.** Before creating its tmux session, a run counts the
  live tmuxlet run sessions (`tmux list-sessions`, matched against the run-id
  grammar so a `tmuxlet-server` or a human's `tmuxlet-*` session never consumes a
  slot). At or above the cap it **queues** — polling, printing a one-line
  `waiting for a free slot (N/M live)` notice — until a slot frees or the acquire
  timeout elapses. The count is derived from tmux itself, so it is crash-safe:
  dead sessions vanish and the count self-clears with no persisted counter to
  reconcile. It is a *soft* cap (no machine-wide lock) — two truly simultaneous
  launches may briefly reach N+1, which self-corrects on the next acquire.
- **Default is a guardrail, not a throttle.** The default cap is **24** — high
  enough that normal interactive or modest parallel use never blocks, low enough
  to stop a runaway loop. A consumer that intentionally fans out should set a
  tighter value (e.g. `TMUXLET_MAX_CONCURRENCY=4`). Set `0` to disable entirely
  and restore the original unbounded behavior. A single run pays at most one
  cheap `tmux list-sessions` call; with the cap disabled there is zero overhead.
- **Self-healing.** If a run waits past the grace window while saturated, it runs
  one conservative reap pass (kills only sessions stuck at a confirmation prompt)
  so a fully-leaked machine recovers without manual intervention. At the acquire
  timeout it exits non-zero with `at capacity …; run \`tmuxlet reap\`` rather than
  hanging forever.
- **Memory backpressure (opt-in).** `--memory-pressure-guard` /
  `TMUXLET_MEMORY_GUARD` queues launches while host free memory is below
  `TMUXLET_MEMORY_MIN_FREE` percent (default 15). Off by default; on platforms
  whose memory probe is unavailable it degrades to "allow".
- **No keystroke herds.** Use `tmuxlet send-all` to drive many sessions at once;
  it paces between sessions instead of firing at all of them simultaneously.

| Knob | Default | Meaning |
| --- | --- | --- |
| `--max-concurrency <n>` / `TMUXLET_MAX_CONCURRENCY` | `24` | Live-session cap; `0` = unlimited |
| `TMUXLET_ACQUIRE_TIMEOUT` | `300` | Seconds to wait for a slot; `0` = wait forever |
| `--memory-pressure-guard` / `TMUXLET_MEMORY_GUARD` | off | Queue under host memory pressure |
| `TMUXLET_MEMORY_MIN_FREE` | `15` | Free-memory % threshold for the guard |

These live in tmuxlet itself so the cap holds regardless of how many callers or
loops fan out. Lowering a consumer's own concurrency and adding an external
circuit breaker are complementary, not substitutes.

## Runtime State

Runtime files live in:

```text
~/.tmuxlet/runs/<run-id>/
  meta.json
  prompt.txt
  answer.txt
  complete.txt
  pane.log
  error.txt
```

Set `TMUXLET_HOME` to override the state directory.

A successful print-mode run auto-kills its tmux session before returning, but
the run directory itself is left on disk so callers can inspect `complete.txt`,
`pane.log`, and the rest. Runs that end in `timeout` or `blocked` leave the
tmux session alive too, so you can `attach` and investigate. `tmuxlet prune` is
how those run directories and any leftover sessions get cleaned up.

## Confirmation Handling

For Claude and Codex, tmuxlet passes the full prompt in the launch command with
the user's prompt first and tmuxlet's completion contract after it. By default
tmuxlet grants scoped permissions for the contract operations (see
[Permissions](#permissions)), so the completion step itself does not prompt.
During the startup window, tmuxlet also watches for common confirmation gates and
sends `Enter` up to three times. This startup check runs during normal print-mode
polling, so successful runs can return as soon as the completion file appears.

If a run stalls later on a permission or confirmation prompt outside the default
grant, tmuxlet returns a nonzero `blocked` status with the captured pane output.
Rerun with `--dangerously-skip-permissions` for a full bypass where supported, or
inspect the run with `tmuxlet attach <id>`.

## Output And Failures

Text output prints only the target's final response when the completion contract
is satisfied. JSON output includes the run id, target, status, output, cwd, tmux
session, completion source, and elapsed time.

Nonzero statuses:

- `blocked`: the pane stopped changing and appears to be waiting on a
  permission or confirmation prompt.
- `timeout`: the target did not satisfy the completion contract before
  `--timeout`.
- `exited`: the tmux session ended before completion.

On `blocked` and `timeout`, tmuxlet writes `pane.log` and `error.txt` under the
run directory for debugging.

A typical cleanup flow:

```bash
tmuxlet -p "say ready"          # auto-kills tmux session on success
tmuxlet --timeout 5 -p "..."    # times out — tmux session stays alive
tmuxlet attach <id>             # investigate
tmuxlet prune                   # remove terminal runs + leftover sessions
```

## Development

```bash
cargo test
cargo build
```
