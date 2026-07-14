---
name: tmuxlet
description: Run prompts through local coding CLIs (claude, codex, agy, opencode, pi, cursor, grok, hermes) as blocking `-p`-style programmatic calls via the tmuxlet CLI, which drives each CLI's interactive mode inside tmux and returns text or JSON. Use when the user asks to dispatch a task or prompt to a coding agent via tmuxlet, run something "through tmuxlet", fan out prompts across multiple agent sessions, check on / read / nudge / unblock tmuxlet runs, investigate a blocked or timed-out tmuxlet run, or clean up tmuxlet sessions and run directories (reap/prune). Also use when automation needs a claude -p equivalent that bills against the interactive subscription pool instead of Agent SDK credits.
---

# tmuxlet

`tmuxlet -p` launches a coding CLI (default `claude`) in interactive mode inside a
fresh tmux session, pastes the prompt, and blocks until the target satisfies a
completion contract (writes `answer.txt`, calls `tmuxlet bridge complete`). The
result comes back as text or JSON — like `claude -p`, but driven through the
interactive TUI, normalized across eight CLIs.

Sanity check before first use: `tmuxlet --version` (requires `tmux` on PATH and
the target CLI installed, e.g. `claude`).

## Launch a run

```bash
# Simple prompt, text output
tmuxlet -p "say ready"

# Programmatic: JSON result, explicit target, cwd, model, timeout
tmuxlet -p --target codex --output-format json --cwd ~/Sites/org/repo \
  --timeout 600 "run the test suite and summarize failures"

# Multi-line or quote-heavy prompts: pipe via stdin with `-`
tmuxlet -p --output-format json - <<'EOF'
Refactor src/lib.rs:
- extract the parser into its own module
- keep the public API unchanged
EOF
```

Rules:

- `-p` / `--print` is required for every run; non-print launches are unsupported.
- Prefer `--output-format json` whenever parsing the result.
- Prefer stdin (`-` + heredoc) for any prompt with newlines, quotes, or `$`.
- Common flags: `--target <name>`, `-C/--cwd <dir>`, `-m/--model <model>`,
  `--timeout <secs>` (default 1800), `--system-prompt <text>`,
  `--add-dir <path>`, `--target-arg <raw>` (escape hatch).
- Session-resume flags (`--continue`, `--resume`, `--session`, `--session-id`)
  are mutually exclusive and target-specific — read
  [references/targets.md](references/targets.md) before using them.

## Read the result

Exit code 0 means status `completed`; anything else is a failure. JSON output
is a single object:

```json
{"id":"<run-id>","target":"claude","status":"completed","output":"...",
 "cwd":"...","tmuxSession":"tmuxlet-<run-id>","completionSource":"explicit","elapsedMs":12345}
```

Text output prints only the target's final response. On failure, stderr always
ends with `tmuxlet: run <run-id> ended with status <status>` — capture stderr to
recover the run id even in text mode.

## Blocking call vs Bash tool timeout (critical)

`tmuxlet -p` blocks until completion or `--timeout` (default 1800s = 30 min).
The Bash tool defaults to 120s and caps at 600s, so a default tmuxlet run will
outlive a default Bash call. Pick one:

- **Quick prompts** (< ~2 min of agent work): pass `--timeout 120` or so, and a
  matching Bash `timeout`.
- **Longer tasks**: launch with `run_in_background: true` and let the harness
  notify on exit. To find the run id before it finishes:
  `ls -t ~/.tmuxlet/runs | head -1` (newest run dir), then monitor with
  `tmuxlet read <id>`.

Never lower `--timeout` below what the task realistically needs — a `timeout`
status kills nothing; it just stops waiting (see below).

## Handle failure statuses

| Status | Meaning | tmux session |
| --- | --- | --- |
| `blocked` | Pane stopped changing at a permission/confirmation prompt outside the default grant | left alive |
| `timeout` | Completion contract not satisfied before `--timeout` | left alive |
| `exited` | Target CLI/tmux session ended before completing | gone |

Recovery flow for `blocked` / `timeout`:

```bash
tmuxlet read <id> --lines 120        # capture the live pane — what is it waiting on?
tmuxlet send <id> "1"                # answer a menu/prompt (pastes text, then presses Enter)
tmuxlet send <id> ""                 # or just press Enter
tmuxlet read <id>                    # confirm it moved on
```

