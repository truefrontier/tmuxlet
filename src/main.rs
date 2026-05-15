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
const HEARTBEAT_FILENAME: &str = "heartbeat";
const SETTINGS_FILENAME: &str = "settings.json";

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
    heartbeat: PathBuf,
    settings: PathBuf,
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
    validate_claude_settings_collision(&opts)?;
    ensure_tmux()?;

    let started = Instant::now();
    let run_id = make_run_id();
    let tmux_session = format!("tmuxlet-{run_id}");
    let paths = run_paths(&run_id)?;
    fs::create_dir_all(&paths.root).map_err(|e| format!("failed to create run dir: {e}"))?;

    let prompt = prompt_from(&opts)?;
    fs::write(&paths.prompt, &prompt).map_err(|e| format!("failed to write prompt: {e}"))?;

    if opts.target == "claude" {
        let exe_path = env::current_exe()
            .ok()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "tmuxlet".into());
        let settings_json = claude_hooks_settings(&run_id, &exe_path, &state_dir());
        fs::write(&paths.settings, settings_json)
            .map_err(|e| format!("failed to write settings: {e}"))?;
        opts.settings = Some(paths.settings.display().to_string());
    }

    let final_prompt = if needs_completion_contract(&opts.target) {
        completion_prompt(&prompt, &run_id, &paths.answer)
    } else {
        prompt.clone()
    };
    let base_command = target_command(&opts)?;
    let (launch_command, prompt_in_launch) =
        command_with_prompt(base_command.clone(), &opts.target, &final_prompt);

    write_meta(&paths, &run_id, &tmux_session, &opts, &base_command)?;
    start_tmux_session(&tmux_session, &opts.cwd)?;
    send_launch_command(&tmux_session, &launch_command)?;
    if !prompt_in_launch {
        paste_text(&tmux_session, &final_prompt)?;
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

fn needs_completion_contract(target: &str) -> bool {
    target != "claude"
}

fn validate_claude_settings_collision(opts: &Options) -> Result<(), String> {
    if opts.target == "claude" && opts.settings.is_some() {
        Err(
            "claude target: --settings collides with tmuxlet auto-hooks; \
             use --target-arg --settings <path> to bypass"
                .into(),
        )
    } else {
        Ok(())
    }
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

fn claude_hooks_settings(run_id: &str, exe_path: &str, state_dir: &Path) -> String {
    let quoted_exe = shell_quote(exe_path);
    let quoted_state_dir = shell_quote(&state_dir.display().to_string());
    // Bake TMUXLET_HOME into the hook command so the bridge resolves the same
    // state directory tmuxlet is using, regardless of how tmux/Claude inherit env.
    let env_prefix = format!("TMUXLET_HOME={quoted_state_dir} ");
    let stop_cmd = format!("{env_prefix}{quoted_exe} bridge stop-hook --run {run_id}");
    let tool_cmd = format!("{env_prefix}{quoted_exe} bridge tool-hook --run {run_id}");
    format!(
        "{{\"hooks\":{{\
\"Stop\":[{{\"hooks\":[{{\"type\":\"command\",\"command\":\"{stop}\"}}]}}],\
\"PostToolUse\":[{{\"matcher\":\"*\",\"hooks\":[{{\"type\":\"command\",\"command\":\"{tool}\"}}]}}]\
}}}}\n",
        stop = json_escape(&stop_cmd),
        tool = json_escape(&tool_cmd),
    )
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
    match args.first().map(String::as_str) {
        Some("complete") => handle_bridge_complete(&args[1..]),
        Some("stop-hook") => handle_bridge_stop_hook(&args[1..]),
        Some("tool-hook") => handle_bridge_tool_hook(&args[1..]),
        _ => Err(
            "usage: tmuxlet bridge <complete|stop-hook|tool-hook> --run <id> [--file <path>]"
                .into(),
        ),
    }
}

fn handle_bridge_complete(args: &[String]) -> Result<(), String> {
    let mut run_id = None;
    let mut file = None;
    let mut i = 0;
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
            other => return Err(format!("unknown bridge complete option: {other}")),
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

fn handle_bridge_stop_hook(args: &[String]) -> Result<(), String> {
    let mut run_id = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--run" => {
                i += 1;
                run_id = args.get(i).cloned();
            }
            other => return Err(format!("unknown bridge stop-hook option: {other}")),
        }
        i += 1;
    }
    let run_id = run_id.ok_or("--run is required")?;
    let paths = run_paths(&run_id)?;

    let mut stdin_buf = String::new();
    io::stdin()
        .read_to_string(&mut stdin_buf)
        .map_err(|e| format!("failed to read stdin: {e}"))?;
    let transcript_path = extract_top_level_string(&stdin_buf, "transcript_path");

    let text = match transcript_path {
        Some(p) => extract_final_assistant_text(Path::new(&p)).unwrap_or_default(),
        None => String::new(),
    };

    write_complete_once(&paths.complete, &text)
}

