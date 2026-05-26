# README.md Structure Analysis

**File:** `/Users/kk/Sites/CodefiLabs/tmuxlet/README.md`
**Total line count:** 271 lines (the file ends at line 270 with content, line 271 is a trailing blank line)
**Date analyzed:** 2026-05-26

## 1. Top-Level Sections (in order)

| Line | Section |
| --- | --- |
| 1 | `# tmuxlet` (title) |
| 13 | `## Install` |
| 47 | `## Why` |
| 57 | `## Why not just `claude -p`?` |
| 73 | `## Origin` |
| 95 | `## Targets` |
| 109 | `## CLI` |
| 147 | `## Resuming Target Sessions` |
| 188 | `## How Print Mode Works` |
| 216 | `## Runtime State` |
| 234 | `## Confirmation Handling` |
| 248 | `## Output And Failures` |
| 265 | `## Development` |

## 2. Subcommand Documentation

The subcommands are documented in a single fenced code block inside the `## CLI` section at **lines 111–118**:

```
111	```bash
112	tmuxlet -p [options] [prompt]
113	tmuxlet status
114	tmuxlet read <id> [--lines N]
115	tmuxlet send <id> <message>
116	tmuxlet attach <id>
117	tmuxlet stop <id>
118	```
```

### Per-subcommand line references in that block:

- **`-p` (print)**: line 112 — `tmuxlet -p [options] [prompt]`
- **`status`**: line 113 — `tmuxlet status`
- **`read`**: line 114 — `tmuxlet read <id> [--lines N]`
- **`send`**: line 115 — `tmuxlet send <id> <message>`
- **`attach`**: line 116 — `tmuxlet attach <id>`
- **`stop`**: line 117 — `tmuxlet stop <id>`

### `bridge` subcommand

`bridge` is **not listed in the CLI subcommand block** (lines 111–118). It is only referenced indirectly in the "How Print Mode Works" section:

- Line 194: `transcript and writes the final assistant text to `complete.txt`.` (mentions `tmuxlet bridge stop-hook`)
- Line 193: `- A `Stop` hook calls `tmuxlet bridge stop-hook`, which reads Claude's`
- Line 195: `- A `PostToolUse` hook calls `tmuxlet bridge tool-hook`, which touches`
- Line 213: `The contract tells Codex to write its final response to `answer.txt` and call`
- Line 214: ``tmuxlet bridge complete`. Other targets (Gemini, opencode, pi, Cursor) use a`

There is no standalone `tmuxlet bridge` example block in the CLI section. Bridge is treated as an internal helper invoked by hooks/contracts rather than as a user-facing subcommand.

### Other examples in the CLI section (lines 139–145):

```
139	Examples:
140	
141	```bash
142	tmuxlet -p --target claude --continue "summarize the last task"
143	tmuxlet -p --target codex --session-id 01984d2f-... "continue the fix"
144	tmuxlet -p --target opencode --resume ses_abc123 "check status"
145	```
```

A `prune` subcommand would naturally fit by adding a new line into the block at lines 111–118, e.g. between line 117 (`tmuxlet stop <id>`) and line 118 (closing fence), following the same `tmuxlet <verb> <id>` style.

## 3. Runtime State Section (verbatim)

Section header at line 216. The full section spans lines 216–232:

```
216	## Runtime State
217	
218	Runtime files live in:
219	
220	```text
221	~/.tmuxlet/runs/<run-id>/
222	  meta.json
223	  prompt.txt
224	  answer.txt        # written by the target on non-Claude runs
225	  complete.txt
226	  pane.log
227	  error.txt
228	  settings.json     # generated for Claude runs (hook config)
229	  heartbeat         # touched by the Claude PostToolUse hook
230	```
231	
232	Set `TMUXLET_HOME` to override the state directory.
```

This is the canonical place to mention that `prune` deletes `~/.tmuxlet/runs/<run-id>/` directories. The directory layout listing ends at line 230; a paragraph at line 232 already documents an override (`TMUXLET_HOME`). A new paragraph after line 232 (or before it) would be the natural location to describe per-run cleanup.

## 4. Status States Section (verbatim)

The nonzero statuses are documented inside the `## Output And Failures` section. Header at line 248. Status states are listed at lines 254–263:

