# tmuxlet

`tmuxlet` runs coding CLIs inside tmux while exposing a small print-mode
programmatic interface.

```bash
tmuxlet -p "say ready"
tmuxlet -p --target claude --output-format json "say ready"
tmuxlet -p --target codex --cwd ~/Sites/app "say ready"
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

The same wrapper works against Codex, Antigravity (agy), opencode, pi, Cursor, Grok, and Hermes — one normalized print-mode interface across eight coding CLIs. Useful for OpenClaw, Paperclip, scheduled scripts, and any other local automation that wants a blocking `-p` call against the user's local, official CLI without extracting OAuth tokens.

## Why not just `claude -p`?

Five reasons, in roughly the order most people care:

**1. Billing pool.** After June 15, 2026, `claude -p` and Agent SDK calls drain from the separate Agent SDK credit ($20 / $100 / $200 by plan). `claude` interactive draws from the regular subscription usage limits, which on a Max plan are worth substantially more than that credit in equivalent API spend. `tmuxlet` runs Claude in interactive mode and drives it from the outside, so programmatic workflows can use the larger subscription bucket. ([Anthropic's announcement](https://support.claude.com/en/articles/15036540-use-the-claude-agent-sdk-with-your-claude-plan).)

**2. Multi-CLI normalization.** `claude -p` is Claude-only. `tmuxlet` wraps Claude, Codex, Antigravity (agy), opencode, pi, Cursor, Grok, and Hermes with one normalized flag set (`--continue`, `--resume`, `--session-id`, `--cwd`, `--model`, `--dangerously-skip-permissions`). Unsupported combinations fail fast instead of being silently dropped.

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
- `agy` / `gemini` (Google Antigravity CLI; `gemini` is a deprecated alias)
- `codex`
- `opencode`
- `pi`
- `cursor` / `cursor-agent`
- `grok` (xAI Grok Build CLI)
- `hermes` / `hermes-agent` (Nous Research Hermes Agent CLI)
- `muse` / `musecli` / `llmcli:muse` (Meta Muse via [llmcli](https://github.com/truefrontier/webcli) `llmcli muse`; a chat backend, see [Chat backends](#chat-backends-muse-gweb-chatgpt))
- `gweb` / `gemini-web` / `llmcli:gemini` (Gemini on the web via `llmcli gemini`; a chat backend)
- `chatgpt` / `cgpt` / `llmcli:chatgpt` (ChatGPT on the web via `llmcli chatgpt`; a chat backend)

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
tmuxlet reap [--blocked] [--older-than <secs>] [--dry-run] [-y|--yes]
tmuxlet prune [<id>...] [-y|--yes] [--dry-run]
tmuxlet bridge complete --run <id> --file <path>
tmuxlet -h | --help
tmuxlet -v | --version
```

Non-print prompt launches are intentionally unsupported; pass `-p` / `--print`
for runs. Prompt text may be positional args, `-` (read stdin), or empty stdin
when no prompt args are given. `--` ends option parsing so the rest is prompt.

### Session ownership (0.8.0)

Print-mode runs record `owner` and `startedAt` (Unix epoch seconds) in `meta.json`.
Identity resolves from `--owner <name>`, then `TMUXLET_OWNER`, then
`<parent-process-name>@<caller-cwd>`. A print-mode `--cwd` does not change the
caller identity. Set an explicit owner for automation that launches from different
parent processes or directories. `tmuxlet status` includes the recorded owner.

`send`, `send-all`, `stop`, `prune`, and manual `reap` accept `--owner <name>`
and `--force-foreign`. A foreign run is refused with its id and owner; `send-all`
and `prune` instead skip foreign runs and report the count. Manual `reap` refuses
the pass if any selected runs are foreign. `--force-foreign` explicitly overrides
these checks. Legacy runs without an owner remain accessible with a warning
and appear as `unknown owner` in status. `read` and `attach` remain read/interactive
inspection commands without an ownership restriction.

```bash
tmuxlet -p --owner nightly "Run the checks"
tmuxlet send <id> --owner nightly "Continue"
tmuxlet send-all --owner nightly --enter
tmuxlet stop <id> --owner maintenance --force-foreign
tmuxlet send <id> --owner nightly -- "literal --owner text"
```

### Print-mode options