fn handle_bridge_tool_hook(args: &[String]) -> Result<(), String> {
    let mut run_id: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        if args[i].as_str() == "--run" {
            i += 1;
            run_id = args.get(i).cloned();
        }
        i += 1;
    }
    if let Some(run_id) = run_id
        && let Ok(paths) = run_paths(&run_id)
    {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or_default();
        let _ = fs::write(&paths.heartbeat, now_ms.to_string());
    }
    Ok(())
}

fn write_complete_once(path: &Path, text: &str) -> Result<(), String> {
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut f) => f
            .write_all(text.as_bytes())
            .map_err(|e| format!("failed to write completion: {e}")),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(format!("failed to open completion: {e}")),
    }
}

fn handle_status() -> Result<(), String> {
    let root = state_dir().join("runs");
    if !root.exists() {
        println!("No runs.");
        return Ok(());
    }
    for entry in fs::read_dir(root).map_err(|e| format!("failed to read runs: {e}"))? {
        let entry = entry.map_err(|e| format!("failed to read run: {e}"))?;
        let run_dir = entry.path();
        let meta = run_dir.join("meta.json");
        if !meta.exists() {
            continue;
        }
        println!("{}", fs::read_to_string(&meta).unwrap_or_default().trim());
        let heartbeat = run_dir.join(HEARTBEAT_FILENAME);
        let complete = run_dir.join("complete.txt");
        if heartbeat.exists()
            && !complete.exists()
            && let Some(ago) = heartbeat_age_secs(&heartbeat)
        {
            println!("last activity: {ago}s ago");
        }
    }
    Ok(())
}