```
248	## Output And Failures
249	
250	Text output prints only the target's final response when the completion contract
251	is satisfied. JSON output includes the run id, target, status, output, cwd, tmux
252	session, completion source, and elapsed time.
253	
254	Nonzero statuses:
255	
256	- `blocked`: the pane stopped changing and appears to be waiting on a
257	  permission or confirmation prompt.
258	- `timeout`: the target did not satisfy the completion contract before
259	  `--timeout`.
260	- `exited`: the tmux session ended before completion.
261	
262	On `blocked` and `timeout`, tmuxlet writes `pane.log` and `error.txt` under the
263	run directory for debugging.
```

### Important observations about the status states section:

- The README lists **only three nonzero statuses**: `blocked` (line 256), `timeout` (line 258), `exited` (line 260).
- **`completed` is NOT documented as a status name in this section.** The section is explicitly framed as "Nonzero statuses" (line 254), so the success status is implicit.
- The word "completed" does not appear in `## Output And Failures`. It appears nowhere else in the README as a named status either (verified via reading the full file).
- The phrase "completion contract" appears at lines 198, 213, 237, 241, 250 but refers to the protocol, not a status state.

If the plan wants to note that `completed` now auto-kills, it would need to either:
- Add `completed` as a documented (zero-exit) status to this section, or
- Add a sentence somewhere else (e.g. in `## Runtime State` or a new cleanup section) noting the behavior.

## 5. Existing Cleanup / Session-Management Discussion

**There is no existing "Cleanup" section in the README.** Verified by scanning every section heading.

The closest existing references to session lifecycle / cleanup:

- **Line 134**: `Non-print prompt launches are intentionally unsupported; pass `-p` for runs.`
- **Lines 136–137**: `Session controls are mutually exclusive. Use one of `--continue`, `--resume`, / `--session`, or `--session-id` per launch.`
- **Line 117**: `tmuxlet stop <id>` — the only existing teardown-related subcommand, listed without further explanation.
- **Line 246**: `tmuxlet attach <id>.` (in `## Confirmation Handling`, as a debugging suggestion)
- **Line 260**: `- `exited`: the tmux session ended before completion.` — only mention of a tmux session ending.
- **Line 263**: `run directory for debugging.` — explains that `pane.log` / `error.txt` persist after failure for debugging, implying runs are *not* auto-cleaned.
- **Line 232**: `Set `TMUXLET_HOME` to override the state directory.` — only mention of where runtime files live.

The "Resuming Target Sessions" section (lines 147–177) discusses **target CLI conversation sessions**, not tmux sessions or run directories. Line 163–165 explicitly disambiguates:

```
163	These flags select the target CLI conversation, not the `tmuxlet` run id under
164	`~/.tmuxlet/runs/<run-id>/`. They are mutually exclusive, so use only one per
165	launch.
```

So the README currently:

- Documents `tmuxlet stop <id>` as a subcommand name only (no description of what it does to the run directory or tmux session).
- Does **not** discuss removing run directories.
- Does **not** discuss what happens to tmux sessions after `completed`, `blocked`, `timeout`, or `exited` runs.
- Does **not** discuss bulk cleanup, retention, or garbage collection of `~/.tmuxlet/runs/`.

## 6. Total Line Count

`README.md` ends with content at line 270 (`cargo build`) followed by a trailing newline. The file has **271 lines** total (270 content + 1 trailing newline) per the Read tool output, which displayed line 271 as blank.

## Summary of Fit for `prune` Documentation

| Insertion point | Line range | Purpose |
| --- | --- | --- |
| CLI subcommand list | between 117 and 118 | Add `tmuxlet prune [...]` to the canonical subcommand block |
| CLI examples | after 145 | Optional concrete examples mirroring the existing pattern |
| Runtime State | after 232 | Note that `prune` deletes `~/.tmuxlet/runs/<run-id>/` directories |
| Output And Failures (status states) | between 260 and 262 | Optional: add `completed` as a documented status if behavior changes |
| New top-level section | after 264 (before `## Development`) | Optional: a new `## Cleanup` section if the prune feature warrants standalone discussion |

No existing "Cleanup" section exists, so the plan can either add prune mentions to existing sections (CLI, Runtime State, Output And Failures) or introduce a new section between `## Output And Failures` (ends line 264) and `## Development` (starts line 265).
