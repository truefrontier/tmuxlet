use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_TARGET: &str = "claude";
const DEFAULT_TIMEOUT_SECS: u64 = 30 * 60;
const STARTUP_AUTO_ENTER_WINDOW_SECS: u64 = 8;
const STARTUP_AUTO_ENTER_MAX_ATTEMPTS: usize = 3;
const EXECUTION_STALL_SECS: u64 = 60;
const PANE_POLL_SECS: u64 = 1;

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
    timeout: Duration,
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
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
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
                OsStr::new(&session_name),
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

fn run_print(opts: Options) -> Result<RunResult, String> {
    ensure_tmux()?;
    let started = Instant::now();
    let run_id = make_run_id();
    let tmux_session = format!("tmuxlet-{run_id}");
    let paths = run_paths(&run_id)?;
    fs::create_dir_all(&paths.root).map_err(|e| format!("failed to create run dir: {e}"))?;

    let prompt = prompt_from(&opts)?;
    fs::write(&paths.prompt, &prompt).map_err(|e| format!("failed to write prompt: {e}"))?;
    let instruction = completion_prompt(&prompt, &run_id, &paths.answer);
    let base_command = target_command(&opts)?;
    let (launch_command, prompt_in_launch) =
        command_with_prompt(base_command.clone(), &opts.target, &instruction);

    write_meta(&paths, &run_id, &tmux_session, &opts, &base_command)?;
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
                OsStr::new(&tmux_session),
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
    push_opt(&mut args, "--permission-mode", &opts.permission_mode);
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
    if opts.dangerously_skip_permissions {
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
    if opts.dangerously_skip_permissions {
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
    if opts.dangerously_skip_permissions {
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

fn completion_prompt(prompt: &str, run_id: &str, answer_path: &Path) -> String {
    let exe = env::current_exe()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "tmuxlet".into());
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

fn send_launch_command(tmux_session: &str, command: &TargetCommand) -> Result<(), String> {
    let mut parts = Vec::with_capacity(command.args.len() + 1);
    parts.push(shell_quote(&command.program));
    for arg in &command.args {
        parts.push(shell_quote(arg));
    }
    let line = parts.join(" ");
    run_tmux([
        OsStr::new("send-keys"),
        OsStr::new("-t"),
        OsStr::new(tmux_session),
        OsStr::new(&line),
        OsStr::new("Enter"),
    ])
    .map(|_| ())
}

fn paste_text(tmux_session: &str, text: &str) -> Result<(), String> {
    let tmp = env::temp_dir().join(format!("tmuxlet-paste-{}.txt", make_run_id()));
    fs::write(&tmp, text).map_err(|e| format!("failed to stage prompt: {e}"))?;
    let load = run_tmux([OsStr::new("load-buffer"), tmp.as_os_str()]);
    fs::remove_file(&tmp).ok();
    load?;
    run_tmux([
        OsStr::new("paste-buffer"),
        OsStr::new("-t"),
        OsStr::new(tmux_session),
    ])?;
    thread::sleep(Duration::from_millis(200));
    run_tmux([
        OsStr::new("send-keys"),
        OsStr::new("-t"),
        OsStr::new(tmux_session),
        OsStr::new("Enter"),
    ])
    .map(|_| ())
}

fn send_enter(tmux_session: &str) -> Result<(), String> {
    run_tmux([
        OsStr::new("send-keys"),
        OsStr::new("-t"),
        OsStr::new(tmux_session),
        OsStr::new("Enter"),
    ])
    .map(|_| ())
}

fn is_startup_auto_enter_prompt(pane: &str) -> bool {
    let text = pane.to_ascii_lowercase();
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
    let text = pane.to_ascii_lowercase();
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
        .args(["has-session", "-t", name])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn capture_pane(tmux_session: &str, lines: usize) -> Result<String, String> {
    let start = format!("-{lines}");
    run_tmux([
        OsStr::new("capture-pane"),
        OsStr::new("-t"),
        OsStr::new(tmux_session),
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
        .args(["attach-session", "-t", &tmux_name_from_id(id)])
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
            OsStr::new(&name),
            OsStr::new("/exit"),
            OsStr::new("Enter"),
        ])
        .ok();
        thread::sleep(Duration::from_secs(2));
        if tmux_session_exists(&name) {
            run_tmux([
                OsStr::new("kill-session"),
                OsStr::new("-t"),
                OsStr::new(&name),
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
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{:x}-{:x}", nanos, std::process::id())
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
Usage:\n  tmuxlet -p [options] [prompt]\n  tmuxlet <status|read|send|attach|stop|prune>\n\n\
Options:\n  -p, --print                         Blocking programmatic mode\n      --target <name>                  Target CLI: claude, gemini, codex, opencode, pi, cursor\n      --output-format <text|json>      Output format for tmuxlet result\n  -C, --cwd <dir>                      Working directory (--cd and --dir aliases)\n  -m, --model <model>                  Model pass-through where supported\n  -c, --continue                       Continue latest session where supported\n  -r, --resume [id]                    Resume session where supported\n      --session <id>                   Normalized session selector\n      --session-id <id>                Explicit normalized session id\n      --dangerously-skip-permissions   Normalized bypass flag (--yolo, --force aliases)\n      --target-arg <arg>               Raw target CLI escape hatch\n      --timeout <seconds>              Print-mode timeout (default: 1800)\n\n\
Examples:\n  tmuxlet -p \"say ready\"\n  tmuxlet --target codex -p --cwd /tmp \"say ready\"\n  printf 'say ready' | tmuxlet -p -"
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
        assert!(has_pair(&cmd.args, "--model", "sonnet"));
    }

    #[test]
    fn codex_launch_receives_final_prompt_argument() {
        let opts = parse(&["--target", "codex", "--yolo", "-p", "x"]);
        let (cmd, prompt_in_launch) =
            command_with_prompt(target_command(&opts).unwrap(), &opts.target, "final prompt");
        assert!(prompt_in_launch);
        assert_eq!(cmd.args.last().map(String::as_str), Some("final prompt"));
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
}
