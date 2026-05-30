# tmuxlet — Concurrency / Throttling Plan (conceptual)

> Status: **Layer 1 implemented in tmuxlet 0.4.x** (2026-05-30). See
> "Implementation status" below for what shipped, what was deferred, and why.
> Motivation: an unthrottled fan-out of tmuxlet runs crashed a Mac.

## What happened (the incident)

Each `tmuxlet -p` run launches a **full interactive Claude Code TUI** inside its own
tmux session. Those are heavy (hundreds of MB resident each). Two failure modes
compounded:

1. **Silent accumulation.** A permission-gate bug (runs hung at the Claude "create
   `answer.txt`?" prompt instead of completing) meant sessions never reached the
   completion contract and never tore down. The consumer's concurrency cap only
   limited *new launches*, not *live sessions*, so ~135 zombie Claude TUIs piled up.
2. **Thundering-herd resume.** Sending Enter to all ~135 stalled sessions at once made
   every TUI resume inference simultaneously → VM_compressor/VM_pageout pinned, swap
   thrash, kernel memory-pressure crash + reboot.

Root lesson: **tmuxlet has no notion of "how many of me are alive right now," and no
backpressure.** The cap lives in the consumer, is launch-only, and is defeated the
moment runs stop completing.

## Goals

- Make it structurally impossible for tmuxlet to have more than N live sessions at once,
  regardless of how many callers/loops fan out.
- Ramp launches (no cold-start herd); pace resumes/keystrokes across sessions.
- Detect and reap stuck/blocked/leaked sessions so the cap reflects reality.
- Optional: refuse to launch under host memory pressure.
- Keep single-run behavior unchanged (zero overhead when not at cap).

## Implementation status (2026-05-30)

Layer 1 shipped in `src/main.rs`, dependency-free, after a 4-lens design review.

**Shipped:**

- **Live-session semaphore** — the slot count is the number of live tmux
  sessions matching the run-id grammar (`tmuxlet-<hex>-<hex>[-<hex>]`), read from
  `tmux list-sessions`. Crash-safe / self-clearing; siblings like `tmuxlet-server`
  are excluded. Implemented as a **soft cap** (no machine-wide lock): a rare
  simultaneous-launch race may transiently reach cap+1 and self-corrects — the
  live count, not a lock, is the source of truth. (`flock(2)` is the noted upgrade
  path if real overshoot is ever observed; rejected for v1 as over-engineering and
  for adding an mtime stale-break race.)
- **Block-and-queue semantics** with a one-line `waiting for a free slot (N/M
  live)` notice, a bounded `TMUXLET_ACQUIRE_TIMEOUT` (default 300s; `0` = forever),
  and a dedicated non-zero exit (`at capacity …; run \`tmuxlet reap\``) so a saturated
  consumer can back off instead of hot-looping.
- **`tmuxlet reap`** — standalone sweep of leaked sessions, a thin extension of the
  `prune` confirmation UX. Default is conservative (confirmation-blocked **and**
  aged past a 60s grace window); `--blocked` / `--older-than` widen it. Also runs
  once automatically from within a saturated acquire (self-heal).
- **`tmuxlet send-all [msg] [--interval] [--enter]`** — paced keystrokes across
  live sessions; directly fixes the mass-resume thundering herd.
- **Memory-pressure backpressure (opt-in)** — `--memory-pressure-guard` /
  `TMUXLET_MEMORY_GUARD`, parsing macOS `memory_pressure` / Linux
  `/proc/meminfo`; unknown platform degrades to "allow".
- **Pre-existing bug fixed:** tmux `-t` does unique-**prefix** matching (verified:
  `has-session -t tmuxlet-1a2b` matched a live `tmuxlet-1a2b3c`), so liveness and
  every kill/send could mis-target. All targeting now forces exact match — `=name`
  for session commands, `=name:` for pane commands. Also hardened `make_run_id`
  with a process-local counter (fixed a latent same-nanosecond collision that was
  also a flaky test).

**Decisions (resolving the open questions below):**

- **Cap default = 24, a runaway guardrail, not a throttle.** Default-ON cap=4 was
  rejected as a silent behavior change that would block any existing caller fanning
  out past 4 — the throttle value belongs in the consumer (`TMUXLET_MAX_CONCURRENCY`),
  the binary offers the mechanism. `0` disables.
- **Flat machine cap** (not per-target) for v1; the heaviest target dominates and
  the memory guard covers heterogeneity.
- **Lives in the CLI** (not tmuxlet-server) so the cap holds for every caller.

**Deferred:** the explicit `--launch-interval` stagger — redundant with the
semaphore's queue (the 250ms re-poll already spaces launches at the cap), and it
would add a second shared-state file. Revisit only if real ramps still herd.

**Out of scope (other layers):** lowering the consumer's own cap (Layer 2) and an
external circuit breaker such as `pm2 stop` (Layer 3) remain the consumer's
responsibility — complementary to, not replaced by, the in-CLI cap.

## Proposed mechanisms (by layer)

### Layer 1 — tmuxlet core (the real fix)

- **Global live-session semaphore.** A machine-wide cap on concurrent runs, enforced via
  a lockfile/counter in `~/.tmuxlet/` (count = live run dirs / sessions). A new run
  acquires a slot before creating its tmux session and **blocks or queues** if the cap
  is hit. Slot released on completion *or* on reap. Default conservative (e.g. 3–4).
- **Launch stagger.** Minimum interval between session creations (`--launch-interval`),
  so even a burst of allowed runs ramps in rather than cold-starting together.
- **Liveness + zombie reaping.** A run that exceeds its timeout or sits in `blocked`
  past a grace window gets its tmux session killed and its semaphore slot freed. This is
  what was missing — blocked runs leaked forever. Could be a per-run watchdog and/or a
  `tmuxlet reap` sweep.
- **Memory-pressure backpressure (opt-in).** Before acquiring a slot, sample host memory
  pressure (macOS `memory_pressure` / `vm_stat`); if under pressure, queue rather than
  launch.
- **Paced batch keystrokes.** Any "act on many sessions" operation (e.g. a `tmuxlet
  drain`/`resume-all`/`send-all`) walks sessions with a configurable inter-session delay
  — never a tight loop firing send-keys at every session at once. This directly fixes the
  mass-resume trigger.

### Layer 2 — analyze pipeline (tfb-wiki consumer)

- **Live-aware gating, not just launch-count.** Before each pass, count live tmuxlet
  sessions; skip/throttle if already at cap (don't trust an in-process counter).
- Lower default `TFB_CONCURRENCY`; consider memory-aware auto-tuning.
- **Fix the root permission bug** so runs complete and tear down (the actual incident
  trigger) — sessions accumulating is the precondition for everything else.

### Layer 3 — guardrail / circuit breaker

- A watchdog that **hard-stops the spawner** (e.g. `pm2 stop tfb-analyze-loop`) and kills
  zombies if live-session count or memory pressure crosses a danger threshold. Last line
  of defense independent of tmuxlet's own cap.

## Open questions (resolved — see "Implementation status" above)

> These are kept for context; each is answered in the shipped design: queue (not
> fail-fast); the semaphore lives as a soft cap derived from live tmux sessions
> (no persisted integer); "blocked" = confirmation-blocked + 60s grace;
> flat machine cap; defaults = cap 24 / 300s acquire / 60s reap grace / 15%
> memory floor; it lives in the CLI.

- Cap semantics: block vs. queue vs. fail-fast when full? (queue is friendliest to the
  analyze loop)
- Where does the semaphore live and how is it made crash-safe (stale slots after a hard
  crash must self-clear — count live sessions, don't trust a persisted integer)?
- What counts as "blocked" for reaping vs. legitimately-long inference? Grace window?
- Should the cap be per-target-CLI (claude heavier than others) or a flat machine cap?
- Default numbers: live cap, launch interval, reap grace, memory-pressure threshold.
- Does this belong partly in `tmuxlet-server` (central coordinator) vs. the CLI?

## Immediate (already done / cheap)

- Saved durable guidance: never mass-spawn/resume Claude TUIs (see memory
  `tmuxlet-concurrency-crash-risk`).
- The analyze permission bug is the precondition for the accumulation. Verified against
  the **authoritative Agent SDK permissions doc** (code.claude.com/docs/en/agent-sdk/permissions)
  + the settings page. The real evaluation order is:

  > **Hooks → Deny rules → Permission MODE → Allow rules → canUseTool**

  Permission mode is checked **before** allow rules. That ordering drives everything:

  - **`auto` is a model classifier** ("a model classifier approves or denies each tool
    call"), documented **TypeScript-SDK only** — and the settings-page table describes it
    differently ("auto-approve actions matching configured rules"). The two docs conflict,
    so `auto` is the wrong primitive to depend on for deterministic headless behavior.
    For the `answer.txt` write it neither matched a rule (none existed) nor got classified
    as safe → it prompted.
  - **`acceptEdits` *does* honor `--add-dir`.** Doc, verbatim: Edit/Write + fs commands
    are auto-approved but "both apply only to paths inside the working directory or
    `additionalDirectories`. Paths outside that scope … still prompt." So acceptEdits +
    `--add-dir <rundir>` *would* have auto-approved `answer.txt` — but it's broader than
    needed (auto-approves *all* in-scope edits) **and still wouldn't cover the Bash bridge
    command** (non-filesystem Bash isn't an "edit"). So acceptEdits alone is neither
    minimal nor sufficient. (My earlier note calling it "the fix" was wrong on both counts.)
  - **Two independent gates for a write:** (1) directory **access** — `--add-dir` /
    `additionalDirectories` (settings doc: grants external-dir file access but "does not
    extend tool permission rules"); (2) tool **permission** — a matching allow-rule (or an
    approving mode). tmuxlet 0.4.0 had `--add-dir <rundir>` + a `Bash(...)` rule but **no
    `Write(...)` rule**, and ran under `auto` — so the write satisfied gate #1, had nothing
    to match at the allow step, and fell to the classifier/prompt.

  **Shipped approach (per-target policy, validated against the docs).** *If a
  target has an auto-classifier for tool calls, use it AND still pin the exact
  contract operations (write `answer.txt`/etc. + the `tmuxlet bridge` call) with
  explicit allow-rules; otherwise use that target's yolo/bypass; and if pinning
  under the classifier is not possible or stops working, fall back to yolo.* Only
  `claude` has a classifier (`auto`), so the shipped `claude` grant keeps `auto`
  mode and adds the previously-missing `Write`/`Edit` allow-rules for the rundir
  alongside the `Bash` bridge rule:

  ```
  --permission-mode auto \
  --add-dir /Users/kk/.tmuxlet/runs/<id> \
  --allowedTools 'Write(//Users/kk/.tmuxlet/runs/<id>/**)' \
                 'Edit(//Users/kk/.tmuxlet/runs/<id>/**)' \
                 'Bash(/Users/kk/.cargo/bin/tmuxlet bridge complete:*)'
  ```

  Verified end-to-end: a real `claude -p` run writes `answer.txt` and calls the
  bridge with no permission prompt → `complete.txt` is produced.

  - **Why not `--permission-mode default`?** An earlier draft called `default` +
    explicit rules "the correct fix." It is deterministic but it is *not* the
    endpoint: under `default`, any rule mismatch (an abs path that resolves
    differently, a glob that doesn't match) drops straight to an interactive
    prompt and hangs a headless run. Keeping the `auto` classifier UNDER the
    explicit allow-rules is strictly safer for a headless drop-in — the rules
    handle the contract deterministically, and the classifier catches anything
    they miss instead of prompting. The fact-check also corrected the draft's
    model of `auto`: it is a background *safety classifier*, not a blanket
    approval of in-`--add-dir` writes — which is exactly why the rules must be
    explicit, and why the old `auto` + Bash-rule-only grant (no `Write` rule)
    was still prompt-prone.
  - The `Write`/`Edit` rules approve `answer.txt` (and any rundir file) at the
    allow step; `--add-dir` satisfies the directory-access gate; the `Bash` rule
    approves the bridge (Bash is NOT gated by `--add-dir`). tmuxlet emits the
    resolved gitignore `//abs` form (`//abs`, `~/home`, `/rel-to-settings`,
    `path` are the four forms).
  - `Bash(cmd:*)` is prefix-match but shell-operator-aware; the rule string must
    match the actual absolute-path invocation. Version note: `auto` in *project*
    settings is ignored since v2.1.142 (untrusted-repo guard); only
    `~/.claude/settings.json` honors it.
  - **Next robustness step (deferred, separate from concurrency):** collapse the
    contract to a **single Bash-gated op** (the model passes its answer to the
    bridge via stdin/heredoc, or writes `answer.txt` inside cwd) so there is no
    out-of-cwd Write gate at all and only one scoped `Bash(...)` rule is needed —
    the most version-stable shape, since Bash allow-rules are the most
    deterministic primitive.