fn heartbeat_age_secs(path: &Path) -> Option<u64> {
    let mtime = fs::metadata(path).ok()?.modified().ok()?;
    SystemTime::now()
        .duration_since(mtime)
        .ok()
        .map(|d| d.as_secs())
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

struct JsonScanner<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> JsonScanner<'a> {
    fn new(s: &'a str) -> Self {
        Self {
            bytes: s.as_bytes(),
            pos: 0,
        }
    }

    fn skip_ws(&mut self) {
        while let Some(&b) = self.bytes.get(self.pos) {
            if matches!(b, b' ' | b'\t' | b'\n' | b'\r') {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.pos += 1;
        Some(b)
    }

    fn expect(&mut self, want: u8) -> Result<(), ()> {
        self.skip_ws();
        if self.bump() == Some(want) {
            Ok(())
        } else {
            Err(())
        }
    }

    fn parse_string(&mut self) -> Result<String, ()> {
        self.skip_ws();
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            match self.bump().ok_or(())? {
                b'"' => return Ok(out),
                b'\\' => match self.bump().ok_or(())? {
                    b'"' => out.push('"'),
                    b'\\' => out.push('\\'),
                    b'/' => out.push('/'),
                    b'n' => out.push('\n'),
                    b'r' => out.push('\r'),
                    b't' => out.push('\t'),
                    b'b' => out.push('\u{08}'),
                    b'f' => out.push('\u{0C}'),
                    b'u' => {
                        let mut hex = [0u8; 4];
                        for slot in &mut hex {
                            *slot = self.bump().ok_or(())?;
                        }
                        let s_hex = std::str::from_utf8(&hex).map_err(|_| ())?;
                        let cp = u32::from_str_radix(s_hex, 16).map_err(|_| ())?;
                        if let Some(c) = char::from_u32(cp) {
                            out.push(c);
                        }
                    }
                    _ => return Err(()),
                },
                b => {
                    if b < 0x80 {
                        out.push(b as char);
                    } else {
                        let extra = if b >= 0xF0 {
                            3
                        } else if b >= 0xE0 {
                            2
                        } else {
                            1
                        };
                        let start = self.pos - 1;
                        for _ in 0..extra {
                            self.bump().ok_or(())?;
                        }
                        let chunk =
                            std::str::from_utf8(&self.bytes[start..self.pos]).map_err(|_| ())?;
                        out.push_str(chunk);
                    }
                }
            }
        }
    }

    fn skip_value(&mut self) -> Result<(), ()> {
        self.skip_ws();
        match self.peek().ok_or(())? {
            b'"' => self.parse_string().map(|_| ()),
            b'{' => self.skip_object(),
            b'[' => self.skip_array(),
            b't' => {
                if self.bytes[self.pos..].starts_with(b"true") {
                    self.pos += 4;
                    Ok(())
                } else {
                    Err(())
                }
            }
            b'f' => {
                if self.bytes[self.pos..].starts_with(b"false") {
                    self.pos += 5;
                    Ok(())
                } else {
                    Err(())
                }
            }
            b'n' => {
                if self.bytes[self.pos..].starts_with(b"null") {
                    self.pos += 4;
                    Ok(())
                } else {
                    Err(())
                }
            }
            b'-' | b'0'..=b'9' => {
                self.skip_number();
                Ok(())
            }
            _ => Err(()),
        }
    }

    fn skip_object(&mut self) -> Result<(), ()> {
        self.expect(b'{')?;
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.bump();
            return Ok(());
        }
        loop {
            self.parse_string()?;
            self.skip_ws();
            self.expect(b':')?;
            self.skip_value()?;
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.bump();
                    self.skip_ws();
                }
                Some(b'}') => {
                    self.bump();
                    return Ok(());
                }
                _ => return Err(()),
            }
        }
    }

    fn skip_array(&mut self) -> Result<(), ()> {
        self.expect(b'[')?;
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.bump();
            return Ok(());
        }
        loop {
            self.skip_value()?;
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.bump();
                    self.skip_ws();
                }
                Some(b']') => {
                    self.bump();
                    return Ok(());
                }
                _ => return Err(()),
            }
        }
    }

    fn skip_number(&mut self) {
        while let Some(b) = self.peek() {
            if matches!(b, b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9') {
                self.bump();
            } else {
                break;
            }
        }
    }
}

fn extract_top_level_string(json: &str, target_key: &str) -> Option<String> {
    let mut s = JsonScanner::new(json);
    s.skip_ws();
    s.expect(b'{').ok()?;
    s.skip_ws();
    if s.peek() == Some(b'}') {
        return None;
    }
    loop {
        let key = s.parse_string().ok()?;
        s.skip_ws();
        s.expect(b':').ok()?;
        s.skip_ws();
        if key == target_key {
            return s.parse_string().ok();
        }
        s.skip_value().ok()?;
        s.skip_ws();
        match s.peek()? {
            b',' => {
                s.bump();
                s.skip_ws();
            }
            b'}' => return None,
            _ => return None,
        }
    }
}

