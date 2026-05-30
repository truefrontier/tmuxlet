use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_TARGET: &str = "claude";
const DEFAULT_TIMEOUT_SECS: u64 = 30 * 60;
const STARTUP_AUTO_ENTER_WINDOW_SECS: u64 = 8;
const STARTUP_AUTO_ENTER_MAX_ATTEMPTS: usize = 3;
const EXECUTION_STALL_SECS: u64 = 60;
const PANE_POLL_SECS: u64 = 1;
// Concurrency / throttling. The live-session semaphore is a soft cap: the count
// is derived from live `tmuxlet-*` tmux sessions (crash-safe, self-clearing),
// so a rare simultaneous-launch race may transiently reach cap+1 and self-corrects
// on the next acquire. The default is a high runaway guardrail, not a throttle —
// consumers that want tighter pacing set TMUXLET_MAX_CONCURRENCY.
const DEFAULT_MAX_CONCURRENCY: usize = 24;
const DEFAULT_ACQUIRE_TIMEOUT_SECS: u64 = 300;
const ACQUIRE_POLL_MS: u64 = 250;
const REAP_GRACE_SECS: u64 = EXECUTION_STALL_SECS;
const SEND_ALL_DEFAULT_INTERVAL_SECS: u64 = 1;
const DEFAULT_MEMORY_MIN_FREE_PCT: u8 = 15;

#[derive(Debug, Clone, PartialEq, Eq)]
enum OutputFormat {
    Text,
    Json,
}

impl OutputFormat {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "text" => Ok(Self::Text),
            "json" => Ok(Self::Json),
            "stream-json" => {
                Err("tmuxlet v3 does not support --output-format stream-json yet".into())
            }
            other => Err(format!("unsupported output format: {other}")),
        }
    }
}

#[derive(Debug, Clone)]
struct Options {
    print: bool,
    target: String,
    output_format: OutputFormat,
    cwd: PathBuf,
    prompt_parts: Vec<String>,
    model: Option<String>,
    resume: Option<Option<String>>,
    continue_latest: bool,
    session: Option<String>,
    session_id: Option<String>,
    system_prompt: Option<String>,
    append_system_prompts: Vec<String>,
    add_dirs: Vec<String>,
    dangerously_skip_permissions: bool,
    permission_mode: Option<String>,
    allowed_tools: Vec<String>,
    disallowed_tools: Vec<String>,
    tools: Vec<String>,
    mcp_configs: Vec<String>,
    settings: Option<String>,
    target_args: Vec<String>,
    contract_grant: Option<ContractGrant>,
    timeout: Duration,
    // None => resolve from TMUXLET_MAX_CONCURRENCY env, else DEFAULT_MAX_CONCURRENCY.
    // 0 == unlimited (semaphore disabled).
    max_concurrency: Option<usize>,
    // Opt-in host-memory backpressure (also enabled by TMUXLET_MEMORY_GUARD).
    memory_guard: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            print: false,
            target: DEFAULT_TARGET.to_string(),
            output_format: OutputFormat::Text,
            cwd: env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            prompt_parts: Vec::new(),
            model: None,
            resume: None,
            continue_latest: false,
            session: None,
            session_id: None,
            system_prompt: None,
            append_system_prompts: Vec::new(),
            add_dirs: Vec::new(),
            dangerously_skip_permissions: false,
            permission_mode: None,
            allowed_tools: Vec::new(),
            disallowed_tools: Vec::new(),
            tools: Vec::new(),
            mcp_configs: Vec::new(),
            settings: None,
            target_args: Vec::new(),
            contract_grant: None,
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            max_concurrency: None,
            memory_guard: false,
        }
    }
}

#[derive(Debug)]
struct RunPaths {
    root: PathBuf,
    meta: PathBuf,
    prompt: PathBuf,
    answer: PathBuf,
    complete: PathBuf,
    pane_log: PathBuf,
    error: PathBuf,
}

/// Per-run authorization context for the completion contract. tmuxlet knows the
/// run dir (which holds answer.txt/complete.txt, outside the target's cwd) and
/// its own absolute exe path at launch, so it can grant exactly the two contract
/// operations to each target without a blanket permission bypass.
#[derive(Debug, Clone)]
struct ContractGrant {
    run_root: PathBuf,
    exe: String,
}

#[derive(Debug)]
struct RunResult {
    id: String,
    target: String,
    status: String,
    output: String,
    output_format: OutputFormat,
    cwd: PathBuf,
    tmux_session: String,
    completion_source: String,
    elapsed_ms: u128,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunClassification {
    Completed,
    Timeout,
    Blocked,
    Exited,
    InProgress,
}

impl RunClassification {
    fn label(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Timeout => "timeout",
            Self::Blocked => "blocked",
            Self::Exited => "exited",
            Self::InProgress => "in-progress",
        }
    }

    fn is_terminal(self) -> bool {
        !matches!(self, Self::InProgress)
    }
}

#[derive(Debug)]
struct RunRecord {
    id: String,
    classification: RunClassification,
    age_secs: Option<u64>,
    session_alive: bool,
}