- The run may still complete after a `timeout` return — check
  `~/.tmuxlet/runs/<id>/complete.txt` before retrying.
- If blocked on permissions repeatedly, rerun with
  `--dangerously-skip-permissions` (full bypass) or a scoped
  `--allowed-tools <rule>` on claude/grok.
- `tmuxlet stop <id>` gracefully exits then kills a run you're abandoning.
- Debug artifacts live in `~/.tmuxlet/runs/<id>/`: `prompt.txt`, `answer.txt`,
  `complete.txt`, `pane.log`, `meta.json` (plus `error.txt` on
  `blocked`/`timeout`).
- **Never run `tmuxlet attach`** — it's interactive and will hang a
  non-interactive shell. It exists for the human; suggest it to the user
  instead. Use `read`/`send` yourself.

## Monitor live runs

```bash
tmuxlet status                         # one meta.json line per run dir (includes finished, un-pruned runs)
tmux ls 2>/dev/null | grep '^tmuxlet-' # only the actually-live sessions
tmuxlet read <id> --lines 80           # snapshot of a run's pane
```

`status` lists every run directory still on disk, not just live sessions — use
the `tmux ls` filter to see what's actually running.

## Fan out multiple runs

Each run is a full interactive TUI (hundreds of MB). tmuxlet enforces a
machine-wide live-session cap (default 24); launches at the cap queue until a
slot frees (up to `TMUXLET_ACQUIRE_TIMEOUT`, default 300s), then exit non-zero.

```bash
# Deliberate fan-out: set a tight cap, launch in background, collect results
export TMUXLET_MAX_CONCURRENCY=4
for task in "${tasks[@]}"; do
  tmuxlet -p --output-format json "$task" >> results.jsonl 2>> errors.log &
done
wait
```

- Nudge every live session at once with `tmuxlet send-all "<msg>"` (or
  `--enter`); it paces 1s between sessions (`--interval`) instead of herding.
- On memory-constrained hosts add `--memory-pressure-guard`
  (or `TMUXLET_MEMORY_GUARD=1`).

## Cleanup

Two distinct commands — both prompt for confirmation, so **always pass `-y`**
(or `--dry-run` to preview) from a non-interactive shell:

```bash
tmuxlet reap --dry-run             # preview: which live sessions look stuck/leaked
tmuxlet reap -y                    # kill sessions stuck at a prompt >60s (frees concurrency slots)
tmuxlet reap --blocked -y          # any session at a prompt, regardless of age
tmuxlet reap --older-than 7200 -y  # by run age

tmuxlet prune --dry-run            # preview: terminal run dirs (completed/timeout/blocked/exited)
tmuxlet prune -y                   # delete all terminal run dirs + their leftover sessions
tmuxlet prune <id> -y              # specific runs (refuses non-terminal ids)
```

`reap` kills live sessions; `prune` deletes finished run directories. A
successful run auto-kills its session but leaves the run dir for inspection, so
periodic `prune -y` is the normal hygiene step.

## Environment variables

| Variable | Default | Effect |
| --- | --- | --- |
| `TMUXLET_HOME` | `~/.tmuxlet` | Runtime state root (`runs/`) |
| `TMUXLET_MAX_CONCURRENCY` | `24` | Live-session cap; `0` = unlimited |
| `TMUXLET_ACQUIRE_TIMEOUT` | `300` | Seconds to wait for a free slot; `0` = forever |
| `TMUXLET_MEMORY_GUARD` | off | Queue launches under host memory pressure |
| `TMUXLET_MEMORY_MIN_FREE` | `15` | Free-memory % threshold for the guard |
| `TMUXLET_CLAUDE_PERMISSION_MODE` | `auto` | Default Claude `--permission-mode` |

## Per-target details

Session resume mappings, permission-grant behavior, and target quirks (e.g.
grok's `--session-id` creates a session rather than resuming; hermes takes no
positional prompt — tmuxlet types into its `--cli` REPL) differ by target.
Read [references/targets.md](references/targets.md) whenever using
`--continue`/`--resume`/`--session`/`--session-id`, permission flags, or a
non-claude target for the first time in a session.