Every flag accepted by `parse_options` (print mode). Unsupported combinations
for a given `--target` fail fast instead of being silently dropped. See
[Resuming Target Sessions](#resuming-target-sessions) and
[Permissions](#permissions) for target-specific mapping.

| Flag / aliases | Value | Default | Purpose |
| --- | --- | --- | --- |
| `-p`, `--print` | flag | off | Blocking programmatic mode (required for runs). |
| `--target` | `<name>` | `claude` | Target CLI: `claude`, `agy` / `gemini`, `codex`, `opencode`, `pi`, `cursor` / `cursor-agent`, `grok`, `hermes` / `hermes-agent`, `muse` / `musecli` / `llmcli:muse`, `gweb` / `gemini-web` / `llmcli:gemini`, `chatgpt` / `cgpt` / `llmcli:chatgpt`. |
| `--output-format` | `text` \| `json` | `text` | Shape of tmuxlet’s result. `stream-json` is not supported. |
| `-C`, `--cwd`, `--cd`, `--dir` | `<dir>` | process cwd | Working directory for the run (`~` expanded). |
| `-m`, `--model` | `<model>` | unset | Model pass-through where the target supports it. |
| `--thinking` | `<level>` | unset | Gemini's Thinking level for `gweb`: `low`, `medium`, `high` (or `l`, `m`, `h`). Other targets reject it. |
| `--image-out` | `<dir\|path>` | unset | `gweb` and `chatgpt`: save the reply's images there (the CLI's `--image-out`; relative paths are taken from `--cwd`). The JSON result gets `images`. Other targets reject it. See [Chat backends](#chat-backends-muse-gweb-chatgpt). |
| `--canvas` | flag | off | `gweb` only: turn on Gemini's Canvas tool. The JSON result gets `canvas`. Other targets reject it. |
| `--canvas-out` | `<path>` | unset | `gweb` only: also write the Canvas doc's markdown to this path (relative paths are taken from `--cwd`; implies `--canvas`). Other targets reject it. |
| `--file`, `--attach` | `<path>` (repeatable) | unset | Attach a file with the target's own mechanism, or inline it if it's text. Relative paths are taken from `--cwd`. Exit 7 if the target can't take it. See [Attachments](#attachments---file). |
| `-c`, `--continue` | flag | off | Continue the latest target session where supported. Mutually exclusive with `--resume` / `--session` / `--session-id`. |
| `-r`, `--resume` | optional `[id]` | unset | Resume latest/picker (value-less) or a specific session id where supported. |
| `--session` | `<id>` | unset | Target-native session selector where supported (e.g. opencode, pi). |
| `--session-id` | `<id>` | unset | Explicit normalized session id; mapping is target-specific (on `grok`, create-new UUID only — not resume). |
| `--system-prompt` | `<text>` | unset | System prompt pass-through where supported. |
| `--append-system-prompt` | `<text>` (repeatable) | unset | Append system-prompt text where supported. |
| `--add-dir`, `--include-directories` | `<path>` or CSV (repeatable) | unset | Extra directories the target may access where supported. |
| `--dangerously-skip-permissions`, `--yolo`, `--force`, `--dangerously-bypass-approvals-and-sandbox` | flag | off | Full permission bypass; replaces the scoped completion-contract grant. Mapping is target-specific. |
| `--permission-mode` | `<mode>` | unset (Claude builder defaults to `auto`) | Permission mode pass-through where supported (`default`, `acceptEdits`, `plan`, `auto`, `dontAsk`, `bypassPermissions` on Claude/Grok). |
| `--allowed-tools`, `--allowedTools` | `<rule>` (repeatable) | unset | Allow-list / allow-rule pass-through where supported. |
| `--disallowed-tools`, `--disallowedTools` | `<rule>` (repeatable) | unset | Deny-list / deny-rule pass-through where supported. |
| `--tools` | `<tools>` (repeatable) | unset | Tools allow-list pass-through where supported. |
| `--mcp-config` | `<path>` (repeatable) | unset | MCP config path pass-through where supported. |
| `--settings` | `<path>` | unset | Settings file pass-through where supported. |
| `--target-arg` | `<arg>` (repeatable) | unset | Raw escape hatch appended to the target launch command. |
| `--timeout` | `<seconds>` | `1800` | Max wait for the completion contract before `timeout` status. Chat backends also get it as the CLI's `--timeout` when given explicitly. |
| `--owner` | `<name>` | `TMUXLET_OWNER` or parent-process-name@caller-cwd | Record the run's owner. |
| `--reap-min-age` | `<minutes>` | `10` (or `TMUXLET_REAP_MIN_AGE_MIN`) | Minimum run age for automatic self-healing; non-negative integer. |
| `--max-concurrency` | `<n>` | `24` (or `TMUXLET_MAX_CONCURRENCY`) | Machine-wide live-session cap; `0` = unlimited. |
| `--memory-pressure-guard`, `--memory-guard` | flag | off | Queue launches under host memory pressure (also `TMUXLET_MEMORY_GUARD`). |

Session controls are mutually exclusive. Use only one of `--continue`,
`--resume`, `--session`, or `--session-id` per launch (and do not combine
`--continue` with any of the others).

Examples:

```bash
tmuxlet -p --target claude --continue "summarize the last task"
tmuxlet -p --target codex --session-id 01984d2f-... "continue the fix"
tmuxlet -p --target opencode --resume ses_abc123 "check status"
tmuxlet -p --system-prompt "be terse" --model sonnet "say ready"
printf "say ready" | tmuxlet -p -
```

### Subcommands

| Command | Parameters | Purpose |
| --- | --- | --- |
| `status` | (none) | Print `meta.json` for each run under the state dir (one JSON object per line when present). |
| `read <id>` | `--lines <N>` (default **80**) | Capture and print the live tmux pane for that run. |
| `send <id> <message>` | message required | Paste a message into the run’s pane (as if typed). |
| `send-all [message]` | `--interval <secs>` (default **1**), `--enter` | Deliver a message (or just Enter) to every live tmuxlet session, paced so sessions are not herded. Empty message implies Enter. |
| `attach <id>` | (none) | Attach to the run’s tmux session interactively. |
| `stop <id>` | (none) | Try a graceful `/exit`, then kill the session if it remains; prints `Stopped <id>`. |
| `reap` | `--blocked`, `--older-than <secs>`, `--dry-run`, `-y` / `--yes` | Kill stuck/leaked live sessions so concurrency slots free (see below). |
| `prune [<id>...]` | `--dry-run`, `-y` / `--yes` | Delete terminal run dirs and leftover sessions they own (see below). |
| `bridge complete` | `--run <id>`, `--file <path>` (both required) | Completion-contract surface: copy the answer file into the run’s `complete.txt` so a waiting `-p` call finishes. Invoked by the target CLI, not typically by hand. |

`tmuxlet prune` deletes terminal runs (`completed`, `timeout`, `blocked`,
`exited`) and any leftover tmux sessions they own. With no ids it sweeps every
terminal run; with explicit ids it refuses non-terminal ones. Use `--dry-run`
to preview and `-y` / `--yes` to skip the confirmation prompt.

`tmuxlet reap` kills stuck/leaked live sessions so their concurrency slot frees
up (see [Concurrency & Throttling](#concurrency--throttling)). With no flags it
is conservative — only sessions sitting at a confirmation/permission prompt
**and** older than the 60s grace window. `--blocked` reaps any session at such a
prompt regardless of age; `--older-than <secs>` reaps by age since run start.
When both filters are set they are ORed. Age uses `startedAt` or legacy
`meta.json` creation time, never directory modification time. Like
`prune`, it lists candidates and asks before killing; pass `-y` / `--yes` for
cron, `--dry-run` to preview. It only kills sessions — `prune` cleans the dirs.

`tmuxlet send-all` walks every live tmuxlet session and delivers a message (or
just Enter, with `--enter` or an empty message) with a pause between sessions
(`--interval`, default 1s) — the safe way to nudge many sessions at once instead
of a tight loop firing keystrokes at every session simultaneously.

### Environment variables

| Variable | Default | Effect |
| --- | --- | --- |
| `TMUXLET_OWNER` | parent-process-name@caller-cwd | Default identity for runs and management commands; `--owner` wins. |
| `TMUXLET_REAP_MIN_AGE_MIN` | `10` | Minimum run age in minutes for automatic reap; `--reap-min-age` wins. |
| `TMUXLET_HOME` | `~/.tmuxlet` | Root directory for runtime state (`runs/`, etc.). |
| `TMUXLET_MAX_CONCURRENCY` | `24` | Live-session cap when `--max-concurrency` is omitted; `0` = unlimited. |
| `TMUXLET_ACQUIRE_TIMEOUT` | `300` | Seconds to wait for a free concurrency slot; `0` = wait forever. |
| `TMUXLET_MEMORY_GUARD` | off | Truthy values enable the memory-pressure guard (same as `--memory-pressure-guard` / `--memory-guard`). |
| `TMUXLET_MEMORY_MIN_FREE` | `15` | Free-memory percent threshold for the memory guard. |
| `TMUXLET_CLAUDE_PERMISSION_MODE` | `auto` | Default Claude `--permission-mode` when neither `--permission-mode` nor full bypass is set. |
| `TMUXLET_LLMCLI_BIN` | `llmcli` | Program run for the chat backends `muse` / `gweb` / `chatgpt` (as `llmcli <site>`). |

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
| `agy` / `gemini` | `--continue` or value-less `--resume` map to `--continue` | `--resume <id>` or `--session-id <id>` map to `--conversation <id>` | `--session` is not supported. |
| `codex` | `--continue` or value-less `--resume` maps to `codex resume --last` | `--resume <id>` or `--session-id <id>` maps to `codex resume <id>` | `--session` is not supported for Codex. |
| `opencode` | `--continue` | `--resume <id>`, `--session <id>`, or `--session-id <id>` maps to `--session <id>` | Value-less `--resume` is rejected; use `--continue` for latest. |
| `pi` | `--continue` or value-less `--resume` | `--resume <id>`, `--session <id>`, or `--session-id <id>` | `--session-id` maps to `--session <id>`. |
| `cursor` / `cursor-agent` | `--continue` maps to `cursor-agent resume` | `--resume <id>` or `--session-id <id>` maps to `--resume <id>` | `--session` is not supported for Cursor. |
| `grok` | `--continue` or value-less `--resume` | `--resume <id>` | `--session` is not supported. `--session-id` maps to grok's create-new UUID flag (does **not** resume; use `--resume` / `--continue`). |
| `hermes` / `hermes-agent` | `--continue` or value-less `--resume` | `--resume <id>` | `--session` is not supported. `--session-id` maps to `--resume <id>` (resume that session; hermes has no create-with-id flag). |
| `muse` / `musecli` | `--continue` or value-less `--resume` map to `-c` (the CLI's last chat) | `--resume <id>`, `--session <id>`, or `--session-id <id>` map to `--chat <id>` | Chat ids are musecli chat ids; `main` is the main chat. |
| `gweb` / `gemini-web` | `--continue` or value-less `--resume` map to `-c` (the CLI's last chat) | `--resume <id>`, `--session <id>`, or `--session-id <id>` map to `--chat <id>` | Chat ids are Gemini chat ids. |
| `chatgpt` / `cgpt` | `--continue` or value-less `--resume` map to `-c` (the CLI's last chat) | `--resume <id>`, `--session <id>`, or `--session-id <id>` map to `--chat <id>` | Chat ids are the UUID in chatgpt.com/c/<id>. |

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
Chat backends (`muse`, `gweb`, `chatgpt`) never see the contract; see below.

## Attachments (`--file`)

`--file <path>` (alias `--attach`, repeatable) hands a file to the target. Each
path is resolved against `--cwd`, made absolute (symlinks resolved), and must be
a regular file. tmuxlet then sniffs its first 8 KiB: PNG, JPEG, GIF, and WebP
are images, `%PDF-` is a PDF, UTF-8 with no control characters (tab, newlines,
form feed, and escape are fine) is text, and anything else is binary. All of
this happens before a run directory or tmux session exists.

Each target gets the file through its own mechanism where it has one, checked
against the installed CLI's `--help` and docs (2026-10-18):

| Target | Text | Image | PDF | Other binary | Mechanism |
| --- | --- | --- | --- | --- | --- |
| `claude` | `@path` | `@path` | `@path` | refused | `@/abs/path` lines under "Attached files:" in the prompt. Claude Code reads `@` mentions into context, and its Read tool takes images and PDFs. There is no local-file flag: `--file` takes Files API ids. |
| `codex` | inlined | `--image` | refused | refused | `--image <abs path>` launch arguments, before the `--` that ends options, so the variadic `-i` can't swallow the prompt. No flag for other files. |
| `pi` | `@path` arg | `@path` arg | refused | refused | `@<abs path>` launch arguments: `pi [options] [@files...]` includes "a text file or image in the first prompt". Not live-checked: pi isn't installed here. |
| `hermes` | `@file:` | refused | refused | refused | `@file:/abs/path` lines in the REPL message, which Hermes expands. Its `--image` exists only on `hermes chat -q`, not the `--cli` REPL tmuxlet drives. |
| `gweb` | `--file` | `--file` | `--file` | `--file` | `--file <abs path>` per file. gweb enforces Gemini's own limits (10 files per prompt, 100 MB per file, 2 GB per video) and exits 2 past them, so the run ends `exited` with gweb's error. |
| `chatgpt` | `--file` | `--file` | `--file` | `--file` | `--file <abs path>` per file, through ChatGPT's own composer file input. cgpt checks ChatGPT's limits (512 MB per file, 20 MB per image) and exits 2 past them, so the run ends `exited` with cgpt's error. |
| `cursor` | inlined | refused | refused | refused | No file flag. |
| `agy` / `gemini` | inlined | refused | refused | refused | No file flag. Its `@` file picker is interactive, so the `--prompt-interactive` launch prompt gets the text inlined. |
| `opencode` | inlined | refused | refused | refused | `-f/--file` exists only on `opencode run`, not the TUI that tmuxlet drives. |
| `grok` | inlined | refused | refused | refused | No file flag. Its `@` operator opens an interactive fuzzy picker, so the launch prompt gets the text inlined. |
| `muse` | inlined | refused | refused | refused | musecli takes text only. |

Inlined files go after your prompt (and before tmuxlet's completion contract), one block each:

```text
<<<FILE path="/abs/notes.md" bytes=1234>>>
...the file's text...
<<<END FILE>>>
```

`bytes` is the file's exact size, so a reader can find the end even if the text
contains the closing marker. A newline is added before the marker when the file
doesn't end with one.

Limits and refusals:

- Inlined text is capped at **256 KiB per file** and **768 KiB in total** (raw
  bytes; `INLINE_FILE_MAX_BYTES`, `INLINE_TOTAL_MAX_BYTES`). The total stays
  under macOS's 1 MiB argument limit, because claude, codex, grok, cursor, and
  agy get the whole prompt as one launch argument. Files sent by mention or flag
  aren't capped by tmuxlet.
- `@`-mention paths may hold only letters, digits, and `-_./+,@%~=`. Other text
  files are inlined instead; other images and PDFs are refused.
- **Exit 7** (`EXIT_ATTACHMENT_REFUSED`) means a `--file` was refused: a file the
  target has no native way to take and that isn't text, or inlined text over the
  caps. The message names the file and the targets that can take it, for example
  `--file /abs/pic.png: cursor can't take image files: it has no native way to
  attach one, and only text is inlined (targets that take image files: claude,
  codex, pi, gweb, chatgpt)`. A missing path or a directory is exit 1, like other bad
  arguments.
- The launch line is typed into the pane's shell, sometimes before the shell
  is reading: the tty then keeps only 1024 bytes and drops the rest (macOS), and
  tmux refuses any command over about 16 KiB. So when the line would pass 1000
  bytes, tmuxlet writes it to `<run>/launch.sh` and types `. <run>/launch.sh`
  instead. The pane's own shell runs it, with your aliases and functions, and
  every argument arrives byte for byte. Many `--file` paths fit this way, and
  long prompts no longer fail with "command too long".

```bash
tmuxlet -p --target gweb --file README.md --file chart.png "Summarize both"
tmuxlet -p --target codex --file screenshot.png --file notes.md "Fix what the screenshot shows"
tmuxlet -p --target claude --file src/main.rs "Review this file"
```

## Chat backends (muse, gweb, chatgpt)

`muse` sends the prompt to Meta Muse, `gweb` to Gemini on the web, and
`chatgpt` to ChatGPT on the web, all through the shared `llmcli` binary
(`llmcli muse` / `llmcli gemini` / `llmcli chatgpt`). The `llmcli:<site>`
target names are aliases for the same backends. They return the answer exactly
like any other target:

```bash
tmuxlet -p --target muse "Reply with exactly: pong"
tmuxlet -p --target gweb -m "3.8 Flash" "Reply with exactly: pong"
tmuxlet -p --target gweb -m pro --thinking high "Prove it step by step"
tmuxlet -p --target gweb --resume <chatId> "and again"
tmuxlet -p --target muse --continue "follow-up"
tmuxlet -p --target gweb -m Argon --image-out ./out --output-format json "a seamless tileable pattern of small blue stars on white"
tmuxlet -p --target chatgpt -m gpt-6-thinking "Reply with exactly: pong"
TMUXLET_OWNER=Press tmuxlet -p --target chatgpt --image-out ./out --output-format json --timeout 900 "Create a square image: seamless tileable pattern of small blue stars on white"
```

These are chat CLIs, not coding agents, so they cannot follow the completion
contract. tmuxlet completes the run itself:

- The raw prompt goes to the CLI through `<run>/prompt.txt`. No contract text is
  added.
- The pane runs `exec sh -c '<cli> --lock-wait <timeout> -p --quiet [flags]
  --prompt-file <run>/prompt.txt --output-file <run>/answer.txt 2> <run>/stderr.txt
  && tmuxlet bridge complete ... || { c=$?; cp <run>/stderr.txt <run>/pane.log; printf "%s\n" "$c" > <run>/exit-code.txt; exit "$c"; }'`.
- On success the run is `completed` and the output is the answer.
- On failure (not signed in, profile lock busy, the CLI's own timeout, or Gemini
  answering with its own error message, which `gweb` exits 5 on) the pane
  exits, the run ends `exited`, and the output is the CLI's error message.
- For `gweb` / `gemini-web` and `chatgpt` / `cgpt`, the CLI's exit **6**
  becomes tmuxlet exit **6**, with status `rate_limited`. The wrapper forces
  `GWEB_FORMAT=json` / `CGPT_FORMAT=json` for reliable error capture. stderr contains `{"error":"rate_limited","code":6,"message":<original
  notice>,"resets_at":<ISO 8601 timestamp or null>}`. `--output-format json` adds
  the same four fields to the normal run object on stdout. Text mode emits the
  error on stderr and no answer on stdout. Reset timestamps retain gweb's
  `America/Chicago` offset. tmuxlet does not retry the prompt. Other backend
  failures retain status `exited` and tmuxlet exit 1 (including gweb and cgpt
  exit 5). cgpt `--strict-model` (via `--target-arg`) also exits 6 when ChatGPT
  answers with another model; that run is `rate_limited` too, and its error stays
  `{"error":"model_fallback","code":6,"message":…,"requestedModel":…,"servedModel":…}`.
- The startup auto-Enter and `blocked` detection are skipped.
- `gweb --image-out <dir|path>` (needs gweb 0.3.0) turns on Gemini's Create image
  tool and saves the reply's images at full size. tmuxlet forwards it as an
  absolute path and also passes `--output-format json`, so `answer.txt` holds
  gweb's JSON result. The run's output is gweb's text form, the answer and then
  one saved path per line, and `--output-format json` adds gweb's `images` list:

  ```json
  {"id":"…","target":"gweb","status":"completed","output":"/abs/out/a-seamless-tileable-pattern-of-small-blue-stars-1.png\n",
   "…":"…","images":[{"path":"/abs/out/a-seamless-tileable-pattern-of-small-blue-stars-1.png","width":2752,"height":1536,
   "bytes":3098113,"mime":"image/png","source_url_kind":"full_size"}]}
  ```

  A reply without an image is a gweb exit 5, so the run ends `exited`. Image
  generation takes minutes, so gweb's own default timeout is 300s with
  `--image-out`.
- `gweb --canvas` and `--canvas-out <path>` (need gweb 0.4.0) turn on Gemini's
  Canvas tool and read the reply's Canvas doc. tmuxlet forwards `--canvas-out`
  as an absolute path (resolved against `--cwd`) and passes `--output-format
  json`. The run's output is the answer, then the saved path (or the doc's
  markdown with `--canvas` alone), and `--output-format json` adds gweb's
  `canvas` object `{title, markdown, chars, path}`. A reply without a Canvas doc
  is a gweb exit 5, so the run ends `exited`. gweb's default timeout is 300s
  with `--canvas`.

  ```bash
  tmuxlet -p --target gweb -m Argon --canvas-out ./scene.md --output-format json "Write a 3-sentence scene into a Canvas doc"
  ```
- `--file <path>` becomes gweb `--file <absolute path>`; see [Attachments](#attachments---file).
- `chatgpt` (cgpt 0.1.0) works the same way. `--image-out <dir|path>` becomes
  cgpt `--image-out <absolute path> --output-format json`; cgpt saves each image
  as ChatGPT stores it (the original file, not a preview), and its `images`
  entries are `{path, width, height, bytes, mime, original, sourceWidth,
  sourceHeight, source}`. A reply without an image is a cgpt exit 5. cgpt's own
  default timeout is 600s with `--image-out`. `-m` takes a cgpt slug
  (`cgpt models`; e.g. `gpt-6-thinking`, `gpt-6-instant`).
- Model fields: with `--output-format json` or `--image-out`, tmuxlet reads the
  CLI's JSON result and copies its model fields into the run object, after
  `citations`: cgpt's `requestedModel` (the slug `-m` asked for, or null),
  `servedModel` (the slug that wrote the reply, from the chat's record),
  `servedModelDisplay`, `fallback`, `fallbackFrom`, `fallbackTo`, and gweb's
  `servedModel` / `servedModelId` when gweb reports them. A field the CLI
  doesn't report is left out.

  ```json
  {"id":"…","target":"chatgpt","status":"completed","output":"/abs/out/create-a-square-image-seamless-tileable-pattern-1.png\n",
   "…":"…","images":[{"path":"/abs/out/create-a-square-image-seamless-tileable-pattern-1.png","width":1254,"height":1254,
   "bytes":1200158,"mime":"image/png","original":true,"sourceWidth":1254,"sourceHeight":1254,"source":"original"}],
   "citations":[],"requestedModel":null,"servedModel":"gpt-5-4-thinking","servedModelDisplay":null,"fallback":false,
   "fallbackFrom":null,"fallbackTo":null}
  ```
- A cited gweb answer (gweb 0.4.1: Gemini's source chips for attached files or
  web pages) arrives with `[^n]` footnotes and a footnote block in the run's
  output. With `--output-format json`, tmuxlet also passes gweb `--output-format
  json` and adds gweb's `citations` list (`{n, label, file}` or `{n, label,
  title, url}` per footnote) to its result:

  ```json
  {"id":"…","target":"gweb","status":"completed","output":"… the attached outline[^1].\n\n[^1]: outline.md\n",
   "…":"…","citations":[{"n":1,"label":"MD","file":"outline.md"}]}
  ```

Flag mapping:

| tmuxlet flag | `muse` | `gweb` | `chatgpt` |
| --- | --- | --- | --- |
| `-m`, `--model` | rejected (Muse has no model picker) | `--model <name>` | `--model <slug>` |
| `--thinking <level>` | rejected | `--thinking <level>` | rejected |
| `--image-out <dir\|path>` | rejected | `--image-out <absolute path> --output-format json` | `--image-out <absolute path> --output-format json` |
| `--canvas` | rejected | `--canvas --output-format json` | rejected |
| `--canvas-out <path>` | rejected | `--canvas-out <absolute path> --output-format json` | rejected |
| `--file <path>` | text inlined into `prompt.txt`; other files refused (exit 7) | `--file <absolute path>` | `--file <absolute path>` |
| `--output-format json` | — | also `--output-format json` (for `citations`) | also `--output-format json` (for `citations` and the model fields) |
| `--continue`, value-less `--resume` | `-c` | `-c` | `-c` |
| `--resume <id>`, `--session-id <id>`, `--session <id>` | `--chat <id>` | `--chat <id>` | `--chat <id>` |
| `--timeout <secs>` | also `--timeout <secs>` | also `--timeout <secs>` | also `--timeout <secs>` |
| `--target-arg <arg>` | passed through | passed through | passed through (e.g. `--strict-model`) |
| `--system-prompt`, `--append-system-prompt`, tools flags, `--permission-mode`, `--add-dir`, `--mcp-config`, `--settings`, `--dangerously-skip-permissions` | rejected | rejected | rejected |

Each CLI drives one Chrome profile, so runs on the same backend queue on that
CLI's profile lock. tmuxlet passes `--lock-wait <timeout>` so a queued run waits
up to its run timeout instead of failing after the CLI's 30s default. The CLI's
answer timeout starts only after it gets the lock, so a long queue can still end
in tmuxlet's own `timeout`.

You need `llmcli` with print mode (`-p`) on the `PATH` of your login shell, with
each backend's identity already signed in (`llmcli muse login` /
`llmcli gemini login` / `llmcli chatgpt login`). Point `TMUXLET_LLMCLI_BIN`
at another program to override.

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
| `agy` | `--dangerously-skip-permissions` (no graded CLI mode; scoped allowlist lives in settings.json, not a flag) |
| `opencode` | `--dangerously-skip-permissions` (no confirmed headless allow-list flag) |
| `cursor` | `--force` (enables print-mode writes) |
| `hermes` | `--yolo` (bypasses dangerous-command approval; hermes has no scoped allow/deny tier) |
| `pi` | none — auto-approves tool use by default |
| `muse` / `gweb` / `chatgpt` | none — chat backends run no tools and no completion contract |

Policy: if a target has an auto-classifier (only `claude`'s `auto`), use it **and**
still pin the exact contract operations with explicit allow-rules rather than
trusting the classifier; otherwise use that target's yolo/bypass. `claude`,
`codex`, and `grok` end up genuinely scoped to the contract; `agy`, `opencode`,
`cursor`, and `hermes` fall back to their broad auto-approve because those CLIs offer no graded
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
  over and replaces the scoped grant. On Grok this maps to `--always-approve`; on Hermes to `--yolo`; on agy to `--dangerously-skip-permissions`.

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
- **Self-healing.** If a run waits past the 60s grace window while saturated, it
  runs one conservative reap pass. Only owned sessions at least **10 minutes**
  old whose panes are unchanged across two captures **2 seconds** apart qualify.
  `--reap-min-age <minutes>` overrides `TMUXLET_REAP_MIN_AGE_MIN` (default 10).
  Unknown ages, empty panes, and failed captures are skipped. Permission/trust prompts are
  always spared, including prompts appearing during the observation interval.
  Detection checks only the last 15 non-empty lines for prompt-shaped text;
  ordinary mentions of allow, confirm, or permission are insufficient.
  At the acquire timeout it exits non-zero with `at capacity …; run \`tmuxlet reap\`` rather than
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
| `--memory-pressure-guard` / `--memory-guard` / `TMUXLET_MEMORY_GUARD` | off | Queue under host memory pressure |
| `TMUXLET_MEMORY_MIN_FREE` | `15` | Free-memory % threshold for the guard |

See [Environment variables](#environment-variables) for the full list including
`TMUXLET_HOME` and `TMUXLET_CLAUDE_PERMISSION_MODE`.

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
  stderr.txt   # chat backends: the CLI's stderr
  launch.sh    # the launch command, when it would pass 1000 bytes typed
```

Set `TMUXLET_HOME` to override the state directory.

Every automatic/manual reap and every skip appends a JSON line to
`<state-dir>/reap.log`, including dry runs and declined confirmations. Each line
records an ISO UTC timestamp, `action` (`reaped` or `skipped`), full run id,
owner, cwd, target, age in seconds (null if unknown), and reason. Permission
skips say `waiting on permission/trust prompt`. Foreign and unreadable metadata
are also logged. Audit-log failures are reported; an unopenable log prevents
reaping. Manual reap can intentionally kill permission-blocked sessions after
confirmation; automatic reap always leaves those sessions for their owner.

A successful print-mode run auto-kills its tmux session before returning, but
the run directory itself is left on disk so callers can inspect `complete.txt`,
`pane.log`, and the rest. Runs that end in `timeout` or `blocked` leave the
tmux session alive too, so you can `attach` and investigate. A Codex run stopped
by a startup dialog is the exception: its session is killed and only the run
directory remains (see [Codex startup](#codex-startup)). `tmuxlet prune` is
how those run directories and any leftover sessions get cleaned up.

## Confirmation Handling

For Claude and Codex, tmuxlet passes the full prompt in the launch command with
the user's prompt first and tmuxlet's completion contract after it. By default
tmuxlet grants scoped permissions for the contract operations (see
[Permissions](#permissions)), so the completion step itself does not prompt.
During the startup window, tmuxlet also watches for common confirmation gates and
sends `Enter` up to three times (every target except Codex; see below). This
startup check runs during normal print-mode polling, so successful runs can
return as soon as the completion file appears.

### Codex startup

tmuxlet never presses `Enter` for Codex. Its startup menus save a folder-trust
choice, upgrade Codex, or restart the shared background server, so tmuxlet
avoids them instead:

- Every Codex launch passes `--no-daemon` (no shared background server, so no
  version-mismatch prompt) and `-c check_for_update_on_startup=false`.
- tmuxlet trusts the working folder for that run only when you pass `--force`
  (or another bypass alias) or the canonical cwd is under `~/Sites`. It passes
  `-c 'projects={"<canonical cwd>"={trust_level="trusted"}}'`. Nothing is saved
  to `~/.codex/config.toml`.

If Codex still stops on a startup dialog before its chat screen appears (the
folder-trust screen elsewhere, or any other numbered menu), tmuxlet kills the
run's tmux session and exits 1 within a few seconds instead of waiting for
`--timeout`:

```text
tmuxlet: codex startup blocked by interactive prompt: Trust this folder? ... | › 1. Trust and continue | 2. Quit; hint: pass --force, or run under ~/Sites, so tmuxlet trusts this folder for the run
```

The run directory keeps `pane.log` and `error.txt`.

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
- `exited`: the tmux session ended before completion. For chat backends the
  output is the CLI's error message.

Exit codes: **0** completed; **1** an error, or a run that ended `blocked`,
`timeout`, or `exited`; **6** gweb or chatgpt rate limited (`rate_limited`); **7** a
`--file` the target can't take (see [Attachments](#attachments---file)).

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