#[derive(Debug, Clone)]
struct TargetCommand {
    program: String,
    args: Vec<String>,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("tmuxlet: {err}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<(), String> {
    let mut args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() {
        print_help();
        return Ok(());
    }

    match args[0].as_str() {
        "-h" | "--help" => {
            print_help();
            Ok(())
        }
        "-v" | "--version" => {
            println!("tmuxlet {VERSION}");
            Ok(())
        }
        "bridge" => handle_bridge(&args[1..]),
        "status" => handle_status(),
        "read" => handle_read(&args[1..]),
        "send" => handle_send(&args[1..]),
        "attach" => handle_attach(&args[1..]),
        "stop" => handle_stop(&args[1..]),
        "prune" => handle_prune(&args[1..]),
        "reap" => handle_reap(&args[1..]),
        "send-all" => handle_send_all(&args[1..]),
        _ => {
            let opts = parse_options(&mut args)?;
            if opts.print {
                let result = run_print(opts)?;
                emit_result(&result)?;
            } else {
                return Err("tmuxlet currently supports print mode only; pass -p/--print".into());
            }
            Ok(())
        }
    }
}

fn parse_options(args: &mut [String]) -> Result<Options, String> {
    let mut opts = Options::default();
    let mut i = 0;

    while i < args.len() {
        let arg = &args[i];
        match arg.as_str() {
            "-p" | "--print" => opts.print = true,
            "--target" => opts.target = take_value(args, &mut i, arg)?,
            "--output-format" => {
                opts.output_format = OutputFormat::parse(&take_value(args, &mut i, arg)?)?;
            }
            "-C" | "--cwd" | "--cd" | "--dir" => {
                opts.cwd = expand_home(&take_value(args, &mut i, arg)?);
            }
            "-m" | "--model" => opts.model = Some(take_value(args, &mut i, arg)?),
            "-c" | "--continue" => opts.continue_latest = true,
            "-r" | "--resume" => {
                let value = optional_value(args, &mut i);
                opts.resume = Some(value);
            }
            "--session" => opts.session = Some(take_value(args, &mut i, arg)?),
            "--session-id" => opts.session_id = Some(take_value(args, &mut i, arg)?),
            "--system-prompt" => opts.system_prompt = Some(take_value(args, &mut i, arg)?),
            "--append-system-prompt" => {
                opts.append_system_prompts
                    .push(take_value(args, &mut i, arg)?);
            }
            "--add-dir" | "--include-directories" => {
                let value = take_value(args, &mut i, arg)?;
                opts.add_dirs.extend(split_csv(&value));
            }
            "--dangerously-skip-permissions"
            | "--yolo"
            | "--force"
            | "--dangerously-bypass-approvals-and-sandbox" => {
                opts.dangerously_skip_permissions = true;
            }
            "--permission-mode" => opts.permission_mode = Some(take_value(args, &mut i, arg)?),
            "--allowedTools" | "--allowed-tools" => {
                opts.allowed_tools.push(take_value(args, &mut i, arg)?);
            }
            "--disallowedTools" | "--disallowed-tools" => {
                opts.disallowed_tools.push(take_value(args, &mut i, arg)?);
            }
            "--tools" => opts.tools.push(take_value(args, &mut i, arg)?),
            "--mcp-config" => opts.mcp_configs.push(take_value(args, &mut i, arg)?),
            "--settings" => opts.settings = Some(take_value(args, &mut i, arg)?),
            "--target-arg" => opts.target_args.push(take_value(args, &mut i, arg)?),
            "--timeout" => {
                let seconds = take_value(args, &mut i, arg)?
                    .parse::<u64>()
                    .map_err(|_| "--timeout must be an integer number of seconds".to_string())?;
                opts.timeout = Duration::from_secs(seconds);
            }
            "--max-concurrency" => {
                let n = take_value(args, &mut i, arg)?
                    .parse::<usize>()
                    .map_err(|_| {
                        "--max-concurrency must be a non-negative integer (0 = unlimited)"
                            .to_string()
                    })?;
                opts.max_concurrency = Some(n);
            }
            "--memory-pressure-guard" | "--memory-guard" => opts.memory_guard = true,
            "--" => {
                opts.prompt_parts.extend(args[i + 1..].iter().cloned());
                break;
            }
            other if other.starts_with('-') => return Err(format!("unknown option: {other}")),
            _ => opts.prompt_parts.push(arg.clone()),
        }
        i += 1;
    }

    Ok(opts)
}

#[derive(Debug, Default, PartialEq, Eq)]
struct PruneArgs {
    ids: Vec<String>,
    yes: bool,
    dry_run: bool,
}

fn parse_prune_args(args: &[String]) -> Result<PruneArgs, String> {
    let mut out = PruneArgs::default();
    for arg in args {
        match arg.as_str() {
            "-y" | "--yes" => out.yes = true,
            "--dry-run" => out.dry_run = true,
            other if other.starts_with('-') => {
                return Err(format!("unknown option for prune: {other}"));
            }
            other => out.ids.push(other.to_string()),
        }
    }
    Ok(out)
}

fn prune_selection(
    records: Vec<RunRecord>,
    explicit_ids: &[String],
) -> Result<Vec<RunRecord>, String> {
    if explicit_ids.is_empty() {
        return Ok(records
            .into_iter()
            .filter(|r| r.classification.is_terminal())
            .collect());
    }

    use std::collections::HashMap;
    let mut by_id: HashMap<String, RunRecord> = HashMap::new();
    for r in records {
        by_id.insert(r.id.clone(), r);
    }

    let mut found: Vec<RunRecord> = Vec::new();
    let mut unknown: Vec<String> = Vec::new();
    let mut non_terminal: Vec<String> = Vec::new();
    for id in explicit_ids {
        match by_id.remove(id) {
            None => unknown.push(id.clone()),
            Some(r) if !r.classification.is_terminal() => non_terminal.push(r.id.clone()),
            Some(r) => found.push(r),
        }
    }
    if !unknown.is_empty() {
        return Err(format!("unknown run(s): {}", unknown.join(", ")));
    }
    if !non_terminal.is_empty() {
        return Err(format!(
            "run(s) still in progress; use `tmuxlet stop <id>` first: {}",
            non_terminal.join(", ")
        ));
    }
    Ok(found)
}

struct PruneOutcome {
    deleted: usize,
    failures: Vec<String>,
}

fn execute_prune(selected: &[RunRecord], runs_root: &Path, dry_run: bool) -> PruneOutcome {
    if dry_run {
        return PruneOutcome {
            deleted: 0,
            failures: Vec::new(),
        };
    }
    let mut deleted = 0usize;
    let mut failures = Vec::new();
    for rec in selected {
        if rec.session_alive {
            let session_name = format!("tmuxlet-{}", rec.id);
            let _ = run_tmux([
                OsStr::new("kill-session"),
                OsStr::new("-t"),
                OsStr::new(&tmux_target(&session_name)),
            ]);
        }
        let dir = runs_root.join(&rec.id);
        match fs::remove_dir_all(&dir) {
            Ok(()) => deleted += 1,
            Err(e) => {
                eprintln!("tmuxlet: failed to remove {}: {e}", dir.display());
                failures.push(rec.id.clone());
            }
        }
    }
    PruneOutcome { deleted, failures }
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

fn format_age_secs(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

fn take_value(args: &[String], i: &mut usize, flag: &str) -> Result<String, String> {
    *i += 1;
    args.get(*i)
        .cloned()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| format!("{flag} requires a value"))
}

fn optional_value(args: &[String], i: &mut usize) -> Option<String> {
    if let Some(next) = args.get(*i + 1)
        && !next.starts_with('-')
    {
        *i += 1;
        return Some(next.clone());
    }
    None
}

fn split_csv(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn prompt_from(opts: &Options) -> Result<String, String> {
    let joined = opts.prompt_parts.join(" ");
    if joined == "-" || joined.is_empty() {
        let mut input = String::new();
        io::stdin()
            .read_to_string(&mut input)
            .map_err(|e| format!("failed to read stdin: {e}"))?;
        Ok(input)
    } else {
        Ok(joined)
    }
}

fn run_print(mut opts: Options) -> Result<RunResult, String> {
    ensure_tmux()?;
    let started = Instant::now();
    let run_id = make_run_id();
    let tmux_session = format!("tmuxlet-{run_id}");
    let paths = run_paths(&run_id)?;
    fs::create_dir_all(&paths.root).map_err(|e| format!("failed to create run dir: {e}"))?;

    // Authorize the completion-contract operations for this run. The per-target
    // builders translate this into the narrowest grant available (claude/codex
    // get true scoping; gemini/opencode/cursor fall back to their auto-approve
    // mode because their CLIs offer no graded tier).
    opts.contract_grant = Some(ContractGrant {
        run_root: paths.root.clone(),
        exe: tmuxlet_exe(),
    });

    let prompt = prompt_from(&opts)?;
    fs::write(&paths.prompt, &prompt).map_err(|e| format!("failed to write prompt: {e}"))?;
    let instruction = completion_prompt(&prompt, &run_id, &paths.answer);
    let base_command = target_command(&opts)?;
    let (launch_command, prompt_in_launch) =
        command_with_prompt(base_command.clone(), &opts.target, &instruction);

    write_meta(&paths, &run_id, &tmux_session, &opts, &base_command)?;
    // Acquire a concurrency slot before the heavy TUI session exists. Blocks/queues
    // when at cap or (opt-in) under memory pressure; on a clean acquire-timeout we
    // drop the staged run dir so it doesn't linger as a phantom "exited" run.
    if let Err(e) = acquire_run_slot(&opts) {
        fs::remove_dir_all(&paths.root).ok();
        return Err(e);
    }
    start_tmux_session(&tmux_session, &opts.cwd)?;
    send_launch_command(&tmux_session, &launch_command)?;
    if !prompt_in_launch {
        paste_text(&tmux_session, &instruction)?;
    }

    let startup_auto_enter_deadline =
        Instant::now() + Duration::from_secs(STARTUP_AUTO_ENTER_WINDOW_SECS);
    let mut startup_enter_attempts = 0usize;
    let mut last_pane = String::new();
    let mut last_pane_changed_at = Instant::now();
    let mut next_pane_poll = Instant::now();

    loop {
        if paths.complete.exists() {
            let output = fs::read_to_string(&paths.complete)
                .map_err(|e| format!("failed to read completion file: {e}"))?;
            capture_pane_to_file(&tmux_session, &paths.pane_log).ok();
            let _ = run_tmux([
                OsStr::new("kill-session"),
                OsStr::new("-t"),
                OsStr::new(&tmux_target(&tmux_session)),
            ]);
            return Ok(RunResult {
                id: run_id,
                target: opts.target,
                status: "completed".into(),
                output,
                output_format: opts.output_format,
                cwd: opts.cwd,
                tmux_session,
                completion_source: "explicit".into(),
                elapsed_ms: started.elapsed().as_millis(),
            });
        }

        if Instant::now() >= next_pane_poll {
            let pane = capture_pane(&tmux_session, 200).unwrap_or_default();
            if Instant::now() <= startup_auto_enter_deadline
                && startup_enter_attempts < STARTUP_AUTO_ENTER_MAX_ATTEMPTS
                && is_startup_auto_enter_prompt(&pane)
                && send_enter(&tmux_session).is_ok()
            {
                startup_enter_attempts += 1;
            }

            if pane != last_pane {
                last_pane = pane;
                last_pane_changed_at = Instant::now();
            } else if !pane.trim().is_empty()
                && last_pane_changed_at.elapsed() >= Duration::from_secs(EXECUTION_STALL_SECS)
                && is_execution_confirmation_blocker(&pane)
            {
                fs::write(&paths.pane_log, &pane).ok();
                fs::write(
                    &paths.error,
                    blocked_error_message(&opts.target, &run_id, startup_enter_attempts),
                )
                .ok();
                return Ok(RunResult {
                    id: run_id,
                    target: opts.target,
                    status: "blocked".into(),
                    output: pane,
                    output_format: opts.output_format,
                    cwd: opts.cwd,
                    tmux_session,
                    completion_source: "fallback".into(),
                    elapsed_ms: started.elapsed().as_millis(),
                });
            }
            next_pane_poll = Instant::now() + Duration::from_secs(PANE_POLL_SECS);
        }

        if started.elapsed() >= opts.timeout {
            let pane = capture_pane(&tmux_session, 200).unwrap_or_default();
            fs::write(&paths.pane_log, &pane).ok();
            fs::write(&paths.error, "timeout waiting for explicit completion\n").ok();
            return Ok(RunResult {
                id: run_id,
                target: opts.target,
                status: "timeout".into(),
                output: pane,
                output_format: opts.output_format,
                cwd: opts.cwd,
                tmux_session,
                completion_source: "fallback".into(),
                elapsed_ms: started.elapsed().as_millis(),
            });
        }

        if !tmux_session_exists(&tmux_session) {
            let pane = fs::read_to_string(&paths.pane_log).unwrap_or_default();
            return Ok(RunResult {
                id: run_id,
                target: opts.target,
                status: "exited".into(),
                output: pane,
                output_format: opts.output_format,
                cwd: opts.cwd,
                tmux_session,
                completion_source: "fallback".into(),
                elapsed_ms: started.elapsed().as_millis(),
            });
        }

        thread::sleep(Duration::from_millis(500));
    }
}

fn command_with_prompt(
    mut command: TargetCommand,
    target: &str,
    final_prompt: &str,
) -> (TargetCommand, bool) {
    if target_accepts_prompt_argument(target) {
        // Terminate option parsing before the positional prompt. claude's
        // --allowed-tools/--add-dir (and codex's --add-dir/--image) are variadic
        // `<...>` flags; without `--`, the parser swallows the prompt as another
        // flag value and the CLI launches idle with no prompt. `--` is a no-op
        // when no variadic flag precedes it, so it is always safe to emit here.
        command.args.push("--".to_string());
        command.args.push(final_prompt.to_string());
        (command, true)
    } else {
        (command, false)
    }
}

fn target_accepts_prompt_argument(target: &str) -> bool {
    matches!(target, "claude" | "codex")
}

fn target_command(opts: &Options) -> Result<TargetCommand, String> {
    validate_session_controls(opts)?;

    let target = opts.target.as_str();
    let mut cmd = match target {
        "claude" => claude_command(opts)?,
        "gemini" => gemini_command(opts)?,
        "codex" => codex_command(opts)?,
        "opencode" => opencode_command(opts)?,
        "pi" => pi_command(opts)?,
        "cursor" | "cursor-agent" => cursor_command(opts)?,
        other => return Err(format!("unsupported target: {other}")),
    };
    cmd.args.extend(opts.target_args.iter().cloned());
    Ok(cmd)
}

fn validate_session_controls(opts: &Options) -> Result<(), String> {
    let selectors = [
        ("--resume", opts.resume.is_some()),
        ("--session", opts.session.is_some()),
        ("--session-id", opts.session_id.is_some()),
    ];
    let selected = selectors
        .iter()
        .filter_map(|(flag, present)| present.then_some(*flag))
        .collect::<Vec<_>>();

    if opts.continue_latest && !selected.is_empty() {
        return Err(format!(
            "--continue cannot be combined with {}",
            selected.join(", ")
        ));
    }

    if selected.len() > 1 {
        return Err(format!(
            "session selectors are mutually exclusive: {}",
            selected.join(", ")
        ));
    }

    Ok(())
}

fn claude_command(opts: &Options) -> Result<TargetCommand, String> {
    let mut args = Vec::new();
    push_opt(&mut args, "--model", &opts.model);
    // Default autonomous (tmux-driven, non-interactive) claude runs to the
    // `auto` permission mode: it auto-approves safe operations (in-cwd edits,
    // safe bash) and only escalates genuinely dangerous ones, so headless runs
    // don't hang on permission prompts the way bare interactive claude does.
    // An explicit --permission-mode overrides this; --dangerously-skip-permissions
    // (full bypass) takes precedence and suppresses --permission-mode entirely.
    // Override the default per-invocation with `--permission-mode default`, or
    // globally via the TMUXLET_CLAUDE_PERMISSION_MODE env var.
    let permission_mode = if opts.dangerously_skip_permissions {
        None
    } else if opts.permission_mode.is_some() {
        opts.permission_mode.clone()
    } else {
        Some(
            env::var("TMUXLET_CLAUDE_PERMISSION_MODE")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "auto".to_string()),
        )
    };
    push_opt(&mut args, "--permission-mode", &permission_mode);
    if opts.dangerously_skip_permissions {
        args.push("--dangerously-skip-permissions".into());
    }
    push_opt(&mut args, "--system-prompt", &opts.system_prompt);
    for value in &opts.append_system_prompts {
        args.extend(["--append-system-prompt".into(), value.clone()]);
    }
    for value in &opts.add_dirs {
        args.extend(["--add-dir".into(), value.clone()]);
    }
    for value in &opts.allowed_tools {
        args.extend(["--allowed-tools".into(), value.clone()]);
    }
    for value in &opts.disallowed_tools {
        args.extend(["--disallowed-tools".into(), value.clone()]);
    }
    for value in &opts.tools {
        args.extend(["--tools".into(), value.clone()]);
    }
    for value in &opts.mcp_configs {
        args.extend(["--mcp-config".into(), value.clone()]);
    }
    push_opt(&mut args, "--settings", &opts.settings);
    if opts.continue_latest {
        args.push("--continue".into());
    }
    if let Some(value) = &opts.resume {
        args.push("--resume".into());
        if let Some(value) = value {
            args.push(value.clone());
        }
    }
    push_opt(&mut args, "--session-id", &opts.session_id);
    if opts.session.is_some() {
        unsupported("claude", "--session", "use --session-id or --resume")?;
    }
    // Explicitly whitelist the completion-contract operations so headless runs
    // never hang on a permission prompt for them. claude has an auto-classifier
    // (`auto`, the default mode); per policy we keep it AND pin the exact
    // contract ops with allow-rules rather than trusting the classifier. The
    // model writes its answer (and any complete/error notes) into the run dir,
    // which lives outside cwd: --add-dir clears the directory-access gate while
    // the Write/Edit rules clear the tool-permission gate, and the bridge call
    // is allow-listed by command prefix. File-rule paths use the gitignore
    // `//abs` form. Skipped under full bypass, which already permits everything.
    if !opts.dangerously_skip_permissions
        && let Some(grant) = &opts.contract_grant
    {
        let root = grant.run_root.display().to_string();
        let abs = format!("//{}", root.trim_start_matches('/'));
        args.extend(["--add-dir".into(), root]);
        args.extend(["--allowed-tools".into(), format!("Write({abs}/**)")]);
        args.extend(["--allowed-tools".into(), format!("Edit({abs}/**)")]);
        args.extend([
            "--allowed-tools".into(),
            format!("Bash({} bridge complete:*)", grant.exe),
        ]);
    }
    Ok(TargetCommand {
        program: "claude".into(),
        args,
    })
}

fn gemini_command(opts: &Options) -> Result<TargetCommand, String> {
    reject_any("gemini", "--system-prompt", &opts.system_prompt)?;
    reject_nonempty(
        "gemini",
        "--append-system-prompt",
        &opts.append_system_prompts,
    )?;
    reject_nonempty("gemini", "--allowed-tools/--tools", &opts.allowed_tools)?;
    reject_nonempty("gemini", "--disallowed-tools", &opts.disallowed_tools)?;
    reject_nonempty("gemini", "--tools", &opts.tools)?;
    reject_nonempty("gemini", "--mcp-config", &opts.mcp_configs)?;
    reject_any("gemini", "--settings", &opts.settings)?;
    reject_any("gemini", "--permission-mode", &opts.permission_mode)?;
    reject_any("gemini", "--session-id", &opts.session_id)?;
    reject_any("gemini", "--session", &opts.session)?;

    let mut args = Vec::new();
    push_opt(&mut args, "--model", &opts.model);
    // Gemini has no graded auto mode and `auto_edit` does not cover shell tools,
    // so the completion-contract shell call requires full auto-approve (--yolo).
    if opts.dangerously_skip_permissions || opts.contract_grant.is_some() {
        args.push("--yolo".into());
    }
    for value in &opts.add_dirs {
        args.extend(["--include-directories".into(), value.clone()]);
    }
    if opts.continue_latest {
        args.extend(["--resume".into(), "latest".into()]);
    }
    if let Some(value) = &opts.resume {
        args.push("--resume".into());
        args.push(value.clone().unwrap_or_else(|| "latest".into()));
    }
    Ok(TargetCommand {
        program: "gemini".into(),
        args,
    })
}

fn codex_command(opts: &Options) -> Result<TargetCommand, String> {
    reject_any("codex", "--system-prompt", &opts.system_prompt)?;
    reject_nonempty(
        "codex",
        "--append-system-prompt",
        &opts.append_system_prompts,
    )?;
    reject_nonempty("codex", "--allowed-tools", &opts.allowed_tools)?;
    reject_nonempty("codex", "--disallowed-tools", &opts.disallowed_tools)?;
    reject_nonempty("codex", "--tools", &opts.tools)?;
    reject_nonempty("codex", "--mcp-config", &opts.mcp_configs)?;
    reject_any("codex", "--settings", &opts.settings)?;
    reject_any("codex", "--permission-mode", &opts.permission_mode)?;
    reject_any("codex", "--session", &opts.session)?;

    let mut args = Vec::new();
    if opts.continue_latest {
        args.extend(["resume".into(), "--last".into()]);
    } else if let Some(value) = &opts.resume {
        args.push("resume".into());
        if let Some(value) = value {
            args.push(value.clone());
        } else {
            args.push("--last".into());
        }
    } else if let Some(value) = &opts.session_id {
        args.extend(["resume".into(), value.clone()]);
    }
    push_opt(&mut args, "--model", &opts.model);
    args.extend(["--cd".into(), opts.cwd.display().to_string()]);
    for value in &opts.add_dirs {
        args.extend(["--add-dir".into(), value.clone()]);
    }
    if opts.dangerously_skip_permissions {
        args.push("--dangerously-bypass-approvals-and-sandbox".into());
    } else if let Some(grant) = &opts.contract_grant {
        // Codex sandboxes by filesystem, not per-tool prompts. The run dir lives
        // outside the workspace-write sandbox, so carve it in as a writable root
        // and disable approval prompts while keeping the sandbox otherwise intact.
        args.extend(["--ask-for-approval".into(), "never".into()]);
        args.extend(["--sandbox".into(), "workspace-write".into()]);
        args.extend([
            "-c".into(),
            format!(
                "sandbox_workspace_write.writable_roots=[\"{}\"]",
                grant.run_root.display()
            ),
        ]);
    }
    Ok(TargetCommand {
        program: "codex".into(),
        args,
    })
}

fn opencode_command(opts: &Options) -> Result<TargetCommand, String> {
    reject_any("opencode", "--system-prompt", &opts.system_prompt)?;
    reject_nonempty(
        "opencode",
        "--append-system-prompt",
        &opts.append_system_prompts,
    )?;
    reject_nonempty("opencode", "--allowed-tools", &opts.allowed_tools)?;
    reject_nonempty("opencode", "--disallowed-tools", &opts.disallowed_tools)?;
    reject_nonempty("opencode", "--tools", &opts.tools)?;
    reject_nonempty("opencode", "--mcp-config", &opts.mcp_configs)?;
    reject_any("opencode", "--settings", &opts.settings)?;
    reject_any("opencode", "--permission-mode", &opts.permission_mode)?;
    reject_nonempty("opencode", "--add-dir", &opts.add_dirs)?;

    let mut args = Vec::new();
    push_opt(&mut args, "--model", &opts.model);
    if opts.continue_latest {
        args.push("--continue".into());
    }
    push_opt(&mut args, "--session", &opts.session);
    push_opt(&mut args, "--session", &opts.session_id);
    if let Some(value) = &opts.resume {
        args.push("--session".into());
        if let Some(value) = value {
            args.push(value.clone());
        } else {
            return Err(
                "opencode --resume requires a session id; use --continue for latest".into(),
            );
        }
    }
    args.extend(["--dir".into(), opts.cwd.display().to_string()]);
    // opencode has no graded auto mode and no confirmed headless allow-list flag,
    // so its bypass flag is the only reliable non-interactive auto-approve path.
    if opts.dangerously_skip_permissions || opts.contract_grant.is_some() {
        args.push("--dangerously-skip-permissions".into());
    }
    Ok(TargetCommand {
        program: "opencode".into(),
        args,
    })
}

fn pi_command(opts: &Options) -> Result<TargetCommand, String> {
    reject_any("pi", "--permission-mode", &opts.permission_mode)?;
    reject_nonempty("pi", "--mcp-config", &opts.mcp_configs)?;
    reject_any("pi", "--settings", &opts.settings)?;
    reject_nonempty("pi", "--add-dir", &opts.add_dirs)?;
    reject_nonempty("pi", "--disallowed-tools", &opts.disallowed_tools)?;

    let mut args = Vec::new();
    push_opt(&mut args, "--model", &opts.model);
    push_opt(&mut args, "--system-prompt", &opts.system_prompt);
    for value in &opts.append_system_prompts {
        args.extend(["--append-system-prompt".into(), value.clone()]);
    }
    for value in &opts.tools {
        args.extend(["--tools".into(), value.clone()]);
    }
    for value in &opts.allowed_tools {
        args.extend(["--tools".into(), value.clone()]);
    }
    if opts.continue_latest {
        args.push("--continue".into());
    }
    push_opt(&mut args, "--session", &opts.session);
    push_opt(&mut args, "--session", &opts.session_id);
    if let Some(value) = &opts.resume {
        args.push("--resume".into());
        if let Some(value) = value {
            args.push(value.clone());
        }
    }
    if opts.dangerously_skip_permissions {
        return Err("pi has no direct --dangerously-skip-permissions/--yolo equivalent; use --target-arg for explicit pi flags".into());
    }
    // pi auto-approves tool use by default, so the completion contract needs no
    // extra grant — opts.contract_grant requires no injection here.
    Ok(TargetCommand {
        program: "pi".into(),
        args,
    })
}

fn cursor_command(opts: &Options) -> Result<TargetCommand, String> {
    reject_any("cursor-agent", "--system-prompt", &opts.system_prompt)?;
    reject_nonempty(
        "cursor-agent",
        "--append-system-prompt",
        &opts.append_system_prompts,
    )?;
    reject_nonempty("cursor-agent", "--allowed-tools", &opts.allowed_tools)?;
    reject_nonempty("cursor-agent", "--disallowed-tools", &opts.disallowed_tools)?;
    reject_nonempty("cursor-agent", "--tools", &opts.tools)?;
    reject_nonempty("cursor-agent", "--mcp-config", &opts.mcp_configs)?;
    reject_any("cursor-agent", "--settings", &opts.settings)?;
    reject_any("cursor-agent", "--permission-mode", &opts.permission_mode)?;
    reject_nonempty("cursor-agent", "--add-dir", &opts.add_dirs)?;
    reject_any("cursor-agent", "--session", &opts.session)?;

    let mut args = Vec::new();
    push_opt(&mut args, "--model", &opts.model);
    if let Some(value) = &opts.resume {
        args.push("--resume".into());
        if let Some(value) = value {
            args.push(value.clone());
        }
    }
    push_opt(&mut args, "--resume", &opts.session_id);
    if opts.continue_latest {
        args.push("resume".into());
    }
    // --force lets cursor-agent print mode perform writes (incl. the answer file).
    // The bridge shell call may additionally need a .cursor/cli.json allow entry
    // depending on version; --force is the best single-flag auto-approve available.
    if opts.dangerously_skip_permissions || opts.contract_grant.is_some() {
        args.push("--force".into());
    }
    Ok(TargetCommand {
        program: "cursor-agent".into(),
        args,
    })
}

fn push_opt(args: &mut Vec<String>, flag: &str, value: &Option<String>) {
    if let Some(value) = value {
        args.extend([flag.into(), value.clone()]);
    }
}

fn reject_any<T>(target: &str, flag: &str, value: &Option<T>) -> Result<(), String> {
    if value.is_some() {
        unsupported(target, flag, "")
    } else {
        Ok(())
    }
}

fn reject_nonempty<T>(target: &str, flag: &str, values: &[T]) -> Result<(), String> {
    if values.is_empty() {
        Ok(())
    } else {
        unsupported(target, flag, "")
    }
}

fn unsupported(target: &str, flag: &str, hint: &str) -> Result<(), String> {
    let suffix = if hint.is_empty() {
        String::new()
    } else {
        format!(" ({hint})")
    };
    Err(format!(
        "{target} does not support normalized flag {flag}{suffix}"
    ))
}

fn tmuxlet_exe() -> String {
    env::current_exe()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "tmuxlet".into())
}

fn completion_prompt(prompt: &str, run_id: &str, answer_path: &Path) -> String {
    let exe = tmuxlet_exe();
    format!(
        "{prompt}\n\n\
TMUXLET COMPLETION CONTRACT:\n\
When you have the final response ready, write only that final response to this file:\n\
{answer}\n\n\
Then run this exact command:\n\
{exe} bridge complete --run {run_id} --file {answer}\n\n\
Do not wait for the user after the final response is written.",
        answer = answer_path.display(),
    )
}

fn start_tmux_session(name: &str, cwd: &Path) -> Result<(), String> {
    run_tmux([
        OsStr::new("new-session"),
        OsStr::new("-d"),
        OsStr::new("-s"),
        OsStr::new(name),
        OsStr::new("-x"),
        OsStr::new("220"),
        OsStr::new("-y"),
        OsStr::new("50"),
        OsStr::new("-c"),
        cwd.as_os_str(),
    ])
    .map(|_| ())
}

/// tmux target for an exact *session* name (has-session, kill-session,
/// attach-session). A leading `=` forces exact matching instead of tmux's
/// unique-prefix resolution, so `=tmuxlet-1a2b` never resolves to a different
/// run whose id merely starts with `1a2b`.
fn tmux_target(name: &str) -> String {
    format!("={name}")
}

/// tmux target for an exact *pane* (send-keys, capture-pane, paste-buffer).
/// Pane targets prefix-match too, but the exact form needs a trailing `:` to
/// delimit the session component: bare `=name` is parsed as a pane spec and
/// errors `can't find pane`, while `=name:` selects exactly that session's
/// active pane (verified on tmux 3.6a).
fn tmux_pane_target(name: &str) -> String {
    format!("={name}:")
}

fn send_launch_command(tmux_session: &str, command: &TargetCommand) -> Result<(), String> {
    let mut parts = Vec::with_capacity(command.args.len() + 1);
    parts.push(shell_quote(&command.program));
    for arg in &command.args {
        parts.push(shell_quote(arg));
    }
    let line = parts.join(" ");
    let target = tmux_pane_target(tmux_session);
    run_tmux([
        OsStr::new("send-keys"),
        OsStr::new("-t"),
        OsStr::new(&target),
        OsStr::new(&line),
        OsStr::new("Enter"),
    ])
    .map(|_| ())
}

fn paste_text(tmux_session: &str, text: &str) -> Result<(), String> {
    let target = tmux_pane_target(tmux_session);
    let tmp = env::temp_dir().join(format!("tmuxlet-paste-{}.txt", make_run_id()));
    fs::write(&tmp, text).map_err(|e| format!("failed to stage prompt: {e}"))?;
    let load = run_tmux([OsStr::new("load-buffer"), tmp.as_os_str()]);
    fs::remove_file(&tmp).ok();
    load?;
    run_tmux([
        OsStr::new("paste-buffer"),
        OsStr::new("-t"),
        OsStr::new(&target),
    ])?;
    thread::sleep(Duration::from_millis(200));
    run_tmux([
        OsStr::new("send-keys"),
        OsStr::new("-t"),
        OsStr::new(&target),
        OsStr::new("Enter"),
    ])
    .map(|_| ())
}

fn send_enter(tmux_session: &str) -> Result<(), String> {
    let target = tmux_pane_target(tmux_session);
    run_tmux([
        OsStr::new("send-keys"),
        OsStr::new("-t"),
        OsStr::new(&target),
        OsStr::new("Enter"),
    ])
    .map(|_| ())
}

/// Lowercased pane text with tmuxlet's own launch-flag tokens removed. The shell
/// echoes the command tmuxlet typed into the pane, and flags such as
/// `--permission-mode`, `--allowed-tools`, and `--dangerously-skip-permissions`
/// contain the very words ("permission", "allow") the confirmation heuristics
/// scan for. Stripping them keeps detection focused on the target CLI's own
/// prompts instead of tmuxlet's own launch command echo.
fn confirmation_scan_text(pane: &str) -> String {
    let mut text = pane.to_ascii_lowercase();
    for token in [
        "--permission-mode",
        "--allowed-tools",
        "--dangerously-skip-permissions",
    ] {
        text = text.replace(token, " ");
    }
    text
}
fn is_startup_auto_enter_prompt(pane: &str) -> bool {
    let text = confirmation_scan_text(pane);
    contains_any(
        &text,
        &[
            "press enter",
            "press return",
            "hit enter",
            "enter to continue",
            "return to continue",
            "yes/no",
            "[y/n]",
            "(y/n)",
            "allow",
            "approve",
            "proceed",
            "permission",
            "confirm",
            "do you want to continue",
            "continue?",
            "requires approval",
        ],
    )
}

fn is_execution_confirmation_blocker(pane: &str) -> bool {
    let text = confirmation_scan_text(pane);
    contains_any(
        &text,
        &[
            "yes/no",
            "[y/n]",
            "(y/n)",
            "allow",
            "approve",
            "proceed",
            "permission",
            "confirm",
            "do you want to continue",
            "continue?",
            "requires approval",
            "waiting for approval",
            "press enter",
            "press return",
            "enter to continue",
        ],
    )
}

fn contains_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}