fn extract_final_assistant_text(transcript_path: &Path) -> Result<String, String> {
    // Claude can fire the Stop hook before flushing the final assistant message
    // to the transcript file. Poll until the current-turn assistant text lands,
    // the transcript stops growing for a grace window, or the safety cap hits.
    let hard_deadline = Instant::now() + max_retry_secs();
    let idle_grace = Duration::from_secs(2);
    let mut last_size: usize = 0;
    let mut idle_since = Instant::now();
    loop {
        let content = fs::read_to_string(transcript_path)
            .map_err(|e| format!("failed to read transcript: {e}"))?;
        if let Ok(text) = extract_current_turn_assistant_text_from_str(&content) {
            return Ok(text);
        }
        let current_size = content.len();
        if current_size != last_size {
            last_size = current_size;
            idle_since = Instant::now();
        }
        let now = Instant::now();
        if now.duration_since(idle_since) > idle_grace {
            return Err("transcript idle, no current-turn assistant text".into());
        }
        if now >= hard_deadline {
            return Err("retry deadline exceeded".into());
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn max_retry_secs() -> Duration {
    parse_max_retry_secs(env::var("TMUXLET_HOOK_RETRY_SECS").ok().as_deref())
}

fn parse_max_retry_secs(raw: Option<&str>) -> Duration {
    const DEFAULT_SECS: u64 = 30;
    const MIN_SECS: u64 = 1;
    const MAX_SECS: u64 = 600;
    let secs = raw
        .and_then(|s| s.parse::<u64>().ok())
        .map(|n| n.clamp(MIN_SECS, MAX_SECS))
        .unwrap_or(DEFAULT_SECS);
    Duration::from_secs(secs)
}

fn extract_current_turn_assistant_text_from_str(content: &str) -> Result<String, String> {
    let mut seen_user = false;
    let mut best: Option<String> = None;
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if is_main_thread_user_line(line) {
            seen_user = true;
            best = None;
            continue;
        }
        if !seen_user {
            continue;
        }
        let texts = extract_assistant_texts_from_line(line);
        if !texts.is_empty() {
            best = Some(texts.join("\n"));
        }
    }
    best.ok_or_else(|| "no assistant text after last user message".into())
}

fn is_main_thread_user_line(line: &str) -> bool {
    line.contains("\"type\":\"user\"") && !line.contains("\"isSidechain\":true")
}

fn extract_assistant_texts_from_line(line: &str) -> Vec<String> {
    let mut s = JsonScanner::new(line);
    try_extract_assistant_texts(&mut s).unwrap_or_default()
}

fn try_extract_assistant_texts(s: &mut JsonScanner) -> Result<Vec<String>, ()> {
    s.skip_ws();
    s.expect(b'{')?;
    s.skip_ws();
    if s.peek() == Some(b'}') {
        return Ok(Vec::new());
    }

    let mut is_assistant = false;
    let mut is_sidechain = false;
    let mut texts: Vec<String> = Vec::new();

    loop {
        let key = s.parse_string()?;
        s.skip_ws();
        s.expect(b':')?;
        s.skip_ws();
        match key.as_str() {
            "type" => is_assistant = s.parse_string()? == "assistant",
            "isSidechain" => {
                s.skip_ws();
                let rest = &s.bytes[s.pos..];
                if rest.starts_with(b"true") {
                    is_sidechain = true;
                    s.pos += 4;
                } else if rest.starts_with(b"false") {
                    s.pos += 5;
                } else {
                    s.skip_value()?;
                }
            }
            "message" => texts = parse_message_text_blocks(s)?,
            _ => s.skip_value()?,
        }
        s.skip_ws();
        match s.peek() {
            Some(b',') => {
                s.bump();
                s.skip_ws();
            }
            Some(b'}') => {
                s.bump();
                break;
            }
            _ => return Err(()),
        }
    }

    if is_assistant && !is_sidechain {
        Ok(texts)
    } else {
        Ok(Vec::new())
    }
}

fn parse_message_text_blocks(s: &mut JsonScanner) -> Result<Vec<String>, ()> {
    s.skip_ws();
    s.expect(b'{')?;
    s.skip_ws();
    if s.peek() == Some(b'}') {
        s.bump();
        return Ok(Vec::new());
    }

    let mut texts: Vec<String> = Vec::new();

    loop {
        let key = s.parse_string()?;
        s.skip_ws();
        s.expect(b':')?;
        s.skip_ws();
        if key == "content" {
            texts = parse_content_array(s)?;
        } else {
            s.skip_value()?;
        }
        s.skip_ws();
        match s.peek() {
            Some(b',') => {
                s.bump();
                s.skip_ws();
            }
            Some(b'}') => {
                s.bump();
                break;
            }
            _ => return Err(()),
        }
    }
    Ok(texts)
}

fn parse_content_array(s: &mut JsonScanner) -> Result<Vec<String>, ()> {
    let mut texts = Vec::new();
    s.expect(b'[')?;
    s.skip_ws();
    if s.peek() == Some(b']') {
        s.bump();
        return Ok(texts);
    }
    loop {
        if let Some(t) = parse_content_block(s)? {
            texts.push(t);
        }
        s.skip_ws();
        match s.peek() {
            Some(b',') => {
                s.bump();
                s.skip_ws();
            }
            Some(b']') => {
                s.bump();
                break;
            }
            _ => return Err(()),
        }
    }
    Ok(texts)
}

fn parse_content_block(s: &mut JsonScanner) -> Result<Option<String>, ()> {
    s.expect(b'{')?;
    s.skip_ws();
    if s.peek() == Some(b'}') {
        s.bump();
        return Ok(None);
    }

    let mut block_type: Option<String> = None;
    let mut text_value: Option<String> = None;

    loop {
        let key = s.parse_string()?;
        s.skip_ws();
        s.expect(b':')?;
        s.skip_ws();
        match key.as_str() {
            "type" => block_type = Some(s.parse_string()?),
            "text" => text_value = Some(s.parse_string()?),
            _ => s.skip_value()?,
        }
        s.skip_ws();
        match s.peek() {
            Some(b',') => {
                s.bump();
                s.skip_ws();
            }
            Some(b'}') => {
                s.bump();
                break;
            }
            _ => return Err(()),
        }
    }

    if block_type.as_deref() == Some("text") {
        Ok(text_value)
    } else {
        Ok(None)
    }
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
        heartbeat: root.join(HEARTBEAT_FILENAME),
        settings: root.join(SETTINGS_FILENAME),
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
Usage:\n  tmuxlet -p [options] [prompt]\n  tmuxlet <status|read|send|attach|stop>\n  tmuxlet bridge <complete|stop-hook|tool-hook> --run <id> [...]\n\n\
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

    #[test]
    fn claude_hooks_settings_is_deterministic() {
        let dir = PathBuf::from("/home/user/.tmuxlet");
        let a = claude_hooks_settings("abc-123", "/path/tmuxlet", &dir);
        let b = claude_hooks_settings("abc-123", "/path/tmuxlet", &dir);
        assert_eq!(a, b);
    }

    #[test]
    fn claude_hooks_settings_handles_path_with_spaces() {
        let dir = PathBuf::from("/state with spaces/tmuxlet");
        let s = claude_hooks_settings("run-id", "/path with spaces/tmuxlet", &dir);
        assert!(s.contains("'/path with spaces/tmuxlet' bridge stop-hook --run run-id"));
        assert!(s.contains("'/path with spaces/tmuxlet' bridge tool-hook --run run-id"));
        assert!(s.contains("TMUXLET_HOME='/state with spaces/tmuxlet'"));
    }

    #[test]
    fn claude_hooks_settings_contains_both_events() {
        let dir = PathBuf::from("/home/user/.tmuxlet");
        let s = claude_hooks_settings("abc", "/usr/local/bin/tmuxlet", &dir);
        assert!(s.contains("\"Stop\""));
        assert!(s.contains("\"PostToolUse\""));
        assert!(s.contains("\"matcher\":\"*\""));
        assert!(s.contains("bridge stop-hook --run abc"));
        assert!(s.contains("bridge tool-hook --run abc"));
        assert!(s.contains("TMUXLET_HOME=/home/user/.tmuxlet"));
    }

    #[test]
    fn needs_completion_contract_excludes_claude() {
        assert!(!needs_completion_contract("claude"));
        assert!(needs_completion_contract("codex"));
        assert!(needs_completion_contract("gemini"));
        assert!(needs_completion_contract("opencode"));
        assert!(needs_completion_contract("pi"));
        assert!(needs_completion_contract("cursor"));
    }

    #[test]
    fn claude_target_rejects_user_settings_collision() {
        let mut opts = Options::default();
        opts.settings = Some("/tmp/foo.json".into());
        assert!(validate_claude_settings_collision(&opts).is_err());

        let opts = Options::default();
        assert!(validate_claude_settings_collision(&opts).is_ok());

        let mut opts = Options::default();
        opts.target = "codex".into();
        opts.settings = Some("/tmp/foo.json".into());
        assert!(validate_claude_settings_collision(&opts).is_ok());
    }

    #[test]
    fn extract_top_level_string_basic() {
        let s = r#"{"a":1,"transcript_path":"/path/to/file.jsonl","b":true}"#;
        assert_eq!(
            extract_top_level_string(s, "transcript_path"),
            Some("/path/to/file.jsonl".into())
        );
        assert_eq!(extract_top_level_string(s, "missing"), None);
    }

    #[test]
    fn extract_top_level_string_handles_escapes() {
        let s = r#"{"transcript_path":"/path with \"quote\" and \\back\\.jsonl"}"#;
        assert_eq!(
            extract_top_level_string(s, "transcript_path"),
            Some("/path with \"quote\" and \\back\\.jsonl".into())
        );
    }

    #[test]
    fn extract_final_assistant_text_basic() {
        let content = concat!(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}"#,
            "\n",
            r#"{"type":"assistant","isSidechain":false,"message":{"role":"assistant","content":[{"type":"text","text":"hello there"}]}}"#,
            "\n",
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"again"}]}}"#,
            "\n",
            r#"{"type":"assistant","isSidechain":false,"message":{"role":"assistant","content":[{"type":"text","text":"the final answer"}]}}"#,
        );
        let out = extract_current_turn_assistant_text_from_str(content).unwrap();
        assert_eq!(out, "the final answer");
    }

    #[test]
    fn extract_final_assistant_text_walks_past_tool_use() {
        let content = concat!(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}"#,
            "\n",
            r#"{"type":"assistant","isSidechain":false,"message":{"role":"assistant","content":[{"type":"tool_use","name":"Read","input":{"file_path":"/x"}}]}}"#,
            "\n",
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"abc"}]}}"#,
            "\n",
            r#"{"type":"assistant","isSidechain":false,"message":{"role":"assistant","content":[{"type":"text","text":"the real answer"}]}}"#,
        );
        let out = extract_current_turn_assistant_text_from_str(content).unwrap();
        assert_eq!(out, "the real answer");
    }

    #[test]
    fn extract_final_assistant_text_ignores_sidechain() {
        let content = concat!(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}"#,
            "\n",
            r#"{"type":"assistant","isSidechain":false,"message":{"role":"assistant","content":[{"type":"text","text":"main thread answer"}]}}"#,
            "\n",
            r#"{"type":"assistant","isSidechain":true,"message":{"role":"assistant","content":[{"type":"text","text":"subagent answer"}]}}"#,
        );
        let out = extract_current_turn_assistant_text_from_str(content).unwrap();
        assert_eq!(out, "main thread answer");
    }

    #[test]
    fn extract_final_assistant_text_skips_thinking() {
        let content = concat!(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}"#,
            "\n",
            r#"{"type":"assistant","isSidechain":false,"message":{"role":"assistant","content":[{"type":"thinking","thinking":"let me think"},{"type":"text","text":"my answer"}]}}"#,
        );
        let out = extract_current_turn_assistant_text_from_str(content).unwrap();
        assert_eq!(out, "my answer");
    }

    #[test]
    fn extract_final_assistant_text_errors_when_no_text() {
        let content =
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}"#;
        assert!(extract_current_turn_assistant_text_from_str(content).is_err());
    }

    #[test]
    fn extract_final_assistant_text_skips_malformed_lines() {
        let content = concat!(
            "not json at all\n",
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}"#,
            "\n",
            r#"{"type":"assistant","isSidechain":false,"message":{"role":"assistant","content":[{"type":"text","text":"survived"}]}}"#,
            "\n",
            "{broken json"
        );
        let out = extract_current_turn_assistant_text_from_str(content).unwrap();
        assert_eq!(out, "survived");
    }

    #[test]
    fn extract_final_assistant_text_handles_unicode_text() {
        let content = concat!(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}"#,
            "\n",
            r#"{"type":"assistant","isSidechain":false,"message":{"role":"assistant","content":[{"type":"text","text":"café — naïve résumé 你好"}]}}"#,
        );
        let out = extract_current_turn_assistant_text_from_str(content).unwrap();
        assert_eq!(out, "café — naïve résumé 你好");
    }

    #[test]
    fn extract_current_turn_skips_prior_turn_assistant() {
        let content = concat!(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"first"}]}}"#,
            "\n",
            r#"{"type":"assistant","isSidechain":false,"message":{"role":"assistant","content":[{"type":"text","text":"old"}]}}"#,
            "\n",
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"second"}]}}"#,
            "\n",
            r#"{"type":"assistant","isSidechain":false,"message":{"role":"assistant","content":[{"type":"text","text":"new"}]}}"#,
        );
        let out = extract_current_turn_assistant_text_from_str(content).unwrap();
        assert_eq!(out, "new");
    }

    #[test]
    fn extract_current_turn_rejects_when_new_turn_assistant_missing() {
        let content = concat!(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"first"}]}}"#,
            "\n",
            r#"{"type":"assistant","isSidechain":false,"message":{"role":"assistant","content":[{"type":"text","text":"old"}]}}"#,
            "\n",
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"second"}]}}"#,
        );
        assert!(extract_current_turn_assistant_text_from_str(content).is_err());
    }

    #[test]
    fn extract_current_turn_handles_tool_use_within_turn() {
        let content = concat!(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"do the thing"}]}}"#,
            "\n",
            r#"{"type":"assistant","isSidechain":false,"message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash","input":{"command":"ls"}}]}}"#,
            "\n",
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"abc","content":"file.txt"}]}}"#,
            "\n",
            r#"{"type":"assistant","isSidechain":false,"message":{"role":"assistant","content":[{"type":"text","text":"answer"}]}}"#,
        );
        let out = extract_current_turn_assistant_text_from_str(content).unwrap();
        assert_eq!(out, "answer");
    }

    #[test]
    fn is_main_thread_user_line_filters_sidechain() {
        let main_user = r#"{"type":"user","message":{"role":"user","content":[]}}"#;
        let sidechain_user =
            r#"{"type":"user","isSidechain":true,"message":{"role":"user","content":[]}}"#;
        let assistant = r#"{"type":"assistant","message":{"role":"assistant","content":[]}}"#;
        assert!(is_main_thread_user_line(main_user));
        assert!(!is_main_thread_user_line(sidechain_user));
        assert!(!is_main_thread_user_line(assistant));
    }

    #[test]
    fn max_retry_secs_default_when_unset() {
        assert_eq!(parse_max_retry_secs(None), Duration::from_secs(30));
    }

    #[test]
    fn max_retry_secs_env_override() {
        assert_eq!(parse_max_retry_secs(Some("10")), Duration::from_secs(10));
        assert_eq!(parse_max_retry_secs(Some("120")), Duration::from_secs(120));
    }

    #[test]
    fn max_retry_secs_clamps_invalid_input() {
        assert_eq!(
            parse_max_retry_secs(Some("garbage")),
            Duration::from_secs(30)
        );
        assert_eq!(parse_max_retry_secs(Some("")), Duration::from_secs(30));
        assert_eq!(parse_max_retry_secs(Some("0")), Duration::from_secs(1));
        assert_eq!(
            parse_max_retry_secs(Some("99999")),
            Duration::from_secs(600)
        );
    }

    #[test]
    fn write_complete_once_is_idempotent() {
        let dir = env::temp_dir().join(format!("tmuxlet-test-{}", make_run_id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("complete.txt");

        write_complete_once(&path, "first write").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "first write");

        write_complete_once(&path, "second write").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "first write");

        fs::remove_dir_all(&dir).ok();
    }
}