fn blocked_error_message(target: &str, run_id: &str, startup_enter_attempts: usize) -> String {
    format!(
        "blocked waiting for a confirmation or permission prompt\n\
target: {target}\n\
startup auto-enter attempts: {startup_enter_attempts}\n\
hint: rerun with --dangerously-skip-permissions if supported by this target, or attach with tmuxlet attach {run_id} to resolve manually\n"
    )
}

fn classify_from_markers(
    complete_exists: bool,
    error_first_line: Option<&str>,
    session_alive: bool,
) -> RunClassification {
    if complete_exists {
        return RunClassification::Completed;
    }
    if let Some(line) = error_first_line {
        if line.starts_with("timeout ") {
            return RunClassification::Timeout;
        }
        if line.starts_with("blocked ") {
            return RunClassification::Blocked;
        }
        // Unknown error.txt content — treat as Blocked (still terminal).
        return RunClassification::Blocked;
    }
    if !session_alive {
        return RunClassification::Exited;
    }
    RunClassification::InProgress
}

fn classify_run(run_dir: &Path, session_alive: bool) -> Option<RunClassification> {
    if !run_dir.join("meta.json").exists() {
        return None;
    }
    let complete = run_dir.join("complete.txt").exists();
    let error_path = run_dir.join("error.txt");
    let error_first_line = if error_path.exists() {
        fs::read_to_string(&error_path)
            .ok()
            .and_then(|s| s.lines().next().map(str::to_string))
    } else {
        None
    };
    Some(classify_from_markers(
        complete,
        error_first_line.as_deref(),
        session_alive,
    ))
}

fn discover_runs() -> Vec<RunRecord> {
    discover_runs_in(&state_dir().join("runs"))
}

fn discover_runs_in(runs_root: &Path) -> Vec<RunRecord> {
    let entries = match fs::read_dir(runs_root) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    let mut records = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let id = match path.file_name().and_then(|n| n.to_str()) {
            Some(name) => name.to_string(),
            None => continue,
        };
        let session_alive = tmux_session_exists(&format!("tmuxlet-{id}"));
        let Some(classification) = classify_run(&path, session_alive) else {
            continue;
        };
        // heartbeat-based age is only available when the Claude hook
        // integration is present; on this branch it's always None.
        let age_secs: Option<u64> = None;
        records.push(RunRecord {
            id,
            classification,
            age_secs,
            session_alive,
        });
    }

    records.sort_by(|a, b| a.id.cmp(&b.id));
    records
}

fn tmux_session_exists(name: &str) -> bool {
    Command::new("tmux")
        .args(["has-session", "-t", &tmux_target(name)])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn capture_pane(tmux_session: &str, lines: usize) -> Result<String, String> {
    let start = format!("-{lines}");
    let target = tmux_pane_target(tmux_session);
    run_tmux([
        OsStr::new("capture-pane"),
        OsStr::new("-t"),
        OsStr::new(&target),
        OsStr::new("-p"),
        OsStr::new("-S"),
        OsStr::new(&start),
    ])
}

fn capture_pane_to_file(tmux_session: &str, path: &Path) -> Result<(), String> {
    let output = capture_pane(tmux_session, 500)?;
    fs::write(path, output).map_err(|e| format!("failed to write pane log: {e}"))
}

fn run_tmux<I, S>(args: I) -> Result<String, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = Command::new("tmux")
        .args(args)
        .output()
        .map_err(|e| format!("failed to execute tmux: {e}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

fn ensure_tmux() -> Result<(), String> {
    Command::new("tmux")
        .arg("-V")
        .output()
        .map_err(|_| "tmux is required but was not found on PATH".to_string())
        .and_then(|output| {
            if output.status.success() {
                Ok(())
            } else {
                Err("tmux is required but did not run successfully".into())
            }
        })
}

// ---------------------------------------------------------------------------
// Concurrency / throttling
//
// The live-session count is derived from live tmux sessions whose name matches
// the run-id grammar (`tmuxlet-<hex>-<hex>[-<hex>]`). This is crash-safe: dead
// sessions vanish from tmux, so the count self-clears with no persisted integer
// to reconcile. It is a *soft* cap (no machine-wide lock): two truly-simultaneous
// launches may briefly reach cap+1, which self-corrects on the next acquire and
// is harmless relative to the unbounded pileup it prevents.
// ---------------------------------------------------------------------------

/// A tmux session name is a tmuxlet *run* session iff it is `tmuxlet-` followed
/// by >= 2 dash-separated lowercase-hex segments (the make_run_id grammar). This
/// excludes siblings like `tmuxlet-server` or a human's `tmuxlet-foo` so they
/// never consume a concurrency slot.
fn is_run_session_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("tmuxlet-") else {
        return false;
    };
    let mut segments = 0usize;
    for seg in rest.split('-') {
        if seg.is_empty() || !seg.bytes().all(|b| b.is_ascii_hexdigit()) {
            return false;
        }
        segments += 1;
    }
    segments >= 2
}

/// Raw `tmux list-sessions` names. Uses Command directly (not run_tmux) so the
/// "no server running" case (nonzero exit, empty stdout) maps to an empty string
/// instead of an error — the zero-overhead single-run path must not fail here.
fn raw_tmux_session_list() -> String {
    match Command::new("tmux")
        .args(["list-sessions", "-F", "#{session_name}"])
        .output()
    {
        Ok(out) => String::from_utf8_lossy(&out.stdout).into_owned(),
        Err(_) => String::new(),
    }
}

/// Count run sessions in `tmux list-sessions -F '#{session_name}'` output.
fn count_run_sessions(list_output: &str) -> usize {
    list_output
        .lines()
        .map(str::trim)
        .filter(|name| is_run_session_name(name))
        .count()
}

fn live_run_session_count() -> usize {
    count_run_sessions(&raw_tmux_session_list())
}

fn list_run_sessions() -> Vec<String> {
    raw_tmux_session_list()
        .lines()
        .map(str::trim)
        .filter(|name| is_run_session_name(name))
        .map(str::to_string)
        .collect()
}

/// Pure resolution: explicit --max-concurrency wins, else parse the env value,
/// else the default guardrail. 0 == unlimited.
fn pick_max_concurrency(cli: Option<usize>, env_val: Option<&str>) -> usize {
    if let Some(n) = cli {
        return n;
    }
    env_val
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_MAX_CONCURRENCY)
}

fn resolve_max_concurrency(cli: Option<usize>) -> usize {
    pick_max_concurrency(cli, env::var("TMUXLET_MAX_CONCURRENCY").ok().as_deref())
}

/// Pure resolution of the acquire-wait budget; 0 == wait forever.
fn pick_acquire_timeout(env_val: Option<&str>) -> Duration {
    let secs = env_val
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_ACQUIRE_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

fn resolve_acquire_timeout() -> Duration {
    pick_acquire_timeout(env::var("TMUXLET_ACQUIRE_TIMEOUT").ok().as_deref())
}

fn env_flag(name: &str) -> bool {
    match env::var(name) {
        Ok(v) => matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Err(_) => false,
    }
}

fn memory_guard_enabled(opts: &Options) -> bool {
    opts.memory_guard || env_flag("TMUXLET_MEMORY_GUARD")
}

fn memory_min_free_pct() -> u8 {
    env::var("TMUXLET_MEMORY_MIN_FREE")
        .ok()
        .and_then(|v| v.trim().parse::<u8>().ok())
        .unwrap_or(DEFAULT_MEMORY_MIN_FREE_PCT)
}

/// Free-memory percentage from `memory_pressure` ("System-wide memory free
/// percentage: N%"). None when not found.
fn parse_macos_free_pct(out: &str) -> Option<u8> {
    for line in out.lines().rev() {
        if let Some(rest) = line
            .trim()
            .strip_prefix("System-wide memory free percentage:")
        {
            let pct = rest.trim().trim_end_matches('%').trim();
            if let Ok(v) = pct.parse::<f64>() {
                return Some(v.round().clamp(0.0, 100.0) as u8);
            }
        }
    }
    None
}

/// Free-memory percentage from /proc/meminfo (MemAvailable / MemTotal). None
/// when MemAvailable is absent (very old kernels).
fn parse_linux_free_pct(meminfo: &str) -> Option<u8> {
    let mut total: Option<u64> = None;
    let mut avail: Option<u64> = None;
    for line in meminfo.lines() {
        let mut it = line.split_whitespace();
        match it.next() {
            Some("MemTotal:") => total = it.next().and_then(|v| v.parse().ok()),
            Some("MemAvailable:") => avail = it.next().and_then(|v| v.parse().ok()),
            _ => {}
        }
    }
    match (total, avail) {
        (Some(t), Some(a)) if t > 0 => {
            Some(((a as f64 / t as f64) * 100.0).round().clamp(0.0, 100.0) as u8)
        }
        _ => None,
    }
}

/// `true` only with a reading below the threshold. Unknown (None) degrades to
/// "not under pressure" so the opt-in guard can never wedge a launch.
fn under_memory_pressure(free_pct: Option<u8>, min_free: u8) -> bool {
    matches!(free_pct, Some(p) if p < min_free)
}

/// Sample host free-memory percentage, dependency-free, per platform. Any
/// failure / unknown platform => None (allow).
fn sample_free_pct() -> Option<u8> {
    if cfg!(target_os = "macos") {
        let out = Command::new("memory_pressure").output().ok()?;
        if !out.status.success() {
            return None;
        }
        parse_macos_free_pct(&String::from_utf8_lossy(&out.stdout))
    } else if cfg!(target_os = "linux") {
        parse_linux_free_pct(&fs::read_to_string("/proc/meminfo").ok()?)
    } else {
        None
    }
}

/// Block until a concurrency slot is free (and, with the opt-in guard, until the
/// host is not under memory pressure), or error at the acquire-timeout. Zero
/// overhead when no throttling is configured.
fn acquire_run_slot(opts: &Options) -> Result<(), String> {
    let cap = resolve_max_concurrency(opts.max_concurrency);
    let guard = memory_guard_enabled(opts);
    if cap == 0 && !guard {
        return Ok(()); // fast path: nothing to enforce
    }
    let min_free = memory_min_free_pct();
    let timeout = resolve_acquire_timeout(); // ZERO == wait forever
    let start = Instant::now();
    let mut announced = false;
    let mut self_reaped = false;

    loop {
        let mem_block = guard && under_memory_pressure(sample_free_pct(), min_free);
        let live = if cap == 0 {
            0
        } else {
            live_run_session_count()
        };
        let cap_block = cap != 0 && live >= cap;
        if !mem_block && !cap_block {
            return Ok(());
        }

        if !announced {
            let why = if cap_block {
                format!("{live}/{cap} live sessions")
            } else {
                "host under memory pressure".to_string()
            };
            eprintln!("tmuxlet: waiting for a free slot ({why})");
            announced = true;
        }

        // Self-heal: one reap pass after the grace window so a fully-leaked
        // machine recovers without a manual `tmuxlet reap`. Only blocked, aged
        // sessions are killed (same safety as the standalone reaper).
        if cap_block && !self_reaped && start.elapsed() >= Duration::from_secs(REAP_GRACE_SECS) {
            self_reaped = true;
            let killed = reap_run_sessions(&ReapCriteria::default());
            if killed > 0 {
                eprintln!("tmuxlet: reaped {killed} stuck session(s)");
                continue;
            }
        }

        if !timeout.is_zero() && start.elapsed() >= timeout {
            let why = if cap_block {
                format!("{live}/{cap} live sessions")
            } else {
                "host memory pressure".to_string()
            };
            return Err(format!(
                "at capacity after {}s ({why}); run `tmuxlet reap` or raise TMUXLET_MAX_CONCURRENCY",
                start.elapsed().as_secs()
            ));
        }
        thread::sleep(Duration::from_millis(ACQUIRE_POLL_MS));
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct ReapCriteria {
    older_than: Option<u64>,
    blocked: bool,
}

struct ReapTarget {
    id: String,
    session: String,
    reason: &'static str,
    age_secs: Option<u64>,
}

/// Decide whether a session qualifies for reaping. Pure: blocked-state + age in,
/// reason out. Default (no flags) is conservative — blocked AND aged past the
/// grace window. --older-than reaps by age; --blocked reaps any blocked; both
/// together are OR'd.
fn reap_decision(
    blocked: bool,
    age_secs: Option<u64>,
    criteria: &ReapCriteria,
    grace_secs: u64,
) -> Option<&'static str> {
    if let Some(limit) = criteria.older_than
        && age_secs.map(|a| a >= limit).unwrap_or(false)
    {
        return Some("old");
    }
    if criteria.blocked {
        return if blocked { Some("blocked") } else { None };
    }
    if criteria.older_than.is_some() {
        return None; // explicit age filter only
    }
    if blocked && age_secs.map(|a| a >= grace_secs).unwrap_or(false) {
        Some("blocked")
    } else {
        None
    }
}

/// Age of a run from its run-dir mtime (proxy for last activity). None if absent.
fn run_age_secs(id: &str) -> Option<u64> {
    let dir = state_dir().join("runs").join(id);
    let modified = fs::metadata(&dir).ok()?.modified().ok()?;
    SystemTime::now()
        .duration_since(modified)
        .ok()
        .map(|d| d.as_secs())
}

/// Inspect every live run session and select reap targets per criteria.
fn select_reap_targets(criteria: &ReapCriteria) -> Vec<ReapTarget> {
    let mut targets = Vec::new();
    for session in list_run_sessions() {
        let id = session
            .strip_prefix("tmuxlet-")
            .unwrap_or(&session)
            .to_string();
        let age = run_age_secs(&id);
        // Only inspect the pane when blocked-state can change the outcome.
        let blocked = if criteria.blocked || criteria.older_than.is_none() {
            let pane = capture_pane(&session, 200).unwrap_or_default();
            is_execution_confirmation_blocker(&pane)
        } else {
            false
        };
        if let Some(reason) = reap_decision(blocked, age, criteria, REAP_GRACE_SECS) {
            targets.push(ReapTarget {
                id,
                session,
                reason,
                age_secs: age,
            });
        }
    }
    targets.sort_by(|a, b| a.id.cmp(&b.id));
    targets
}

fn kill_session_exact(session: &str) -> bool {
    run_tmux([
        OsStr::new("kill-session"),
        OsStr::new("-t"),
        OsStr::new(&tmux_target(session)),
    ])
    .is_ok()
}

/// Reap (kill) all sessions matching criteria; returns the count killed. Used by
/// the acquire self-heal — the standalone `reap` command lists/confirms first.
fn reap_run_sessions(criteria: &ReapCriteria) -> usize {
    select_reap_targets(criteria)
        .iter()
        .filter(|t| kill_session_exact(&t.session))
        .count()
}

fn parse_reap_args(args: &[String]) -> Result<(ReapCriteria, bool, bool), String> {
    let mut criteria = ReapCriteria::default();
    let mut yes = false;
    let mut dry_run = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-y" | "--yes" => yes = true,
            "--dry-run" => dry_run = true,
            "--blocked" => criteria.blocked = true,
            "--older-than" => {
                i += 1;
                let v = args
                    .get(i)
                    .ok_or("--older-than requires a value (seconds)")?;
                criteria.older_than = Some(v.parse::<u64>().map_err(|_| {
                    "--older-than must be an integer number of seconds".to_string()
                })?);
            }
            other => return Err(format!("unknown option for reap: {other}")),
        }
        i += 1;
    }
    Ok((criteria, yes, dry_run))
}

fn handle_reap(args: &[String]) -> Result<(), String> {
    let (criteria, yes, dry_run) = parse_reap_args(args)?;
    let targets = select_reap_targets(&criteria);
    if targets.is_empty() {
        println!("Nothing to reap.");
        return Ok(());
    }
    let verb = if dry_run { "Would reap" } else { "Found" };
    println!("{verb} {} session(s):", targets.len());
    for t in &targets {
        let age = t
            .age_secs
            .map(format_age_secs)
            .unwrap_or_else(|| "-".to_string());
        println!("  {:<10} {:<8} {}", short_id(&t.id), t.reason, age);
    }
    if dry_run {
        return Ok(());
    }
    if !yes {
        print!("Kill these session(s)? [y/N] ");
        io::stdout().flush().ok();
        let mut response = String::new();
        io::stdin()
            .read_line(&mut response)
            .map_err(|e| format!("failed to read confirmation: {e}"))?;
        if !matches!(response.trim().to_lowercase().as_str(), "y" | "yes") {
            println!("Aborted.");
            return Ok(());
        }
    }
    let killed = targets
        .iter()
        .filter(|t| kill_session_exact(&t.session))
        .count();
    println!("Reaped {killed} session(s).");
    if killed < targets.len() {
        return Err(format!(
            "failed to kill {} session(s)",
            targets.len() - killed
        ));
    }
    Ok(())
}

fn handle_send_all(args: &[String]) -> Result<(), String> {
    let mut interval = Duration::from_secs(SEND_ALL_DEFAULT_INTERVAL_SECS);
    let mut enter = false;
    let mut message_parts: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--interval" => {
                i += 1;
                let v = args.get(i).ok_or("--interval requires a value (seconds)")?;
                let secs = v
                    .parse::<f64>()
                    .map_err(|_| "--interval must be a number of seconds".to_string())?;
                if secs < 0.0 {
                    return Err("--interval must be >= 0".into());
                }
                interval = Duration::from_secs_f64(secs);
            }
            "--enter" => enter = true,
            "--" => {
                message_parts.extend(args[i + 1..].iter().cloned());
                break;
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown option for send-all: {other}"));
            }
            other => message_parts.push(other.to_string()),
        }
        i += 1;
    }
    let message = message_parts.join(" ");
    if message.is_empty() {
        enter = true; // empty message => just resume (Enter) each session
    }

    let sessions = list_run_sessions();
    if sessions.is_empty() {
        println!("No live sessions.");
        return Ok(());
    }
    let total = sessions.len();
    for (idx, session) in sessions.iter().enumerate() {
        // Pace between sessions — never fire send-keys at every session at once.
        if idx > 0 && !interval.is_zero() {
            thread::sleep(interval);
        }
        let id = session.strip_prefix("tmuxlet-").unwrap_or(session);
        let result = if enter {
            send_enter(session)
        } else {
            paste_text(session, &message)
        };
        match result {
            Ok(()) => println!("  -> {} ({}/{})", short_id(id), idx + 1, total),
            // A session that vanished mid-sweep (a run completing) is a benign no-op.
            Err(e) => eprintln!("  -- {} skipped: {e}", short_id(id)),
        }
    }
    Ok(())
}
fn handle_bridge(args: &[String]) -> Result<(), String> {
    if args.first().map(String::as_str) != Some("complete") {
        return Err("usage: tmuxlet bridge complete --run <id> --file <path>".into());
    }
    let mut run_id = None;
    let mut file = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--run" => {
                i += 1;
                run_id = args.get(i).cloned();
            }
            "--file" => {
                i += 1;
                file = args.get(i).map(PathBuf::from);
            }
            other => return Err(format!("unknown bridge option: {other}")),
        }
        i += 1;
    }
    let run_id = run_id.ok_or("--run is required")?;
    let file = file.ok_or("--file is required")?;
    let paths = run_paths(&run_id)?;
    let output =
        fs::read_to_string(&file).map_err(|e| format!("failed to read completion source: {e}"))?;
    fs::write(&paths.complete, output).map_err(|e| format!("failed to write completion: {e}"))?;
    Ok(())
}

fn handle_status() -> Result<(), String> {
    let root = state_dir().join("runs");
    if !root.exists() {
        println!("No runs.");
        return Ok(());
    }
    for entry in fs::read_dir(root).map_err(|e| format!("failed to read runs: {e}"))? {
        let entry = entry.map_err(|e| format!("failed to read run: {e}"))?;
        let meta = entry.path().join("meta.json");
        if meta.exists() {
            println!("{}", fs::read_to_string(meta).unwrap_or_default().trim());
        }
    }
    Ok(())
}

fn handle_read(args: &[String]) -> Result<(), String> {
    let id = args.first().ok_or("usage: tmuxlet read <id> [--lines N]")?;
    let mut lines = 80usize;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--lines" => {
                i += 1;
                lines = args
                    .get(i)
                    .ok_or("--lines requires a value")?
                    .parse()
                    .map_err(|_| "--lines must be an integer".to_string())?;
            }
            other => return Err(format!("unknown read option: {other}")),
        }
        i += 1;
    }
    print!("{}", capture_pane(&tmux_name_from_id(id), lines)?);
    Ok(())
}

fn handle_send(args: &[String]) -> Result<(), String> {
    let id = args.first().ok_or("usage: tmuxlet send <id> <message>")?;
    let message = args.get(1..).unwrap_or_default().join(" ");
    if message.is_empty() {
        return Err("usage: tmuxlet send <id> <message>".into());
    }
    paste_text(&tmux_name_from_id(id), &message)
}

fn handle_attach(args: &[String]) -> Result<(), String> {
    let id = args.first().ok_or("usage: tmuxlet attach <id>")?;
    let status = Command::new("tmux")
        .args(["attach-session", "-t", &tmux_target(&tmux_name_from_id(id))])
        .status()
        .map_err(|e| format!("failed to attach tmux: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("tmux attach exited with {status}"))
    }
}

fn handle_stop(args: &[String]) -> Result<(), String> {
    let id = args.first().ok_or("usage: tmuxlet stop <id>")?;
    let name = tmux_name_from_id(id);
    if tmux_session_exists(&name) {
        run_tmux([
            OsStr::new("send-keys"),
            OsStr::new("-t"),
            OsStr::new(&tmux_pane_target(&name)),
            OsStr::new("/exit"),
            OsStr::new("Enter"),
        ])
        .ok();
        thread::sleep(Duration::from_secs(2));
        if tmux_session_exists(&name) {
            run_tmux([
                OsStr::new("kill-session"),
                OsStr::new("-t"),
                OsStr::new(&tmux_target(&name)),
            ])?;
        }
    }
    println!("Stopped {id}");
    Ok(())
}

fn handle_prune(args: &[String]) -> Result<(), String> {
    let parsed = parse_prune_args(args)?;
    let runs = discover_runs();
    let selected = prune_selection(runs, &parsed.ids)?;

    if selected.is_empty() {
        println!("Nothing to prune.");
        return Ok(());
    }

    // List proposed prunes.
    let verb = if parsed.dry_run {
        "Would prune"
    } else {
        "Found"
    };
    println!("{verb} {} run(s):", selected.len());
    for rec in &selected {
        let age = rec
            .age_secs
            .map(format_age_secs)
            .unwrap_or_else(|| "-".to_string());
        let session = if rec.session_alive { "alive" } else { "gone" };
        println!(
            "  {:<10} {:<11} {:<10} (session: {})",
            short_id(&rec.id),
            rec.classification.label(),
            age,
            session
        );
    }

    if parsed.dry_run {
        return Ok(());
    }

    if !parsed.yes {
        print!("Proceed? [y/N] ");
        io::stdout().flush().ok();
        let mut response = String::new();
        io::stdin()
            .read_line(&mut response)
            .map_err(|e| format!("failed to read confirmation: {e}"))?;
        let trimmed = response.trim().to_lowercase();
        if !matches!(trimmed.as_str(), "y" | "yes") {
            println!("Aborted.");
            return Ok(());
        }
    }

    let runs_root = state_dir().join("runs");
    let outcome = execute_prune(&selected, &runs_root, false);

    println!("Pruned {} run(s).", outcome.deleted);
    if !outcome.failures.is_empty() {
        return Err(format!(
            "failed to remove {} run(s)",
            outcome.failures.len()
        ));
    }
    Ok(())
}

fn tmux_name_from_id(id: &str) -> String {
    if id.starts_with("tmuxlet-") {
        id.to_string()
    } else {
        format!("tmuxlet-{id}")
    }
}

fn emit_result(result: &RunResult) -> Result<(), String> {
    match result.completion_source.as_str() {
        "explicit" => {}
        _ if result.status == "timeout" => {
            if result.completion_source == "fallback" && matches!(result.status.as_str(), "timeout")
            {
                // Still emit the captured output for integrations; the process exit communicates failure.
            }
        }
        _ => {}
    }

    match result_status_code(&result.status) {
        0 => {
            print_result(result);
            Ok(())
        }
        _ => {
            print_result(result);
            Err(format!(
                "run {} ended with status {}",
                result.id, result.status
            ))
        }
    }
}

fn print_result(result: &RunResult) {
    if result.output_format == OutputFormat::Json {
        println!("{}", result_json(result));
    } else {
        print!("{}", result.output);
        if !result.output.ends_with('\n') {
            println!();
        }
    }
}

fn result_status_code(status: &str) -> i32 {
    if status == "completed" { 0 } else { 1 }
}

fn write_meta(
    paths: &RunPaths,
    run_id: &str,
    tmux_session: &str,
    opts: &Options,
    command: &TargetCommand,
) -> Result<(), String> {
    let meta = format!(
        "{{\"id\":\"{}\",\"target\":\"{}\",\"tmuxSession\":\"{}\",\"cwd\":\"{}\",\"program\":\"{}\",\"args\":{},\"outputFormat\":\"{}\"}}\n",
        json_escape(run_id),
        json_escape(&opts.target),
        json_escape(tmux_session),
        json_escape(&opts.cwd.display().to_string()),
        json_escape(&command.program),
        json_array(&command.args),
        match opts.output_format {
            OutputFormat::Text => "text",
            OutputFormat::Json => "json",
        },
    );
    fs::write(&paths.meta, meta).map_err(|e| format!("failed to write metadata: {e}"))?;
    Ok(())
}

fn result_json(result: &RunResult) -> String {
    format!(
        "{{\"id\":\"{}\",\"target\":\"{}\",\"status\":\"{}\",\"output\":\"{}\",\"cwd\":\"{}\",\"tmuxSession\":\"{}\",\"completionSource\":\"{}\",\"elapsedMs\":{}}}",
        json_escape(&result.id),
        json_escape(&result.target),
        json_escape(&result.status),
        json_escape(&result.output),
        json_escape(&result.cwd.display().to_string()),
        json_escape(&result.tmux_session),
        json_escape(&result.completion_source),
        result.elapsed_ms,
    )
}

fn json_array(values: &[String]) -> String {
    let items = values
        .iter()
        .map(|v| format!("\"{}\"", json_escape(v)))
        .collect::<Vec<_>>()
        .join(",");
    format!("[{items}]")
}

fn json_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn state_dir() -> PathBuf {
    env::var_os("TMUXLET_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".tmuxlet")))
        .unwrap_or_else(|| PathBuf::from(".tmuxlet"))
}

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
        root,
    })
}

fn make_run_id() -> String {
    // Process-local monotonic counter guarantees uniqueness even when two ids
    // are minted in the same clock tick: the OS clock can be coarser than
    // nanoseconds, so nanos+pid alone can collide for rapid in-process calls
    // (e.g. parallel tests, which all share one pid).
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{:x}-{:x}-{:x}", nanos, std::process::id(), seq)
}

fn expand_home(value: &str) -> PathBuf {
    if value == "~" {
        env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(value))
    } else if let Some(rest) = value.strip_prefix("~/") {
        env::var_os("HOME")
            .map(|home| PathBuf::from(home).join(rest))
            .unwrap_or_else(|| PathBuf::from(value))
    } else {
        PathBuf::from(value)
    }
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        "''".into()
    } else if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_./:=,@%+".contains(c))
    {
        value.into()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn print_help() {
    println!(
        "tmuxlet {VERSION}\n\n\
Usage:\n  tmuxlet -p [options] [prompt]\n  tmuxlet <status|read|send|send-all|attach|stop|reap|prune>\n\n\
Options:\n  -p, --print                         Blocking programmatic mode\n      --target <name>                  Target CLI: claude, gemini, codex, opencode, pi, cursor\n      --output-format <text|json>      Output format for tmuxlet result\n  -C, --cwd <dir>                      Working directory (--cd and --dir aliases)\n  -m, --model <model>                  Model pass-through where supported\n  -c, --continue                       Continue latest session where supported\n  -r, --resume [id]                    Resume session where supported\n      --session <id>                   Normalized session selector\n      --session-id <id>                Explicit normalized session id\n      --dangerously-skip-permissions   Normalized bypass flag (--yolo, --force aliases)\n      --max-concurrency <n>            Cap live sessions machine-wide (0=unlimited; env TMUXLET_MAX_CONCURRENCY; default 24)\n      --memory-pressure-guard          Queue launches under host memory pressure (env TMUXLET_MEMORY_GUARD)\n      --target-arg <arg>               Raw target CLI escape hatch\n      --timeout <seconds>              Print-mode timeout (default: 1800)\n\n\
Throttling env:\n  TMUXLET_MAX_CONCURRENCY=<n>          Live-session cap (0=unlimited). Consumers that fan out should set this low (e.g. 4).\n  TMUXLET_ACQUIRE_TIMEOUT=<seconds>   Max wait for a free slot (0=forever; default 300)\n  TMUXLET_MEMORY_MIN_FREE=<percent>   Memory-guard threshold; queue when free% drops below it (default 15)\n\n\
Maintenance:\n  tmuxlet reap [--blocked] [--older-than <s>] [--dry-run] [-y]   Kill stuck/leaked sessions, freeing slots\n  tmuxlet send-all [message] [--interval <s>] [--enter]          Paced keystrokes to every live session (no herd)\n\n\
Examples:\n  tmuxlet -p \"say ready\"\n  tmuxlet --target codex -p --cwd /tmp \"say ready\"\n  printf 'say ready' | tmuxlet -p -\n  TMUXLET_MAX_CONCURRENCY=4 tmuxlet -p \"say ready\""
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(argv: &[&str]) -> Options {
        let mut args = argv.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        parse_options(&mut args).unwrap()
    }

    fn has_pair(args: &[String], flag: &str, value: &str) -> bool {
        args.windows(2)
            .any(|window| window[0] == flag && window[1] == value)
    }

    fn with_grant(mut opts: Options) -> Options {
        opts.contract_grant = Some(ContractGrant {
            run_root: PathBuf::from("/home/u/.tmuxlet/runs/abc"),
            exe: "/usr/local/bin/tmuxlet".into(),
        });
        opts
    }

    #[test]
    fn parses_print_target_and_prompt() {
        let opts = parse(&["--target", "codex", "-p", "hello"]);
        assert!(opts.print);
        assert_eq!(opts.target, "codex");
        assert_eq!(opts.prompt_parts, vec!["hello"]);
    }

    #[test]
    fn parses_cwd_aliases_and_bypass_alias() {
        let opts = parse(&["--dir", "/tmp", "--yolo", "-p", "hello"]);
        assert_eq!(opts.cwd, PathBuf::from("/tmp"));
        assert!(opts.dangerously_skip_permissions);
    }

    #[test]
    fn parses_session_controls() {
        let opts = parse(&["--continue", "-p", "x"]);
        assert!(opts.continue_latest);

        let opts = parse(&["--resume", "-p", "x"]);
        assert_eq!(opts.resume, Some(None));

        let opts = parse(&["--resume", "abc", "-p", "x"]);
        assert_eq!(opts.resume, Some(Some("abc".into())));

        let opts = parse(&["--session", "abc", "-p", "x"]);
        assert_eq!(opts.session, Some("abc".into()));

        let opts = parse(&["--session-id", "abc", "-p", "x"]);
        assert_eq!(opts.session_id, Some("abc".into()));
    }

    #[test]
    fn rejects_conflicting_session_controls() {
        let opts = parse(&["--continue", "--resume", "abc", "-p", "x"]);
        assert!(target_command(&opts).is_err());

        let opts = parse(&["--resume", "abc", "--session-id", "def", "-p", "x"]);
        assert!(target_command(&opts).is_err());

        let opts = parse(&["--session", "abc", "--session-id", "def", "-p", "x"]);
        assert!(target_command(&opts).is_err());
    }

    #[test]
    fn claude_maps_session_controls_to_native_flags() {
        let opts = parse(&["--continue", "-p", "x"]);
        let cmd = target_command(&opts).unwrap();
        assert!(cmd.args.contains(&"--continue".to_string()));

        let opts = parse(&["--resume", "abc", "-p", "x"]);
        let cmd = target_command(&opts).unwrap();
        assert!(has_pair(&cmd.args, "--resume", "abc"));

        let opts = parse(&["--session-id", "abc", "-p", "x"]);
        let cmd = target_command(&opts).unwrap();
        assert!(has_pair(&cmd.args, "--session-id", "abc"));
    }

    #[test]
    fn claude_defaults_to_a_permission_mode() {
        // No explicit mode -> a --permission-mode flag is injected (auto by
        // default; tunable via TMUXLET_CLAUDE_PERMISSION_MODE).
        let opts = parse(&["--target", "claude", "-p", "x"]);
        let cmd = target_command(&opts).unwrap();
        assert!(cmd.args.contains(&"--permission-mode".to_string()));

        // An explicit mode is respected verbatim.
        let opts = parse(&[
            "--target",
            "claude",
            "--permission-mode",
            "default",
            "-p",
            "x",
        ]);
        let cmd = target_command(&opts).unwrap();
        assert!(has_pair(&cmd.args, "--permission-mode", "default"));

        // Full bypass takes precedence and suppresses --permission-mode.
        let opts = parse(&[
            "--target",
            "claude",
            "--dangerously-skip-permissions",
            "-p",
            "x",
        ]);
        let cmd = target_command(&opts).unwrap();
        assert!(!cmd.args.contains(&"--permission-mode".to_string()));
        assert!(
            cmd.args
                .contains(&"--dangerously-skip-permissions".to_string())
        );
    }

    #[test]
    fn claude_grant_injects_scoped_contract_permissions() {
        let opts = with_grant(parse(&["--target", "claude", "-p", "x"]));
        let cmd = target_command(&opts).unwrap();
        assert!(has_pair(
            &cmd.args,
            "--add-dir",
            "/home/u/.tmuxlet/runs/abc"
        ));
        // The contract writes are pinned with explicit allow-rules (gitignore
        // `//abs` form), not left to the auto-classifier.
        assert!(has_pair(
            &cmd.args,
            "--allowed-tools",
            "Write(//home/u/.tmuxlet/runs/abc/**)"
        ));
        assert!(has_pair(
            &cmd.args,
            "--allowed-tools",
            "Edit(//home/u/.tmuxlet/runs/abc/**)"
        ));
        assert!(has_pair(
            &cmd.args,
            "--allowed-tools",
            "Bash(/usr/local/bin/tmuxlet bridge complete:*)"
        ));
        // The auto permission-mode default still applies underneath the grant.
        assert!(cmd.args.contains(&"--permission-mode".to_string()));
    }

    #[test]
    fn full_bypass_suppresses_scoped_grant() {
        let opts = with_grant(parse(&[
            "--target",
            "claude",
            "--dangerously-skip-permissions",
            "-p",
            "x",
        ]));
        let cmd = target_command(&opts).unwrap();
        assert!(!cmd.args.contains(&"--add-dir".to_string()));
        assert!(!cmd.args.contains(&"--permission-mode".to_string()));
        assert!(
            cmd.args
                .contains(&"--dangerously-skip-permissions".to_string())
        );
    }

    #[test]
    fn codex_grant_carves_writable_root() {
        let opts = with_grant(parse(&["--target", "codex", "-p", "x"]));
        let cmd = target_command(&opts).unwrap();
        assert!(has_pair(&cmd.args, "--ask-for-approval", "never"));
        assert!(has_pair(&cmd.args, "--sandbox", "workspace-write"));
        assert!(
            cmd.args
                .iter()
                .any(|a| a
                    == "sandbox_workspace_write.writable_roots=[\"/home/u/.tmuxlet/runs/abc\"]")
        );
        assert!(
            !cmd.args
                .contains(&"--dangerously-bypass-approvals-and-sandbox".to_string())
        );
    }

    #[test]
    fn codex_full_bypass_wins_over_grant() {
        let opts = with_grant(parse(&["--target", "codex", "--yolo", "-p", "x"]));
        let cmd = target_command(&opts).unwrap();
        assert!(
            cmd.args
                .contains(&"--dangerously-bypass-approvals-and-sandbox".to_string())
        );
        assert!(!cmd.args.contains(&"--ask-for-approval".to_string()));
    }

    #[test]
    fn gemini_grant_enables_yolo() {
        let opts = with_grant(parse(&["--target", "gemini", "-p", "x"]));
        let cmd = target_command(&opts).unwrap();
        assert!(cmd.args.contains(&"--yolo".to_string()));
    }

    #[test]
    fn opencode_grant_uses_bypass_floor() {
        let opts = with_grant(parse(&["--target", "opencode", "-p", "x"]));
        let cmd = target_command(&opts).unwrap();
        assert!(
            cmd.args
                .contains(&"--dangerously-skip-permissions".to_string())
        );
    }

    #[test]
    fn cursor_grant_forces_writes() {
        let opts = with_grant(parse(&["--target", "cursor", "-p", "x"]));
        let cmd = target_command(&opts).unwrap();
        assert!(cmd.args.contains(&"--force".to_string()));
    }

    #[test]
    fn pi_grant_needs_no_injection() {
        let opts = with_grant(parse(&["--target", "pi", "-p", "x"]));
        let cmd = target_command(&opts).unwrap();
        assert_eq!(cmd.program, "pi");
    }

    #[test]
    fn gemini_maps_resume_latest() {
        let opts = parse(&["--target", "gemini", "--continue", "-p", "x"]);
        let cmd = target_command(&opts).unwrap();
        assert!(has_pair(&cmd.args, "--resume", "latest"));

        let opts = parse(&["--target", "gemini", "--resume", "-p", "x"]);
        let cmd = target_command(&opts).unwrap();
        assert!(has_pair(&cmd.args, "--resume", "latest"));

        let opts = parse(&["--target", "gemini", "--resume", "abc", "-p", "x"]);
        let cmd = target_command(&opts).unwrap();
        assert!(has_pair(&cmd.args, "--resume", "abc"));
    }

    #[test]
    fn codex_maps_session_id_to_resume() {
        let opts = parse(&["--target", "codex", "--continue", "-p", "x"]);
        let cmd = target_command(&opts).unwrap();
        assert_eq!(cmd.args[0], "resume");
        assert_eq!(cmd.args[1], "--last");

        let opts = parse(&["--target", "codex", "--resume", "abc", "-p", "x"]);
        let cmd = target_command(&opts).unwrap();
        assert_eq!(cmd.args[0], "resume");
        assert_eq!(cmd.args[1], "abc");

        let opts = parse(&["--target", "codex", "--session-id", "abc", "-p", "x"]);
        let cmd = target_command(&opts).unwrap();
        assert_eq!(cmd.args[0], "resume");
        assert_eq!(cmd.args[1], "abc");
    }

    #[test]
    fn opencode_maps_explicit_session_ids() {
        let opts = parse(&["--target", "opencode", "--continue", "-p", "x"]);
        let cmd = target_command(&opts).unwrap();
        assert!(cmd.args.contains(&"--continue".to_string()));

        let opts = parse(&["--target", "opencode", "--resume", "abc", "-p", "x"]);
        let cmd = target_command(&opts).unwrap();
        assert!(has_pair(&cmd.args, "--session", "abc"));

        let opts = parse(&["--target", "opencode", "--session-id", "abc", "-p", "x"]);
        let cmd = target_command(&opts).unwrap();
        assert!(has_pair(&cmd.args, "--session", "abc"));
    }

    #[test]
    fn pi_maps_session_id_to_session() {
        let opts = parse(&["--target", "pi", "--session-id", "abc", "-p", "x"]);
        let cmd = target_command(&opts).unwrap();
        assert!(has_pair(&cmd.args, "--session", "abc"));

        let opts = parse(&["--target", "pi", "--resume", "abc", "-p", "x"]);
        let cmd = target_command(&opts).unwrap();
        assert!(has_pair(&cmd.args, "--resume", "abc"));
    }

    #[test]
    fn cursor_maps_session_controls() {
        let opts = parse(&["--target", "cursor", "--continue", "-p", "x"]);
        let cmd = target_command(&opts).unwrap();
        assert!(cmd.args.contains(&"resume".to_string()));

        let opts = parse(&["--target", "cursor", "--session-id", "abc", "-p", "x"]);
        let cmd = target_command(&opts).unwrap();
        assert!(has_pair(&cmd.args, "--resume", "abc"));
    }

    #[test]
    fn completion_prompt_keeps_user_prompt_first() {
        let answer = PathBuf::from("/tmp/tmuxlet-answer.txt");
        let prompt = completion_prompt("user task", "run-123", &answer);
        assert!(prompt.starts_with("user task\n\nTMUXLET COMPLETION CONTRACT:"));
        assert!(prompt.contains("/tmp/tmuxlet-answer.txt"));
        assert!(prompt.contains("bridge complete --run run-123 --file /tmp/tmuxlet-answer.txt"));
    }

    #[test]
    fn claude_launch_receives_final_prompt_argument() {
        let opts = parse(&["--target", "claude", "--model", "sonnet", "-p", "x"]);
        let (cmd, prompt_in_launch) =
            command_with_prompt(target_command(&opts).unwrap(), &opts.target, "final prompt");
        assert!(prompt_in_launch);
        assert_eq!(cmd.args.last().map(String::as_str), Some("final prompt"));
        // `--` terminates option parsing right before the prompt so variadic
        // flags (e.g. --allowed-tools) cannot swallow it.
        assert_eq!(cmd.args.iter().rev().nth(1).map(String::as_str), Some("--"));
        assert!(has_pair(&cmd.args, "--model", "sonnet"));
    }

    #[test]
    fn codex_launch_receives_final_prompt_argument() {
        let opts = parse(&["--target", "codex", "--yolo", "-p", "x"]);
        let (cmd, prompt_in_launch) =
            command_with_prompt(target_command(&opts).unwrap(), &opts.target, "final prompt");
        assert!(prompt_in_launch);
        assert_eq!(cmd.args.last().map(String::as_str), Some("final prompt"));
        assert_eq!(cmd.args.iter().rev().nth(1).map(String::as_str), Some("--"));
        assert!(
            cmd.args
                .contains(&"--dangerously-bypass-approvals-and-sandbox".to_string())
        );
    }

    #[test]
    fn unsupported_prompt_argument_targets_use_paste_fallback() {
        let opts = parse(&["--target", "gemini", "-p", "x"]);
        let (cmd, prompt_in_launch) =
            command_with_prompt(target_command(&opts).unwrap(), &opts.target, "final prompt");
        assert!(!prompt_in_launch);
        assert!(!cmd.args.contains(&"final prompt".to_string()));
    }

    #[test]
    fn detects_startup_auto_enter_prompts() {
        assert!(is_startup_auto_enter_prompt("Press Enter to continue"));
        assert!(is_startup_auto_enter_prompt("Allow this command? Yes/No"));
        assert!(is_startup_auto_enter_prompt("Requires approval"));
        assert!(!is_startup_auto_enter_prompt("Here is the final answer."));
    }

    #[test]
    fn detects_execution_confirmation_blockers() {
        assert!(is_execution_confirmation_blocker(
            "Tool needs permission. Approve?"
        ));
        assert!(is_execution_confirmation_blocker(
            "Proceed with this action?"
        ));
        assert!(is_execution_confirmation_blocker("Continue? [y/n]"));
        assert!(!is_execution_confirmation_blocker(
            "The requested files were updated successfully."
        ));
    }

    #[test]
    fn launch_flag_echo_is_not_mistaken_for_a_prompt() {
        // The pane echoes the command tmuxlet typed; its scoped-grant flags
        // contain "permission"/"allow" but are not a confirmation prompt.
        let pane = "claude --permission-mode auto --add-dir /home/u/.tmuxlet/runs/abc \
             --allowed-tools 'Bash(/usr/local/bin/tmuxlet bridge complete:*)' \
             --dangerously-skip-permissions -- 'do the thing'\n❯ ";
        assert!(!is_startup_auto_enter_prompt(pane));
        assert!(!is_execution_confirmation_blocker(pane));
        // A genuine prompt below the flag echo is still detected.
        let with_real_prompt = format!("{pane}\nAllow this command? (y/n)");
        assert!(is_execution_confirmation_blocker(&with_real_prompt));
    }

    #[test]
    fn blocked_status_is_failure() {
        assert_ne!(result_status_code("blocked"), 0);
    }

    #[test]
    fn claude_maps_bypass_to_claude_flag() {
        let opts = parse(&[
            "--dangerously-skip-permissions",
            "--model",
            "sonnet",
            "-p",
            "x",
        ]);
        let cmd = claude_command(&opts).unwrap();
        assert!(
            cmd.args
                .contains(&"--dangerously-skip-permissions".to_string())
        );
        assert!(cmd.args.contains(&"sonnet".to_string()));
    }

    #[test]
    fn gemini_maps_bypass_to_yolo() {
        let opts = parse(&["--target", "gemini", "--force", "-p", "x"]);
        let cmd = gemini_command(&opts).unwrap();
        assert!(cmd.args.contains(&"--yolo".to_string()));
    }

    #[test]
    fn codex_maps_bypass_to_codex_flag() {
        let opts = parse(&["--target", "codex", "--yolo", "-p", "x"]);
        let cmd = codex_command(&opts).unwrap();
        assert!(
            cmd.args
                .contains(&"--dangerously-bypass-approvals-and-sandbox".to_string())
        );
    }

    #[test]
    fn rejects_unsupported_flags() {
        let opts = parse(&["--target", "gemini", "--system-prompt", "sys", "-p", "x"]);
        assert!(gemini_command(&opts).is_err());
    }

    #[test]
    fn shell_quote_handles_single_quotes() {
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }
    // ---------- Classification ----------

    #[test]
    fn classification_label_returns_expected_strings() {
        assert_eq!(RunClassification::Completed.label(), "completed");
        assert_eq!(RunClassification::Timeout.label(), "timeout");
        assert_eq!(RunClassification::Blocked.label(), "blocked");
        assert_eq!(RunClassification::Exited.label(), "exited");
        assert_eq!(RunClassification::InProgress.label(), "in-progress");
    }

    #[test]
    fn classification_is_terminal_for_all_except_in_progress() {
        assert!(RunClassification::Completed.is_terminal());
        assert!(RunClassification::Timeout.is_terminal());
        assert!(RunClassification::Blocked.is_terminal());
        assert!(RunClassification::Exited.is_terminal());
        assert!(!RunClassification::InProgress.is_terminal());
    }

    #[test]
    fn run_record_can_be_constructed_and_inspected() {
        let r = RunRecord {
            id: "abc".to_string(),
            classification: RunClassification::Completed,
            age_secs: Some(7),
            session_alive: false,
        };
        assert_eq!(r.id, "abc");
        assert_eq!(r.classification, RunClassification::Completed);
        assert_eq!(r.age_secs, Some(7));
        assert!(!r.session_alive);
    }

    #[test]
    fn classify_completed_when_complete_txt_present() {
        assert_eq!(
            classify_from_markers(true, None, true),
            RunClassification::Completed
        );
        assert_eq!(
            classify_from_markers(true, Some("timeout ..."), false),
            RunClassification::Completed
        );
    }

    #[test]
    fn classify_timeout_when_error_starts_with_timeout() {
        assert_eq!(
            classify_from_markers(false, Some("timeout waiting for explicit completion"), true,),
            RunClassification::Timeout
        );
    }

    #[test]
    fn classify_blocked_when_error_starts_with_blocked() {
        assert_eq!(
            classify_from_markers(
                false,
                Some("blocked waiting for a confirmation or permission prompt"),
                true,
            ),
            RunClassification::Blocked
        );
    }

    #[test]
    fn classify_blocked_for_unknown_error_content() {
        assert_eq!(
            classify_from_markers(false, Some("some other error"), true),
            RunClassification::Blocked
        );
    }

    #[test]
    fn classify_exited_when_session_dead_no_markers() {
        assert_eq!(
            classify_from_markers(false, None, false),
            RunClassification::Exited
        );
    }

    #[test]
    fn classify_in_progress_when_session_alive_no_markers() {
        assert_eq!(
            classify_from_markers(false, None, true),
            RunClassification::InProgress
        );
    }

    #[test]
    fn classify_run_returns_none_without_meta_json() {
        let dir = env::temp_dir().join(make_run_id());
        fs::create_dir_all(&dir).unwrap();
        let result = classify_run(&dir, true);
        fs::remove_dir_all(&dir).ok();
        assert_eq!(result, None);
    }

    #[test]
    fn classify_run_reads_error_first_line() {
        let dir = env::temp_dir().join(make_run_id());
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("meta.json"), "{}").unwrap();
        fs::write(
            dir.join("error.txt"),
            "timeout waiting for explicit completion\nextra context line\n",
        )
        .unwrap();
        let result = classify_run(&dir, false);
        fs::remove_dir_all(&dir).ok();
        assert_eq!(result, Some(RunClassification::Timeout));
    }

    #[test]
    fn classify_run_completed_takes_priority_over_session_state() {
        let dir = env::temp_dir().join(make_run_id());
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("meta.json"), "{}").unwrap();
        fs::write(dir.join("complete.txt"), "done").unwrap();
        let result = classify_run(&dir, false);
        fs::remove_dir_all(&dir).ok();
        assert_eq!(result, Some(RunClassification::Completed));
    }

    // ---------- discover_runs_in (spec test #8) ----------

    #[test]
    fn discover_runs_filters_terminal() {
        let root = env::temp_dir().join(make_run_id());
        fs::create_dir_all(&root).unwrap();

        // Completed
        let c = root.join("completed-aaa");
        fs::create_dir_all(&c).unwrap();
        fs::write(c.join("meta.json"), "{}").unwrap();
        fs::write(c.join("complete.txt"), "done").unwrap();

        // Timeout
        let t = root.join("timeout-bbb");
        fs::create_dir_all(&t).unwrap();
        fs::write(t.join("meta.json"), "{}").unwrap();
        fs::write(
            t.join("error.txt"),
            "timeout waiting for explicit completion\n",
        )
        .unwrap();

        // No meta.json → skipped silently
        let skipped = root.join("no-meta-ccc");
        fs::create_dir_all(&skipped).unwrap();

        let records = discover_runs_in(&root);
        fs::remove_dir_all(&root).ok();

        assert_eq!(records.len(), 2, "got: {records:?}");
        let ids: Vec<&str> = records.iter().map(|r| r.id.as_str()).collect();
        assert!(ids.contains(&"completed-aaa"));
        assert!(ids.contains(&"timeout-bbb"));
        assert!(records.iter().all(|r| r.classification.is_terminal()));
        // No heartbeat file written → age_secs is None for all records.
        assert!(records.iter().all(|r| r.age_secs.is_none()));
        // No tmux sessions exist for these fixture ids → session_alive is false.
        assert!(records.iter().all(|r| !r.session_alive));
    }

    #[test]
    fn discover_runs_in_returns_empty_for_missing_root() {
        let root = env::temp_dir().join(make_run_id());
        // Don't create it
        let records = discover_runs_in(&root);
        assert!(records.is_empty());
    }

    // ---------- parse_prune_args ----------

    fn prune_argv(args: &[&str]) -> PruneArgs {
        let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        parse_prune_args(&owned).unwrap()
    }

    #[test]
    fn parses_prune_no_args() {
        let parsed = prune_argv(&[]);
        assert!(parsed.ids.is_empty());
        assert!(!parsed.yes);
        assert!(!parsed.dry_run);
    }

    #[test]
    fn parses_prune_with_ids() {
        let parsed = prune_argv(&["abc123", "def456"]);
        assert_eq!(parsed.ids, vec!["abc123", "def456"]);
    }

    #[test]
    fn parses_prune_dry_run() {
        let parsed = prune_argv(&["--dry-run"]);
        assert!(parsed.dry_run);
        assert!(!parsed.yes);
    }

    #[test]
    fn parses_prune_yes_short_and_long() {
        assert!(prune_argv(&["-y"]).yes);
        assert!(prune_argv(&["--yes"]).yes);
    }

    #[test]
    fn parses_prune_dry_run_and_yes_together() {
        let parsed = prune_argv(&["-y", "--dry-run"]);
        assert!(parsed.yes);
        assert!(parsed.dry_run);
    }

    #[test]
    fn parses_prune_rejects_unknown_flag() {
        let owned = vec!["--no-such-flag".to_string()];
        let result = parse_prune_args(&owned);
        let msg = result.unwrap_err();
        assert!(msg.contains("--no-such-flag"), "got: {msg}");
    }

    #[test]
    fn parses_prune_mixed_ids_and_flags() {
        let parsed = prune_argv(&["abc", "--dry-run", "def"]);
        assert_eq!(parsed.ids, vec!["abc", "def"]);
        assert!(parsed.dry_run);
    }

    // ---------- prune_selection (spec test #9) ----------

    fn record(id: &str, c: RunClassification, alive: bool) -> RunRecord {
        RunRecord {
            id: id.to_string(),
            classification: c,
            age_secs: None,
            session_alive: alive,
        }
    }

    #[test]
    fn prune_selection_no_ids_keeps_only_terminal() {
        let records = vec![
            record("a-completed", RunClassification::Completed, false),
            record("b-in-progress", RunClassification::InProgress, true),
            record("c-timeout", RunClassification::Timeout, true),
            record("d-blocked", RunClassification::Blocked, true),
            record("e-exited", RunClassification::Exited, false),
        ];
        let result = prune_selection(records, &[]).unwrap();
        let ids: Vec<&str> = result.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(result.len(), 4);
        assert!(ids.contains(&"a-completed"));
        assert!(ids.contains(&"c-timeout"));
        assert!(ids.contains(&"d-blocked"));
        assert!(ids.contains(&"e-exited"));
        assert!(!ids.contains(&"b-in-progress"));
    }

    #[test]
    fn prune_rejects_non_terminal_id() {
        let records = vec![
            record("aaa", RunClassification::InProgress, true),
            record("bbb", RunClassification::Completed, false),
        ];
        let result = prune_selection(records, &["aaa".to_string()]);
        let msg = result.unwrap_err();
        assert!(msg.contains("still in progress"), "got: {msg}");
        assert!(msg.contains("aaa"), "got: {msg}");
    }

    #[test]
    fn prune_rejects_unknown_id() {
        let records = vec![record("bbb", RunClassification::Completed, false)];
        let result = prune_selection(records, &["does-not-exist".to_string()]);
        let msg = result.unwrap_err();
        assert!(msg.contains("unknown"), "got: {msg}");
        assert!(msg.contains("does-not-exist"), "got: {msg}");
    }

    #[test]
    fn prune_selection_picks_explicit_terminal_ids() {
        let records = vec![
            record("a-completed", RunClassification::Completed, false),
            record("b-in-progress", RunClassification::InProgress, true),
            record("c-timeout", RunClassification::Timeout, true),
        ];
        let result = prune_selection(
            records,
            &["a-completed".to_string(), "c-timeout".to_string()],
        )
        .unwrap();
        assert_eq!(result.len(), 2);
    }

    // ---------- execute_prune (spec test #10) ----------

    #[test]
    fn dry_run_does_not_delete() {
        let root = env::temp_dir().join(make_run_id());
        fs::create_dir_all(&root).unwrap();
        let run_a = root.join("aaa");
        fs::create_dir_all(&run_a).unwrap();
        fs::write(run_a.join("meta.json"), "{}").unwrap();
        fs::write(run_a.join("complete.txt"), "done").unwrap();

        let outcome = execute_prune(
            &[record("aaa", RunClassification::Completed, false)],
            &root,
            true, // dry_run
        );

        assert_eq!(outcome.deleted, 0);
        assert!(outcome.failures.is_empty());
        assert!(run_a.exists(), "dry-run must not delete the run dir");

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn execute_prune_deletes_when_not_dry_run() {
        let root = env::temp_dir().join(make_run_id());
        fs::create_dir_all(&root).unwrap();

        let a = root.join("aaa");
        fs::create_dir_all(&a).unwrap();
        fs::write(a.join("meta.json"), "{}").unwrap();
        fs::write(a.join("complete.txt"), "done").unwrap();

        let b = root.join("bbb");
        fs::create_dir_all(&b).unwrap();
        fs::write(b.join("meta.json"), "{}").unwrap();
        fs::write(
            b.join("error.txt"),
            "timeout waiting for explicit completion\n",
        )
        .unwrap();

        let outcome = execute_prune(
            &[
                record("aaa", RunClassification::Completed, false),
                record("bbb", RunClassification::Timeout, false),
            ],
            &root,
            false,
        );

        assert_eq!(outcome.deleted, 2);
        assert!(outcome.failures.is_empty());
        assert!(!a.exists());
        assert!(!b.exists());

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn execute_prune_reports_failures_for_missing_dirs() {
        let root = env::temp_dir().join(make_run_id());
        fs::create_dir_all(&root).unwrap();

        // ghost directory: record points to id that doesn't exist on disk
        let outcome = execute_prune(
            &[record("missing", RunClassification::Completed, false)],
            &root,
            false,
        );

        assert_eq!(outcome.deleted, 0);
        assert_eq!(outcome.failures, vec!["missing"]);

        fs::remove_dir_all(&root).ok();
    }

    // ---------- Formatting helpers ----------

    #[test]
    fn format_age_handles_seconds_minutes_hours_days() {
        assert_eq!(format_age_secs(5), "5s ago");
        assert_eq!(format_age_secs(120), "2m ago");
        assert_eq!(format_age_secs(7200), "2h ago");
        assert_eq!(format_age_secs(172800), "2d ago");
    }

    #[test]
    fn short_id_truncates_long_ids() {
        assert_eq!(short_id("abcdef0123456789"), "abcdef01");
        assert_eq!(short_id("short"), "short");
    }

    // ---------- Concurrency / throttling ----------

    #[test]
    fn tmux_target_forces_exact_match() {
        assert_eq!(tmux_target("tmuxlet-1a2b-3c4d"), "=tmuxlet-1a2b-3c4d");
        // Pane targets need the trailing `:` to delimit the session component.
        assert_eq!(tmux_pane_target("tmuxlet-1a2b-3c4d"), "=tmuxlet-1a2b-3c4d:");
    }

    #[test]
    fn make_run_id_is_unique_within_process_and_matches_grammar() {
        let a = make_run_id();
        let b = make_run_id();
        assert_ne!(a, b);
        assert!(is_run_session_name(&format!("tmuxlet-{a}")));
    }

    #[test]
    fn is_run_session_name_accepts_run_grammar() {
        assert!(is_run_session_name("tmuxlet-1a2b-3c4d")); // 2-seg (legacy ids)
        assert!(is_run_session_name("tmuxlet-1a2b3c-d4e5-6f")); // 3-seg (current)
        assert!(is_run_session_name("tmuxlet-0-0-0"));
    }

    #[test]
    fn is_run_session_name_rejects_non_run_sessions() {
        assert!(!is_run_session_name("tmuxlet-server")); // sibling daemon
        assert!(!is_run_session_name("tmuxlet-foo")); // human session
        assert!(!is_run_session_name("tmuxlet-1a2b")); // single segment
        assert!(!is_run_session_name("tmuxlet-1a2b-zz")); // non-hex segment
        assert!(!is_run_session_name("tmuxlet-1a2b-")); // trailing empty segment
        assert!(!is_run_session_name("work")); // no prefix
        assert!(!is_run_session_name("tmuxlet-")); // empty remainder
    }

    #[test]
    fn count_run_sessions_counts_only_run_sessions() {
        let out = "tmuxlet-1a2b-3c4d\ntmuxlet-server\nwork\ntmuxlet-aa-bb-cc\n";
        assert_eq!(count_run_sessions(out), 2);
        assert_eq!(count_run_sessions(""), 0); // no server => empty output
        assert_eq!(count_run_sessions("tmuxlet-1a-2b\n"), 1); // trailing newline
    }

    #[test]
    fn pick_max_concurrency_prefers_cli_then_env_then_default() {
        assert_eq!(pick_max_concurrency(Some(7), Some("3")), 7); // cli wins
        assert_eq!(pick_max_concurrency(Some(0), None), 0); // explicit unlimited
        assert_eq!(pick_max_concurrency(None, Some("3")), 3); // env
        assert_eq!(pick_max_concurrency(None, Some("0")), 0); // env unlimited
        assert_eq!(pick_max_concurrency(None, None), DEFAULT_MAX_CONCURRENCY);
        assert_eq!(
            pick_max_concurrency(None, Some("garbage")),
            DEFAULT_MAX_CONCURRENCY
        );
    }

    #[test]
    fn pick_acquire_timeout_parses_env_or_default() {
        assert_eq!(pick_acquire_timeout(Some("45")), Duration::from_secs(45));
        assert_eq!(pick_acquire_timeout(Some("0")), Duration::from_secs(0)); // forever
        assert_eq!(
            pick_acquire_timeout(None),
            Duration::from_secs(DEFAULT_ACQUIRE_TIMEOUT_SECS)
        );
        assert_eq!(
            pick_acquire_timeout(Some("x")),
            Duration::from_secs(DEFAULT_ACQUIRE_TIMEOUT_SECS)
        );
    }

    #[test]
    fn parses_max_concurrency_and_memory_guard_flags() {
        let opts = parse(&["--max-concurrency", "4", "-p", "x"]);
        assert_eq!(opts.max_concurrency, Some(4));
        let opts = parse(&["--max-concurrency", "0", "-p", "x"]);
        assert_eq!(opts.max_concurrency, Some(0));
        let opts = parse(&["--memory-pressure-guard", "-p", "x"]);
        assert!(opts.memory_guard);
        let opts = parse(&["-p", "x"]);
        assert_eq!(opts.max_concurrency, None);
        assert!(!opts.memory_guard);
    }

    #[test]
    fn max_concurrency_rejects_non_integer() {
        let mut args: Vec<String> = ["--max-concurrency", "lots", "-p", "x"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(parse_options(&mut args).is_err());
    }

    #[test]
    fn macos_free_pct_parses_memory_pressure_output() {
        let sample = "The system has 25769803776 (1572864 pages with a page size of 16384).\nSystem-wide memory free percentage: 80%";
        assert_eq!(parse_macos_free_pct(sample), Some(80));
        assert_eq!(parse_macos_free_pct("no match here"), None);
        assert_eq!(
            parse_macos_free_pct("System-wide memory free percentage: 12 %"),
            Some(12)
        );
    }

    #[test]
    fn linux_free_pct_parses_meminfo() {
        let sample = "MemTotal:       16000000 kB\nMemFree:         100000 kB\nMemAvailable:    8000000 kB\n";
        assert_eq!(parse_linux_free_pct(sample), Some(50));
        // MemAvailable absent => None (unknown => allow)
        assert_eq!(parse_linux_free_pct("MemTotal: 16000000 kB\n"), None);
    }

    #[test]
    fn under_memory_pressure_only_when_below_threshold() {
        assert!(under_memory_pressure(Some(10), 15));
        assert!(!under_memory_pressure(Some(15), 15)); // at threshold = ok
        assert!(!under_memory_pressure(Some(80), 15));
        assert!(!under_memory_pressure(None, 15)); // unknown => allow
    }

    #[test]
    fn reap_decision_default_requires_blocked_and_aged() {
        let c = ReapCriteria::default();
        assert_eq!(reap_decision(true, Some(120), &c, 60), Some("blocked"));
        assert_eq!(reap_decision(true, Some(10), &c, 60), None); // blocked but young
        assert_eq!(reap_decision(false, Some(9999), &c, 60), None); // healthy, even if old
        assert_eq!(reap_decision(true, None, &c, 60), None); // unknown age => spared
    }

    #[test]
    fn reap_decision_blocked_flag_ignores_age() {
        let c = ReapCriteria {
            older_than: None,
            blocked: true,
        };
        assert_eq!(reap_decision(true, Some(1), &c, 60), Some("blocked"));
        assert_eq!(reap_decision(false, Some(9999), &c, 60), None);
    }

    #[test]
    fn reap_decision_older_than_ignores_blocked() {
        let c = ReapCriteria {
            older_than: Some(300),
            blocked: false,
        };
        assert_eq!(reap_decision(false, Some(301), &c, 60), Some("old"));
        assert_eq!(reap_decision(true, Some(10), &c, 60), None); // young, age filter only
        assert_eq!(reap_decision(false, None, &c, 60), None);
    }

    #[test]
    fn reap_decision_both_filters_are_ored() {
        let c = ReapCriteria {
            older_than: Some(300),
            blocked: true,
        };
        assert_eq!(reap_decision(false, Some(301), &c, 60), Some("old"));
        assert_eq!(reap_decision(true, Some(10), &c, 60), Some("blocked"));
        assert_eq!(reap_decision(false, Some(10), &c, 60), None);
    }

    #[test]
    fn parses_reap_args() {
        let to_vec = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let (c, yes, dry) = parse_reap_args(&to_vec(&["--blocked", "-y"])).unwrap();
        assert!(c.blocked && yes && !dry);
        let (c, _, dry) = parse_reap_args(&to_vec(&["--older-than", "120", "--dry-run"])).unwrap();
        assert_eq!(c.older_than, Some(120));
        assert!(dry);
        assert!(parse_reap_args(&to_vec(&["--older-than", "soon"])).is_err());
        assert!(parse_reap_args(&to_vec(&["--bogus"])).is_err());
    }
}
