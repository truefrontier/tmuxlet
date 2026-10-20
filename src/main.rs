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
// How long a codex run is watched for startup dialogs, unless its chat UI shows
// first. Later stalls fall to the generic blocked-pane check.
const CODEX_STARTUP_WINDOW_SECS: u64 = 60;
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
// --file/--attach: text inlined into the prompt is capped in raw bytes. The total stays under macOS's 1 MiB
// ARG_MAX because claude/codex/grok/cursor/agy get the whole prompt as one launch argument.
// ponytail: Linux caps one argument at 128 KiB (MAX_ARG_STRLEN); lower INLINE_TOTAL_MAX_BYTES there if needed.
const INLINE_FILE_MAX_BYTES: u64 = 256 * 1024;
const INLINE_TOTAL_MAX_BYTES: u64 = 768 * 1024;
// Bytes read to tell text from images, PDFs, and other binary files.
const SNIFF_BYTES: u64 = 8 * 1024;
// Exit code for a --file the target can't take: a non-text file it has no native way to attach, or inlined
// text over the caps. 1 stays "error or run not completed", 6 "rate limited".
const EXIT_ATTACHMENT_REFUSED: u8 = 7;
// Characters an @-mention path may hold; other paths are inlined (text) or refused.
const MENTION_SAFE: &str = "-_./+,@%~=";
// The launch line is typed into the pane's shell, maybe before the shell reads it: the tty then keeps at most
// 1024 bytes (macOS MAX_CANON) and drops the rest, and tmux refuses commands over ~16 KiB. A longer line goes
// into <run>/launch.sh, which the pane's shell sources instead.
const LAUNCH_LINE_MAX: usize = 1000;
// How long the target CLI gets to take over the pane's foreground after the
// launch line is typed, and how long it must hold it before tmuxlet pastes.
const TARGET_START_SECS: u64 = 30;
const TARGET_STABLE_MS: u64 = 2500;
// How long the pane may sit back at a bare shell before the run is aborted.
const TARGET_EXIT_GRACE_MS: u64 = 3000;
const DEFAULT_OLLAMA_HOST: &str = "http://localhost:11434";

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileKind {
    Text,
    Image,
    Pdf,
    Binary,
}

impl FileKind {
    fn name(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Image => "image",
            Self::Pdf => "PDF",
            Self::Binary => "binary",
        }
    }
}

/// How a target gets one --file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Delivery {
    /// A launch argument: codex `--image`, gweb `--file`, pi `@path`.
    Flag,
    /// A reference in the prompt that the target expands: claude `@path`, hermes `@file:path`.
    Mention,
    /// The file's text, framed in the prompt.
    Inline,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Attachment {
    // Absolute and canonical.
    path: String,
    kind: FileKind,
    delivery: Delivery,
    // The file's text, for Delivery::Inline only.
    text: Option<String>,
}

#[derive(Debug, Clone)]
struct Options {
    print: bool,
    target: String,
    output_format: OutputFormat,
    cwd: PathBuf,
    prompt_parts: Vec<String>,
    model: Option<String>,
    // gweb only: Gemini's Thinking level (low/medium/high; gweb validates the value).
    thinking: Option<String>,
    // gweb and chatgpt: where the CLI saves the reply's images (a directory or a file path).
    image_out: Option<String>,
    // gweb only: turn on Gemini's Canvas tool, and where to save the doc's markdown.
    canvas: bool,
    canvas_out: Option<String>,
    // --file/--attach values as given, and what plan_attachments made of them.
    files: Vec<String>,
    attachments: Vec<Attachment>,
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
    // True when --timeout was passed; chat backends forward only an explicit value.
    timeout_explicit: bool,
    // None => resolve from TMUXLET_MAX_CONCURRENCY env, else DEFAULT_MAX_CONCURRENCY.
    // 0 == unlimited (semaphore disabled).
    max_concurrency: Option<usize>,
    // Opt-in host-memory backpressure (also enabled by TMUXLET_MEMORY_GUARD).
    memory_guard: bool,
    owner: Option<String>,
    reap_min_age: Option<u64>,
    // --env KEY=VALUE pairs, set on the run's tmux session (never the server env).
    env: Vec<(String, String)>,
    // --ollama MODEL: route claude/codex through the local Ollama server.
    ollama: Option<String>,
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
            thinking: None,
            image_out: None,
            canvas: false,
            canvas_out: None,
            files: Vec::new(),
            attachments: Vec::new(),
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
            timeout_explicit: false,
            max_concurrency: None,
            memory_guard: false,
            owner: None,
            reap_min_age: None,
            env: Vec::new(),
            ollama: None,
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
    // gweb --image-out: gweb's list of saved images, passed through to the JSON result.
    images: Option<serde_json::Value>,
    // gweb --canvas: gweb's Canvas doc {title, markdown, chars, path}, passed through likewise.
    canvas: Option<serde_json::Value>,
    // gweb JSON runs: gweb's citations ({n, label, file} or {n, label, title, url} per footnote).
    citations: Option<serde_json::Value>,
    // gweb / chatgpt JSON runs: which model was asked for and which answered (MODEL_FIELDS).
    models: Option<Vec<(String, serde_json::Value)>>,
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
        Ok(code) => ExitCode::from(code),
        Err(err) => {
            eprintln!("tmuxlet: {err}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<u8, String> {
    let mut args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() {
        print_help();
        return Ok(0);
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
            let mut opts = parse_options(&mut args)?;
            if opts.print {
                // Every --file is checked before a run dir or tmux session exists.
                match plan_attachments(&opts.target, &opts.files, &opts.cwd) {
                    Ok(planned) => opts.attachments = planned,
                    Err((code, message)) if code == EXIT_ATTACHMENT_REFUSED => {
                        eprintln!("tmuxlet: {message}");
                        return Ok(code);
                    }
                    Err((_, message)) => return Err(message),
                }
                let result = run_print(opts)?;
                if result.status == "rate_limited" {
                    emit_result(&result)?;
                    return Ok(6);
                }
                emit_result(&result)?;
            } else {
                return Err("tmuxlet currently supports print mode only; pass -p/--print".into());
            }
            Ok(())
        }
    }
    .map(|_| 0)
}

// Keep identity based on the caller's cwd, even when a run uses --cwd elsewhere.
fn pick_owner(cli: Option<&str>, environment: Option<&str>, fallback: &str) -> String {
    cli.or(environment).unwrap_or(fallback).to_string()
}

fn caller_owner(cli: Option<&str>) -> String {
    let environment = env::var("TMUXLET_OWNER").ok();
    if cli.is_some() || environment.is_some() {
        return pick_owner(cli, environment.as_deref(), "");
    }
    let parent = Command::new("ps")
        .args(["-o", "ppid=", "-p", &std::process::id().to_string()])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    let name = parent
        .and_then(|pid| {
            Command::new("ps")
                .args(["-o", "comm=", "-p", &pid])
                .output()
                .ok()
        })
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown-process".into());
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    pick_owner(
        cli,
        environment.as_deref(),
        &format!("{name}@{}", cwd.display()),
    )
}

fn is_foreign(recorded: Option<&str>, caller: &str, force: bool) -> bool {
    !force && recorded.is_some_and(|owner| owner != caller)
}

fn read_meta(id: &str) -> Result<serde_json::Value, String> {
    let paths = run_paths(id.strip_prefix("tmuxlet-").unwrap_or(id))?;
    match fs::read_to_string(paths.meta) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|e| format!("cannot read metadata for run {id}: {e}")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(serde_json::json!({})),
        Err(e) => Err(format!("cannot read metadata for run {id}: {e}")),
    }
}

fn check_owner(id: &str, caller: &str, force: bool) -> Result<(), String> {
    let meta = read_meta(id)?;
    let owner = meta["owner"].as_str();
    if is_foreign(owner, caller, force) {
        return Err(format!(
            "run {id} belongs to owner {:?}; caller is {caller:?}; use --force-foreign to override",
            owner.unwrap()
        ));
    }
    if owner.is_none() {
        eprintln!("tmuxlet: warning: run {id} has unknown owner (legacy run); allowing access");
    }
    Ok(())
}

#[derive(Debug)]
struct ManagementAccess {
    owner: String,
    force_foreign: bool,
}

// -- ends flag parsing, so send messages can contain literal management flags.
fn management_args(args: &[String]) -> Result<(Vec<String>, ManagementAccess), String> {
    let mut rest = Vec::new();
    let mut owner = None;
    let mut force_foreign = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--owner" => owner = Some(take_value(args, &mut i, "--owner")?),
            "--force-foreign" => force_foreign = true,
            "--" => {
                rest.extend_from_slice(&args[i..]);
                break;
            }
            _ => rest.push(args[i].clone()),
        }
        i += 1;
    }
    Ok((
        rest,
        ManagementAccess {
            owner: caller_owner(owner.as_deref()),
            force_foreign,
        },
    ))
}

fn parse_minutes(value: &str) -> Result<u64, String> {
    value
        .parse::<u64>()
        .ok()
        .filter(|n| n.checked_mul(60).is_some())
        .ok_or_else(|| "reap minimum age must be a non-negative integer number of minutes".into())
}

fn pick_reap_min_age(cli: Option<u64>, environment: Option<&str>) -> Result<u64, String> {
    let minutes = match cli {
        Some(n) => n,
        None => match environment {
            Some(value) => parse_minutes(value)?,
            None => 10,
        },
    };
    minutes
        .checked_mul(60)
        .ok_or_else(|| "reap minimum age is too large".into())
}

fn resolve_reap_min_age(cli: Option<u64>) -> Result<u64, String> {
    pick_reap_min_age(cli, env::var("TMUXLET_REAP_MIN_AGE_MIN").ok().as_deref())
}

fn parse_options(args: &mut [String]) -> Result<Options, String> {
    let mut opts = Options::default();
    let mut i = 0;

    while i < args.len() {
        let arg = &args[i];
        match arg.as_str() {
            "-p" | "--print" => opts.print = true,
            "--owner" => opts.owner = Some(take_value(args, &mut i, arg)?),
            "--reap-min-age" => {
                opts.reap_min_age = Some(parse_minutes(&take_value(args, &mut i, arg)?)?);
            }
            "--target" => opts.target = take_value(args, &mut i, arg)?,
            "--output-format" => {
                opts.output_format = OutputFormat::parse(&take_value(args, &mut i, arg)?)?;
            }
            "-C" | "--cwd" | "--cd" | "--dir" => {
                opts.cwd = expand_home(&take_value(args, &mut i, arg)?);
            }
            "-m" | "--model" => opts.model = Some(take_value(args, &mut i, arg)?),
            "--thinking" => opts.thinking = Some(take_value(args, &mut i, arg)?),
            "--image-out" => opts.image_out = Some(take_value(args, &mut i, arg)?),
            "--canvas" => opts.canvas = true,
            "--canvas-out" => opts.canvas_out = Some(take_value(args, &mut i, arg)?),
            "--file" | "--attach" => opts.files.push(take_value(args, &mut i, arg)?),
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
            "--env" => opts
                .env
                .push(parse_env_pair(&take_value(args, &mut i, arg)?)?),
            "--ollama" => opts.ollama = Some(take_value(args, &mut i, arg)?),
            "--timeout" => {
                let seconds = take_value(args, &mut i, arg)?
                    .parse::<u64>()
                    .map_err(|_| "--timeout must be an integer number of seconds".to_string())?;
                opts.timeout = Duration::from_secs(seconds);
                opts.timeout_explicit = true;
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
            // A bare "-" is the stdin sentinel, not an option; prompt_from reads
            // stdin when it is the whole prompt.
            "-" => opts.prompt_parts.push(arg.clone()),
            other if other.starts_with('-') => return Err(format!("unknown option: {other}")),
            _ => opts.prompt_parts.push(arg.clone()),
        }
        i += 1;
    }

    // `llmcli:<site>` aliases for the chat backends, canonicalized so the rest
    // of the code keeps matching the existing target names.
    if let Some(canon) = match opts.target.as_str() {
        "llmcli:gemini" => Some("gweb"),
        "llmcli:chatgpt" => Some("chatgpt"),
        "llmcli:muse" => Some("muse"),
        _ => None,
    } {
        opts.target = canon.into();
    }

    if let Some(model) = &opts.ollama {
        if model.trim().is_empty() {
            return Err(
                "--ollama requires a model name, e.g. --ollama minimax-m3.1-flash:cloud".into(),
            );
        }
        match &opts.model {
            Some(other) if other != model => {
                return Err(format!(
                    "--ollama {model} conflicts with --model {other}; --ollama sets the model"
                ));
            }
            _ => opts.model = Some(model.clone()),
        }
    }

    Ok(opts)
}

/// `KEY=VALUE` from --env. The key must be a portable env var name; the value
/// may be empty (`ANTHROPIC_API_KEY=`) and may contain `=`.
fn parse_env_pair(value: &str) -> Result<(String, String), String> {
    let (key, val) = value
        .split_once('=')
        .ok_or_else(|| format!("--env expects KEY=VALUE, got {}", redact_env_arg(value)))?;
    let valid = !key.is_empty()
        && !key.starts_with(|c: char| c.is_ascii_digit())
        && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !valid {
        return Err(format!("--env: invalid variable name {key:?}"));
    }
    Ok((key.to_string(), val.to_string()))
}

// A malformed --env value has no key to judge, so never echo it whole.
fn redact_env_arg(value: &str) -> String {
    if value.is_empty() {
        "an empty value".into()
    } else {
        "a value without '='".into()
    }
}

/// Keys whose values tmuxlet never writes to meta.json, status, or errors.
fn is_secret_env_key(key: &str) -> bool {
    let key = key.to_ascii_uppercase();
    [
        "TOKEN",
        "KEY",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "CREDENTIAL",
        "AUTH",
    ]
    .iter()
    .any(|word| key.contains(word))
}

fn display_env_value(key: &str, value: &str) -> String {
    if is_secret_env_key(key) && !value.is_empty() {
        "<redacted>".into()
    } else {
        value.to_string()
    }
}

fn ollama_host() -> String {
    env::var("TMUXLET_OLLAMA_HOST")
        .ok()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_OLLAMA_HOST.to_string())
}

/// The env `ollama launch claude` sets (ollama cmd/launch/claude.go): Claude
/// Code talks to Ollama's Anthropic-compatible API, every model tier and
/// subagents use the chosen model, and auto mode uses the client classifier
/// because Ollama has no server-side auto-mode checks.
fn ollama_claude_env(model: &str, host: &str) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = [
        ("ANTHROPIC_BASE_URL", host),
        ("ANTHROPIC_AUTH_TOKEN", "ollama"),
        ("ANTHROPIC_API_KEY", ""),
        ("ANTHROPIC_DEFAULT_OPUS_MODEL", model),
        ("ANTHROPIC_DEFAULT_SONNET_MODEL", model),
        ("ANTHROPIC_DEFAULT_HAIKU_MODEL", model),
        ("CLAUDE_CODE_SUBAGENT_MODEL", model),
        ("ENABLE_CLAUDEAI_MCP_SERVERS", "false"),
        ("CLAUDE_CODE_ATTRIBUTION_HEADER", "0"),
        ("CLAUDE_CODE_TOTAL_TOKENS_REMINDER", "off"),
        ("DISABLE_ERROR_REPORTING", "1"),
        ("DISABLE_FEEDBACK_COMMAND", "1"),
        ("CLAUDE_CODE_DISABLE_FEEDBACK_SURVEY", "1"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    if env::var_os("CLAUDE_CODE_AUTO_MODE_SERVER").is_none() {
        env.push(("CLAUDE_CODE_AUTO_MODE_SERVER".into(), "0".into()));
    }
    env
}

/// Variables set on the run's tmux session (`new-session -e`): the --ollama
/// preset first, then --env, a later key replacing an earlier one in place.
fn session_env(opts: &Options) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = Vec::new();
    if let Some(model) = &opts.ollama
        && opts.target == "claude"
    {
        env.extend(ollama_claude_env(model, &ollama_host()));
    }
    for (key, value) in &opts.env {
        match env.iter_mut().find(|(k, _)| k == key) {
            Some(slot) => slot.1 = value.clone(),
            None => env.push((key.clone(), value.clone())),
        }
    }
    env
}

/// The session env as recorded in meta.json and status, secrets redacted.
fn redacted_env_json(env: &[(String, String)]) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for (key, value) in env {
        map.insert(
            key.clone(),
            serde_json::json!(display_env_value(key, value)),
        );
    }
    serde_json::Value::Object(map)
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
    let _ = resolve_reap_min_age(opts.reap_min_age)?;
    opts.owner = Some(caller_owner(opts.owner.as_deref()));
    ensure_tmux()?;
    let started = Instant::now();
    let run_id = make_run_id();
    let tmux_session = format!("tmuxlet-{run_id}");
    let paths = run_paths(&run_id)?;
    fs::create_dir_all(&paths.root).map_err(|e| format!("failed to create run dir: {e}"))?;

    // Authorize the completion-contract operations for this run. The per-target
    // builders translate this into the narrowest grant available (claude/codex/grok
    // get true scoping; agy/opencode/cursor/hermes fall back to their auto-approve
    // mode because their CLIs offer no graded tier).
    opts.contract_grant = Some(ContractGrant {
        run_root: paths.root.clone(),
        exe: tmuxlet_exe(),
    });

    let prompt = prompt_with_attachments(&prompt_from(&opts)?, &opts.target, &opts.attachments);
    fs::write(&paths.prompt, &prompt).map_err(|e| format!("failed to write prompt: {e}"))?;
    let base_command = target_command(&opts)?;
    let chat = is_chat_target(&opts.target);
    let (launch_command, paste) = launch_plan(
        &opts.target,
        base_command.clone(),
        &paths,
        &run_id,
        &tmuxlet_exe(),
        &prompt,
    );

    let env = session_env(&opts);
    write_meta(&paths, &run_id, &tmux_session, &opts, &base_command, &env)?;
    // Acquire a concurrency slot before the heavy TUI session exists. Blocks/queues
    // when at cap or (opt-in) under memory pressure; on a clean acquire-timeout we
    // drop the staged run dir so it doesn't linger as a phantom "exited" run.
    if let Err(e) = acquire_run_slot(&opts) {
        fs::remove_dir_all(&paths.root).ok();
        return Err(e);
    }
    start_tmux_session(&tmux_session, &opts.cwd, &env)?;
    let line = launch_line(&launch_command, &paths.root.join("launch.sh"))?;
    send_launch_command(&tmux_session, &line)?;
    if let Some(text) = paste {
        // Never paste into a bare shell: wait for the CLI to own the pane, and
        // re-check right before the paste.
        let guard = wait_for_target_start(&tmux_session, &opts.target).and_then(|()| {
            match pane_current_command(&tmux_session) {
                Some(name) if !is_shell_command(&name) => Ok(()),
                other => Err(target_not_running_message(
                    &opts.target,
                    other.as_deref(),
                    true,
                )),
            }
        });
        if let Err(err) = guard {
            return Err(abort_run(&paths, &tmux_session, err));
        }
        paste_text(&tmux_session, &text)?;
    }
    // Agent CLIs must own the pane's foreground; chat backends run under
    // `exec sh -c` (a shell by name) and handle their own exits.
    let guard_exit = !chat;
    let mut shell_since: Option<Instant> = None;
    let mut target_seen = false;
    // Auto-Enter is for the target's own dialogs, never a bare shell.
    let mut pane_at_shell = false;

    let startup_auto_enter_deadline =
        Instant::now() + Duration::from_secs(STARTUP_AUTO_ENTER_WINDOW_SECS);
    let mut startup_enter_attempts = 0usize;
    let codex = opts.target == "codex";
    let codex_startup_deadline = Instant::now() + Duration::from_secs(CODEX_STARTUP_WINDOW_SECS);
    let mut codex_ready = false;
    let mut last_pane = String::new();
    let mut last_pane_changed_at = Instant::now();
    let mut next_pane_poll = Instant::now();

    loop {
        if paths.complete.exists() {
            let output = fs::read_to_string(&paths.complete)
                .map_err(|e| format!("failed to read completion file: {e}"))?;
            // Only gweb's and cgpt's JSON results are read back; muse's output stays as it is.
            let json_result = chat_json(&opts);
            let citations = json_result.then(|| gweb_citations(&output)).flatten();
            let models = json_result.then(|| model_fields(&output)).flatten();
            let (output, images, canvas) = if json_result {
                gweb_result(&output, opts.image_out.is_some(), gweb_canvas(&opts))
            } else {
                (output, None, None)
            };
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
                images,
                canvas,
                citations,
                models,
                output_format: opts.output_format,
                cwd: opts.cwd,
                tmux_session,
                completion_source: "explicit".into(),
                elapsed_ms: started.elapsed().as_millis(),
            });
        }

        // Chat backends never show confirmation prompts, so skip the auto-Enter
        // and blocked heuristics for them.
        if guard_exit && Instant::now() >= next_pane_poll {
            let foreground = pane_current_command(&tmux_session);
            let at_shell = foreground.as_deref().is_none_or(is_shell_command);
            pane_at_shell = at_shell;
            if !at_shell {
                target_seen = true;
                shell_since = None;
            } else if target_seen || started.elapsed() >= Duration::from_secs(TARGET_START_SECS) {
                let since = *shell_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= Duration::from_millis(TARGET_EXIT_GRACE_MS)
                    && !paths.complete.exists()
                    && tmux_session_exists(&tmux_session)
                {
                    let err = target_not_running_message(
                        &opts.target,
                        foreground.as_deref(),
                        !target_seen,
                    );
                    return Err(abort_run(&paths, &tmux_session, err));
                }
            }
        }

        if !chat && Instant::now() >= next_pane_poll {
            let pane = capture_pane(&tmux_session, 200).unwrap_or_default();
            if codex {
                // Never auto-Enter codex: fail fast on its startup dialogs instead.
                if !codex_ready && Instant::now() <= codex_startup_deadline {
                    match codex_startup_state(&pane, &run_id) {
                        CodexStartup::Pending => {}
                        CodexStartup::Ready => codex_ready = true,
                        CodexStartup::Blocked(err) => {
                            fs::write(&paths.pane_log, &pane).ok();
                            fs::write(&paths.error, format!("{err}\n")).ok();
                            kill_session_exact(&tmux_session);
                            return Err(err);
                        }
                    }
                }
            } else if Instant::now() <= startup_auto_enter_deadline
                && !pane_at_shell
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
                    images: None,
                    canvas: None,
                    citations: None,
                    models: None,
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
                images: None,
                canvas: None,
                citations: None,
                models: None,
                output_format: opts.output_format,
                cwd: opts.cwd,
                tmux_session,
                completion_source: "fallback".into(),
                elapsed_ms: started.elapsed().as_millis(),
            });
        }

        if !tmux_session_exists(&tmux_session) {
            // A chat backend's pane exits right after `bridge complete`; recheck so
            // that race reads as completed, not exited.
            if paths.complete.exists() {
                continue;
            }
            let pane = fs::read_to_string(&paths.pane_log).unwrap_or_default();
            let status = chat_failure_status(&opts.target, &paths.root).into();
            return Ok(RunResult {
                id: run_id,
                target: opts.target,
                status,
                output: pane,
                images: None,
                canvas: None,
                citations: None,
                models: None,
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

/// Ends a run whose target CLI is not running: keeps the pane text and the
/// reason in the run dir, kills only this run's session, returns the error.
fn abort_run(paths: &RunPaths, tmux_session: &str, err: String) -> String {
    capture_pane_to_file(tmux_session, &paths.pane_log).ok();
    fs::write(&paths.error, format!("{err}\n")).ok();
    kill_session_exact(tmux_session);
    err
}

/// The pane launch command plus any text to paste after startup. Agent targets
/// get the prompt wrapped in the completion contract; chat backends never see
/// the contract and read the raw prompt from prompt.txt.
fn launch_plan(
    target: &str,
    base: TargetCommand,
    paths: &RunPaths,
    run_id: &str,
    exe: &str,
    prompt: &str,
) -> (TargetCommand, Option<String>) {
    if is_chat_target(target) {
        return (
            chat_launch_command(&base, paths, run_id, exe, chat_format_env(target)),
            None,
        );
    }
    let instruction = completion_prompt(prompt, run_id, &paths.answer);
    let (command, prompt_in_launch) = command_with_prompt(base, target, &instruction);
    (command, (!prompt_in_launch).then_some(instruction))
}

fn is_chat_target(target: &str) -> bool {
    matches!(
        target,
        "muse" | "musecli" | "gweb" | "gemini-web" | "chatgpt" | "cgpt"
    )
}

/// The env var that makes a chat CLI print JSON errors (gweb, cgpt); muse has none.
fn chat_format_env(target: &str) -> Option<&'static str> {
    match target {
        "gweb" | "gemini-web" => Some("GWEB_FORMAT"),
        "chatgpt" | "cgpt" => Some("CGPT_FORMAT"),
        _ => None,
    }
}

/// Chat backends can't follow the completion contract, so tmuxlet completes the
/// run itself: the CLI writes answer.txt, then the wrapper calls `bridge
/// complete`. On failure the CLI's stderr becomes pane.log and the pane exits
/// (`exec` replaces the pane's shell), so the run ends `exited` with that error
/// as its output. stderr goes to stderr.txt, not error.txt: any error.txt
/// content marks a run terminal for status/prune, even while the CLI still runs.
/// The run dir is named once (`$r`) because the line is typed into a shell that
/// may not be ready yet, and the tty drops input past 1024 bytes (macOS). The
/// pane does not inherit tmuxlet's env, so the bridge gets TMUXLET_HOME (the
/// state dir, `$r/../..`) explicitly.
fn chat_launch_command(
    cli: &TargetCommand,
    paths: &RunPaths,
    run_id: &str,
    exe: &str,
    format_env: Option<&str>,
) -> TargetCommand {
    let mut cli_line = vec![shell_quote(&cli.program)];
    cli_line.extend(cli.args.iter().map(|arg| shell_quote(arg)));
    let script = format!(
        "r={root}; {env}{cli} --prompt-file \"$r/prompt.txt\" --output-file \"$r/answer.txt\" \
2> \"$r/stderr.txt\" && TMUXLET_HOME=\"$r/../..\" {exe} bridge complete --run {run} --file \"$r/answer.txt\" \
2>> \"$r/stderr.txt\" || {{ c=$?; cp \"$r/stderr.txt\" \"$r/pane.log\"; printf '%s\\n' \"$c\" > \"$r/exit-code.txt\"; exit \"$c\"; }}",
        env = format_env
            .map(|var| format!("{var}=json "))
            .unwrap_or_default(),
        root = shell_quote(&paths.root.display().to_string()),
        cli = cli_line.join(" "),
        exe = shell_quote(exe),
        run = shell_quote(run_id),
    );
    TargetCommand {
        program: "exec".into(),
        args: vec!["sh".into(), "-c".into(), script],
    }
}

fn chat_failure_status(target: &str, root: &Path) -> &'static str {
    if matches!(target, "gweb" | "gemini-web" | "chatgpt" | "cgpt")
        && fs::read_to_string(root.join("exit-code.txt"))
            .unwrap_or_default()
            .trim()
            == "6"
    {
        "rate_limited"
    } else {
        "exited"
    }
}

// gweb and cgpt stderr is forced to JSON in the wrapper; preserve the original
// message and timezone-aware reset timestamp, including null when no time was
// supplied. cgpt --strict-model also exits 6, with error "model_fallback" and
// the requested and served models: that error is kept as it is.
fn rate_limit_error(result: &RunResult) -> serde_json::Value {
    let payload = result.output.lines().find_map(|line| {
        serde_json::from_str::<serde_json::Value>(line)
            .ok()
            .filter(|v| {
                matches!(v["error"].as_str(), Some("rate_limited" | "model_fallback"))
                    && v["code"] == 6
                    && v["message"].is_string()
            })
    });
    let payload = payload.unwrap_or_else(|| serde_json::json!({"message": result.output.trim()}));
    if payload["error"] == "model_fallback" {
        return serde_json::json!({
            "error": "model_fallback", "code": 6, "message": payload["message"],
            "requestedModel": payload["requestedModel"], "servedModel": payload["servedModel"]
        });
    }
    serde_json::json!({
        "error": "rate_limited", "code": 6,
        "message": payload["message"], "resets_at": payload["resets_at"]
    })
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
    } else if target == "opencode" {
        // opencode's TUI submits `--prompt <text>` on startup, so the prompt
        // rides in the launch command and is never pasted.
        command.args.push("--prompt".to_string());
        command.args.push(final_prompt.to_string());
        (command, true)
    } else if matches!(target, "agy" | "gemini") {
        // agy (Antigravity) has no positional prompt. Seed the initial prompt
        // into its interactive session via `--prompt-interactive` so it rides in
        // the launch command instead of being pasted after startup: agy's
        // first-run "trust this folder?" gate consumes an early paste, so the
        // prompt must arrive as a launch argument. (`gemini` is the deprecated
        // alias for the same `agy` binary.)
        command.args.push("--prompt-interactive".to_string());
        command.args.push(final_prompt.to_string());
        (command, true)
    } else {
        (command, false)
    }
}

fn target_accepts_prompt_argument(target: &str) -> bool {
    // cursor-agent takes `[prompt...]` positionally. Pasting after launch races
    // its TUI startup: the paste + Enter land in the tty before raw mode, so the
    // text shows in the input box but the Enter is lost and the run hangs.
    matches!(
        target,
        "claude" | "codex" | "grok" | "cursor" | "cursor-agent"
    )
}

fn target_command(opts: &Options) -> Result<TargetCommand, String> {
    validate_session_controls(opts)?;

    let target = opts.target.as_str();
    if opts.thinking.is_some() && !matches!(target, "gweb" | "gemini-web") {
        unsupported(target, "--thinking", "only gweb has a thinking level")?;
    }
    if opts.image_out.is_some() && !matches!(target, "gweb" | "gemini-web" | "chatgpt" | "cgpt") {
        unsupported(target, "--image-out", "only gweb and chatgpt save images")?;
    }
    if !matches!(target, "gweb" | "gemini-web") {
        if opts.canvas {
            unsupported(target, "--canvas", "only gweb has Gemini's Canvas")?;
        }
        if opts.canvas_out.is_some() {
            unsupported(target, "--canvas-out", "only gweb has Gemini's Canvas")?;
        }
    }
    if opts.ollama.is_some() && !matches!(target, "claude" | "codex") {
        unsupported(
            target,
            "--ollama",
            "only claude and codex; use --env or --target-arg for other targets",
        )?;
    }
    let mut cmd = match target {
        "claude" => claude_command(opts)?,
        "agy" | "gemini" => agy_command(opts)?,
        "codex" => codex_command(opts)?,
        "opencode" => opencode_command(opts)?,
        "pi" => pi_command(opts)?,
        "cursor" | "cursor-agent" => cursor_command(opts)?,
        "grok" => grok_command(opts)?,
        "hermes" | "hermes-agent" => hermes_command(opts)?,
        "muse" | "musecli" => muse_command(opts)?,
        "gweb" | "gemini-web" => gweb_command(opts)?,
        "chatgpt" | "cgpt" => chatgpt_command(opts)?,
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

// Antigravity CLI (Google) flag mapping — the `agy` binary, successor to the
// deprecated `gemini` CLI. tmuxlet drives agy's interactive session and the
// completion contract runs as ordinary tool calls (not agy's native `-p`/`--print`
// headless mode). The prompt is seeded via `--prompt-interactive <prompt>` in the
// launch command (see command_with_prompt) rather than pasted after startup: agy
// shows a full-screen "trust this folder?" gate on first run, and an early paste
// lands before the REPL input is ready and is lost. The startup auto-enter loop
// clears that gate with Enter.
// Sessions: `--continue` resumes the most recent; `--conversation <id>` resumes
// by id. Value-less `--resume` (latest) and `--continue` map to `--continue`;
// `--resume <id>` and `--session-id <id>` both map to `--conversation <id>` (agy
// has no create-with-id flag, so an explicit id resumes). `--session` is rejected.
// Permissions: agy's only CLI auto-approve is `--dangerously-skip-permissions`
// (its scoped allowlist lives in settings.json, not a CLI flag), so it fills the
// same broad bucket as opencode/cursor/hermes.
fn agy_command(opts: &Options) -> Result<TargetCommand, String> {
    reject_any("agy", "--system-prompt", &opts.system_prompt)?;
    reject_nonempty("agy", "--append-system-prompt", &opts.append_system_prompts)?;
    reject_nonempty("agy", "--allowed-tools/--tools", &opts.allowed_tools)?;
    reject_nonempty("agy", "--disallowed-tools", &opts.disallowed_tools)?;
    reject_nonempty("agy", "--tools", &opts.tools)?;
    reject_nonempty("agy", "--mcp-config", &opts.mcp_configs)?;
    reject_any("agy", "--settings", &opts.settings)?;
    reject_any("agy", "--permission-mode", &opts.permission_mode)?;
    reject_any("agy", "--session", &opts.session)?;

    let mut args = Vec::new();
    push_opt(&mut args, "--model", &opts.model);
    // agy has no graded auto mode, so the completion-contract shell call requires
    // full auto-approve (--dangerously-skip-permissions).
    if opts.dangerously_skip_permissions || opts.contract_grant.is_some() {
        args.push("--dangerously-skip-permissions".into());
    }
    for value in &opts.add_dirs {
        args.extend(["--add-dir".into(), value.clone()]);
    }
    if opts.continue_latest {
        args.push("--continue".into());
    }
    if let Some(value) = &opts.resume {
        match value {
            Some(id) => args.extend(["--conversation".into(), id.clone()]),
            None => args.push("--continue".into()),
        }
    }
    push_opt(&mut args, "--conversation", &opts.session_id);
    Ok(TargetCommand {
        program: "agy".into(),
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
    // A headless run can't answer codex's startup screens, so skip the shared
    // background server (and its version-mismatch prompt) and the update check.
    // `codex resume` accepts both after its own arguments.
    args.extend([
        "--no-daemon".into(),
        "-c".into(),
        "check_for_update_on_startup=false".into(),
    ]);
    let home = env::var_os("HOME").map(PathBuf::from);
    if let Some(dir) = codex_trusted_dir(
        &opts.cwd,
        home.as_deref(),
        opts.dangerously_skip_permissions,
    ) {
        // Trusts the folder for this invocation only; nothing is saved to
        // config.toml. The dotted `projects."<path>".trust_level` key breaks on
        // paths containing dots, so pass an inline table instead.
        args.extend([
            "-c".into(),
            format!(
                "projects={{{}={{trust_level=\"trusted\"}}}}",
                toml_string(&dir.display().to_string())
            ),
        ]);
    }
    if opts.ollama.is_some() {
        // Ollama's documented manual setup: `codex --oss -m <model>`, pinned to
        // the ollama local provider (not LM Studio) so no picker appears.
        args.extend(["--oss".into(), "--local-provider".into(), "ollama".into()]);
    }
    push_opt(&mut args, "--model", &opts.model);
    args.extend(["--cd".into(), opts.cwd.display().to_string()]);
    for path in flag_files(opts) {
        args.extend(["--image".into(), path.into()]);
    }
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
                "sandbox_workspace_write.writable_roots=[{}]",
                toml_string(&grant.run_root.display().to_string())
            ),
        ]);
    }
    Ok(TargetCommand {
        program: "codex".into(),
        args,
    })
}

/// The folder codex may trust for this run: the canonical cwd, when the caller
/// passed --force or the cwd sits under ~/Sites. Elsewhere codex's own trust
/// screen stays in charge (and fails the run fast; see `codex_startup_state`).
fn codex_trusted_dir(cwd: &Path, home: Option<&Path>, force: bool) -> Option<PathBuf> {
    let cwd = fs::canonicalize(cwd).ok()?;
    let sites = home.and_then(|home| fs::canonicalize(home.join("Sites")).ok());
    (force || sites.is_some_and(|sites| cwd.starts_with(sites))).then_some(cwd)
}

/// A TOML basic string, for paths spliced into codex `-c` overrides.
fn toml_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
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
    // opencode 1.18 TUI: `opencode [project] [options]`. It has no --dir or
    // --dangerously-skip-permissions (those made it print its help and exit);
    // the project is the positional path and --auto is its auto-approve.
    args.insert(0, opts.cwd.display().to_string());
    // opencode has no graded auto mode and no confirmed headless allow-list flag,
    // so --auto ("auto-approve permissions that are not explicitly denied") is
    // the only reliable non-interactive auto-approve path.
    if opts.dangerously_skip_permissions || opts.contract_grant.is_some() {
        args.push("--auto".into());
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
    // `pi [options] [@files...]`: each @path joins the first prompt (text or image).
    args.extend(flag_files(opts).map(|path| format!("@{path}")));
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
        // `--continue` flag, not the `resume` subcommand: `agent resume` takes
        // no positional prompt, so `resume -- "<prompt>"` would drop the prompt.
        args.push("--continue".into());
    }
    // --force lets cursor-agent print mode perform writes (incl. the answer file).
    // The bridge shell call may additionally need a .cursor/cli.json allow entry
    // depending on version; --force is the best single-flag auto-approve available.
    if opts.dangerously_skip_permissions || opts.contract_grant.is_some() {
        args.push("--force".into());
        // Skip the first-run "Workspace Trust Required" modal, which the
        // startup auto-enter heuristics do not recognize.
        args.push("--trust".into());
    }
    Ok(TargetCommand {
        program: "cursor-agent".into(),
        args,
    })
}

// Grok Build (xAI) flag mapping — official `grok` binary only.
// Interactive positional prompt (not headless `-p`/`--single`).
// Sessions: `--continue` / `--resume [id]` resume; `--session-id` names a *new*
// session UUID only (does not resume — reject mapping as resume).
// Permissions: Claude-compatible `--allow`/`--deny` + `--permission-mode`;
// full bypass is `--always-approve`. Sandbox is off by default so no add-dir.
fn grok_command(opts: &Options) -> Result<TargetCommand, String> {
    reject_nonempty("grok", "--add-dir", &opts.add_dirs)?;
    reject_nonempty("grok", "--mcp-config", &opts.mcp_configs)?;
    reject_any("grok", "--settings", &opts.settings)?;
    if opts.session.is_some() {
        unsupported(
            "grok",
            "--session",
            "use --session-id, --resume, or --continue",
        )?;
    }

    let mut args = Vec::new();
    args.extend(["--cwd".into(), opts.cwd.display().to_string()]);
    push_opt(&mut args, "--model", &opts.model);
    // Explicit mode is passed through; full bypass takes precedence and maps to
    // --always-approve (grok's YOLO) rather than Claude's --dangerously-skip-permissions.
    if !opts.dangerously_skip_permissions {
        push_opt(&mut args, "--permission-mode", &opts.permission_mode);
    }
    if opts.dangerously_skip_permissions {
        args.push("--always-approve".into());
    }
    push_opt(&mut args, "--system-prompt-override", &opts.system_prompt);
    for value in &opts.append_system_prompts {
        args.extend(["--rules".into(), value.clone()]);
    }
    for value in &opts.allowed_tools {
        args.extend(["--allow".into(), value.clone()]);
    }
    for value in &opts.disallowed_tools {
        args.extend(["--deny".into(), value.clone()]);
    }
    for value in &opts.tools {
        args.extend(["--tools".into(), value.clone()]);
    }
    if opts.continue_latest {
        args.push("--continue".into());
    }
    if let Some(value) = &opts.resume {
        args.push("--resume".into());
        if let Some(value) = value {
            args.push(value.clone());
        }
    }
    // grok --session-id creates a *new* conversation UUID; it does not resume.
    // Map the normalized flag through with that native meaning.
    push_opt(&mut args, "--session-id", &opts.session_id);
    // Scope the completion contract with Claude-compatible --allow rules. Grok
    // has no --add-dir; sandbox is off by default so path access is unrestricted
    // and the permission rules alone clear the tool-permission gate. Skipped
    // under full bypass, which already auto-approves everything.
    if !opts.dangerously_skip_permissions
        && let Some(grant) = &opts.contract_grant
    {
        let root = grant.run_root.display().to_string();
        let abs = format!("//{}", root.trim_start_matches('/'));
        args.extend(["--allow".into(), format!("Write({abs}/**)")]);
        args.extend(["--allow".into(), format!("Edit({abs}/**)")]);
        args.extend([
            "--allow".into(),
            format!("Bash({} bridge complete:*)", grant.exe),
        ]);
    }
    Ok(TargetCommand {
        program: "grok".into(),
        args,
    })
}

// Hermes Agent (Nous Research) flag mapping — official `hermes` binary.
// tmuxlet drives the line-oriented classic REPL (`--cli`), not the full-screen
// TUI: the prompt is typed in after startup and the completion contract runs as
// ordinary tool calls, so hermes is NOT in `target_accepts_prompt_argument`.
// Prompt delivery: hermes takes no positional prompt argument. Its headless
// one-shot prompt is a flag value (`-z <prompt>` or `chat -q <prompt>`), which
// tmuxlet does not use; the prompt is typed into the REPL after startup.
// Sessions: `--continue [name]` resumes the most recent; `--resume <id>` resumes
// by id/title. Value-less `--resume` (latest) maps to `--continue` because
// hermes `--resume` requires an id. There is no create-with-id flag, so the
// normalized `--session-id` maps to `--resume <id>` (resume that session).
// Permissions: no scoped allow/deny tier — `--yolo` (bypass dangerous-command
// approval) is the only non-interactive auto-approve, so it fills the same broad
// bucket as agy/opencode/cursor.
fn hermes_command(opts: &Options) -> Result<TargetCommand, String> {
    reject_any("hermes", "--system-prompt", &opts.system_prompt)?;
    reject_nonempty(
        "hermes",
        "--append-system-prompt",
        &opts.append_system_prompts,
    )?;
    reject_nonempty("hermes", "--add-dir", &opts.add_dirs)?;
    reject_nonempty("hermes", "--allowed-tools", &opts.allowed_tools)?;
    reject_nonempty("hermes", "--disallowed-tools", &opts.disallowed_tools)?;
    reject_nonempty("hermes", "--tools", &opts.tools)?;
    reject_nonempty("hermes", "--mcp-config", &opts.mcp_configs)?;
    reject_any("hermes", "--settings", &opts.settings)?;
    reject_any("hermes", "--permission-mode", &opts.permission_mode)?;
    if opts.session.is_some() {
        unsupported("hermes", "--session", "use --resume or --continue")?;
    }

    let mut args = Vec::new();
    // Force the classic prompt_toolkit REPL. The full-screen TUI captures raw
    // keys and redraws, which tmux send-keys/capture-pane cannot drive reliably.
    args.push("--cli".into());
    push_opt(&mut args, "--model", &opts.model);
    if opts.continue_latest {
        args.push("--continue".into());
    }
    if let Some(value) = &opts.resume {
        match value {
            Some(id) => args.extend(["--resume".into(), id.clone()]),
            // hermes --resume needs an id; --continue resumes the most recent.
            None => args.push("--continue".into()),
        }
    }
    // No create-with-id flag on hermes: the explicit id resumes that session.
    push_opt(&mut args, "--resume", &opts.session_id);
    // hermes has no graded permission tier, so the completion contract's shell +
    // file-write calls need full auto-approve (--yolo). Skipped when neither a
    // grant nor an explicit bypass asks for it, matching the other broad targets.
    if opts.dangerously_skip_permissions || opts.contract_grant.is_some() {
        args.push("--yolo".into());
    }
    Ok(TargetCommand {
        program: "hermes".into(),
        args,
    })
}

// Chat backends: Meta Muse, Gemini web, and ChatGPT web, all run through the
// shared `llmcli` binary as `llmcli <site> ...` (binary overridable with
// TMUXLET_LLMCLI_BIN; default `llmcli`). They are print-mode
// chat CLIs, not agent TUIs, so they get the raw prompt and tmuxlet completes the
// run itself (see chat_launch_command). Sessions are the CLI's chat ids: an
// explicit id (`--resume <id>`, `--session-id`, `--session`) maps to `--chat
// <id>`; `--continue` or value-less `--resume` maps to `-c` (the CLI's last
// chat). Agent-only flags fail fast. Both CLIs share one Chrome profile per CLI
// and queue on its lock, so `--lock-wait` lets a queued run wait for the whole
// run timeout instead of the CLI's 30s default. `--timeout` is forwarded only
// when given explicitly; otherwise the CLI keeps its own answer timeout.
fn muse_command(opts: &Options) -> Result<TargetCommand, String> {
    if opts.model.is_some() {
        unsupported("muse", "--model", "Muse has no model picker")?;
    }
    chat_command(opts, "muse")
}

fn gweb_command(opts: &Options) -> Result<TargetCommand, String> {
    let mut cmd = chat_command(opts, "gweb")?;
    push_opt(&mut cmd.args, "--model", &opts.model);
    push_opt(&mut cmd.args, "--thinking", &opts.thinking);
    for path in flag_files(opts) {
        cmd.args.extend(["--file".into(), path.into()]);
    }
    if let Some(out) = &opts.image_out {
        cmd.args
            .extend(["--image-out".into(), absolute_from(out, &opts.cwd)]);
    }
    if let Some(out) = &opts.canvas_out {
        cmd.args
            .extend(["--canvas-out".into(), absolute_from(out, &opts.cwd)]);
    } else if opts.canvas {
        cmd.args.push("--canvas".into());
    }
    if gweb_json(opts) {
        // gweb then writes its JSON result to answer.txt; gweb_result turns it
        // back into the text output plus `images` / `canvas`.
        cmd.args.extend(["--output-format".into(), "json".into()]);
    }
    Ok(cmd)
}

fn chatgpt_command(opts: &Options) -> Result<TargetCommand, String> {
    let mut cmd = chat_command(opts, "chatgpt")?;
    push_opt(&mut cmd.args, "--model", &opts.model);
    for path in flag_files(opts) {
        cmd.args.extend(["--file".into(), path.into()]);
    }
    if let Some(out) = &opts.image_out {
        cmd.args
            .extend(["--image-out".into(), absolute_from(out, &opts.cwd)]);
    }
    if chatgpt_json(opts) {
        // cgpt then writes its JSON result to answer.txt; gweb_result turns it back
        // into the text output plus `images` (the shapes match gweb's).
        cmd.args.extend(["--output-format".into(), "json".into()]);
    }
    Ok(cmd)
}

/// cgpt runs whose JSON result tmuxlet reads back: --image-out, and any run whose
/// own result is JSON (for `citations` and the model fields).
fn chatgpt_json(opts: &Options) -> bool {
    opts.image_out.is_some() || opts.output_format == OutputFormat::Json
}

/// Whether this run's chat CLI writes its JSON result to answer.txt.
fn chat_json(opts: &Options) -> bool {
    match opts.target.as_str() {
        "gweb" | "gemini-web" => gweb_json(opts),
        "chatgpt" | "cgpt" => chatgpt_json(opts),
        _ => false,
    }
}

fn gweb_canvas(opts: &Options) -> bool {
    opts.canvas || opts.canvas_out.is_some()
}

/// gweb runs whose JSON result tmuxlet reads back: --image-out, --canvas, and
/// any run whose own result is JSON (for gweb's `citations`).
fn gweb_json(opts: &Options) -> bool {
    opts.image_out.is_some() || gweb_canvas(opts) || opts.output_format == OutputFormat::Json
}

/// The --file paths a target takes as launch arguments.
fn flag_files(opts: &Options) -> impl Iterator<Item = &str> {
    opts.attachments
        .iter()
        .filter(|a| a.delivery == Delivery::Flag)
        .map(|a| a.path.as_str())
}

/// How `target` takes a `kind` of file: its native mechanism, else inlining for
/// text, else None (refused). Verified from each CLI's --help and docs:
/// claude expands `@path` mentions (its Read tool takes images and PDFs too);
/// codex has `-i/--image`; pi takes `@file` launch arguments (text or image);
/// hermes expands `@file:path` in REPL messages; gweb and cgpt have `--file` for
/// every type. cursor-agent and muse have no file input; opencode's `-f` is only on
/// `opencode run`, not the TUI tmuxlet drives; grok's and agy's `@` file pickers
/// are interactive, so their launch prompts get text inlined instead.
fn delivery_for(target: &str, kind: FileKind) -> Option<Delivery> {
    use FileKind::{Image, Pdf, Text};
    match (target, kind) {
        ("gweb" | "gemini-web" | "chatgpt" | "cgpt", _) => Some(Delivery::Flag),
        ("claude", Text | Image | Pdf) => Some(Delivery::Mention),
        ("codex", Image) | ("pi", Text | Image) => Some(Delivery::Flag),
        ("hermes" | "hermes-agent", Text) => Some(Delivery::Mention),
        (_, Text) => Some(Delivery::Inline),
        _ => None,
    }
}

const ATTACH_TARGETS: &[&str] = &[
    "claude", "codex", "cursor", "agy", "opencode", "grok", "hermes", "pi", "muse", "gweb",
    "chatgpt",
];

/// Resolve each --file against `cwd`, classify it, and pick its delivery for
/// `target`, before anything starts. Err is (exit code, message): 1 for a path
/// that isn't a regular file, EXIT_ATTACHMENT_REFUSED for a file the target
/// can't take.
fn plan_attachments(
    target: &str,
    files: &[String],
    cwd: &Path,
) -> Result<Vec<Attachment>, (u8, String)> {
    let mut planned = Vec::new();
    let mut inlined = 0u64;
    for value in files {
        let bad = |e: io::Error| (1, format!("--file {value}: {e}"));
        let path = fs::canonicalize(absolute_from(value, cwd)).map_err(bad)?;
        let meta = fs::metadata(&path).map_err(bad)?;
        if !meta.is_file() {
            return Err((1, format!("--file {value}: not a regular file")));
        }
        let shown = path.display().to_string();
        let refuse = |why: String| (EXIT_ATTACHMENT_REFUSED, format!("--file {shown}: {why}"));
        let mut head = Vec::new();
        fs::File::open(&path)
            .and_then(|file| file.take(SNIFF_BYTES).read_to_end(&mut head))
            .map_err(bad)?;
        let mut kind = file_kind(&head, head.len() as u64 == meta.len());
        let mut delivery = delivery_for(target, kind);
        if delivery == Some(Delivery::Mention) && !mention_safe(&shown) {
            if kind != FileKind::Text {
                return Err(refuse(format!(
                    "{target} attaches {} files by @-mention, which can't carry this path; use a path of \
letters, digits, and {MENTION_SAFE}",
                    kind.name()
                )));
            }
            delivery = Some(Delivery::Inline);
        }
        let mut text = None;
        if delivery == Some(Delivery::Inline) {
            let kib = |bytes: u64| bytes.div_ceil(1024);
            if meta.len() > INLINE_FILE_MAX_BYTES {
                return Err(refuse(format!(
                    "{target} gets text files inlined into the prompt, at most {} KiB each; this one is {} KiB",
                    kib(INLINE_FILE_MAX_BYTES),
                    kib(meta.len())
                )));
            }
            inlined += meta.len();
            if inlined > INLINE_TOTAL_MAX_BYTES {
                return Err(refuse(format!(
                    "inlined --file text would total {} KiB; the cap is {} KiB",
                    kib(inlined),
                    kib(INLINE_TOTAL_MAX_BYTES)
                )));
            }
            match fs::read(&path).map_err(bad)?.try_into() {
                Ok(content) if is_text(String::as_bytes(&content), true) => text = Some(content),
                _ => (kind, delivery) = (FileKind::Binary, None), // not text past the sniffed head
            }
        }
        let Some(delivery) = delivery else {
            let takers: Vec<&str> = ATTACH_TARGETS
                .iter()
                .copied()
                .filter(|t| delivery_for(t, kind).is_some_and(|d| d != Delivery::Inline))
                .collect();
            return Err(refuse(format!(
                "{target} can't take {} files: it has no native way to attach one, and only text is inlined \
(targets that take {} files: {})",
                kind.name(),
                kind.name(),
                takers.join(", ")
            )));
        };
        planned.push(Attachment {
            path: shown,
            kind,
            delivery,
            text,
        });
    }
    Ok(planned)
}

/// Images by magic number (PNG, JPEG, GIF, WebP), PDFs by `%PDF-`, text when
/// the bytes are UTF-8 without control characters, else binary. `complete` says
/// `head` is the whole file, so a UTF-8 sequence cut at its end is an error.
fn file_kind(head: &[u8], complete: bool) -> FileKind {
    let webp = head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"WEBP";
    if webp
        || [
            &b"\x89PNG\r\n\x1a\n"[..],
            b"\xff\xd8\xff",
            b"GIF87a",
            b"GIF89a",
        ]
        .iter()
        .any(|magic| head.starts_with(magic))
    {
        FileKind::Image
    } else if head.starts_with(b"%PDF-") {
        FileKind::Pdf
    } else if is_text(head, complete) {
        FileKind::Text
    } else {
        FileKind::Binary
    }
}

/// UTF-8 with no NUL or other C0 controls besides tab, newlines, form feed, and escape.
fn is_text(bytes: &[u8], complete: bool) -> bool {
    let utf8 = match std::str::from_utf8(bytes) {
        Ok(_) => true,
        Err(e) => !complete && e.error_len().is_none(),
    };
    utf8 && !bytes
        .iter()
        .any(|b| matches!(b, 0..=8 | 0x0e..=0x1a | 0x1c..=0x1f))
}

fn mention_safe(path: &str) -> bool {
    path.chars()
        .all(|c| c.is_ascii_alphanumeric() || MENTION_SAFE.contains(c))
}

/// The prompt with its @-mentions and inlined files appended, ahead of any
/// completion contract. Unchanged when there are none.
fn prompt_with_attachments(prompt: &str, target: &str, attachments: &[Attachment]) -> String {
    let mut out = prompt.to_string();
    let mentions: Vec<String> = attachments
        .iter()
        .filter(|a| a.delivery == Delivery::Mention)
        .map(|a| match target {
            "hermes" | "hermes-agent" => format!("@file:{}", a.path),
            _ => format!("@{}", a.path),
        })
        .collect();
    if !mentions.is_empty() {
        out.push_str("\n\nAttached files:\n");
        out.push_str(&mentions.join("\n"));
    }
    for attachment in attachments {
        if let Some(text) = &attachment.text {
            // bytes= is exact, so a reader can find the end even if the text
            // itself contains the closing marker.
            let newline = if text.is_empty() || text.ends_with('\n') {
                ""
            } else {
                "\n"
            };
            out.push_str(&format!(
                "\n\n<<<FILE path={} bytes={}>>>\n{text}{newline}<<<END FILE>>>",
                serde_json::Value::from(attachment.path.as_str()),
                text.len()
            ));
        }
    }
    out
}

/// `value` with `~` expanded and a relative path joined to `cwd`. A trailing `/`
/// (gweb reads it as "a directory") is kept.
fn absolute_from(value: &str, cwd: &Path) -> String {
    let path = expand_home(value);
    let mut text = if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
    .display()
    .to_string();
    if value.ends_with('/') && !text.ends_with('/') {
        text.push('/');
    }
    text
}

type GwebResult = (String, Option<serde_json::Value>, Option<serde_json::Value>);

/// gweb `--image-out` / `--canvas` runs complete with gweb's JSON result. The
/// run's output is gweb's text form: the answer, then one saved image path per
/// line, then the Canvas doc's saved path (or its markdown when unsaved).
/// `images` is gweb's list of {path, width, height, bytes, mime, source_url_kind}
/// and `canvas` its {title, markdown, chars, path}, each returned only when
/// asked for. Output that isn't gweb's JSON passes through unchanged.
fn gweb_result(output: &str, want_images: bool, want_canvas: bool) -> GwebResult {
    let unchanged = (output.to_string(), None, None);
    let Ok(value) = serde_json::from_str::<serde_json::Value>(output.trim()) else {
        return unchanged;
    };
    let images = value.get("images").filter(|v| v.is_array()).cloned();
    let canvas = value.get("canvas").cloned();
    if !value["answer"].is_string()
        || (want_images && images.is_none())
        || (want_canvas && canvas.is_none())
    {
        return unchanged;
    }
    let images = images.filter(|_| want_images);
    let canvas = canvas.filter(|_| want_canvas);
    let mut lines: Vec<&str> = value["answer"]
        .as_str()
        .filter(|answer| !answer.is_empty())
        .into_iter()
        .collect();
    lines.extend(
        images
            .iter()
            .flat_map(|list| list.as_array().into_iter().flatten())
            .filter_map(|image| image["path"].as_str()),
    );
    if let Some(doc) = canvas.as_ref().filter(|doc| doc.is_object()) {
        lines.extend(doc["path"].as_str().or(doc["markdown"].as_str()));
    }
    (format!("{}\n", lines.join("\n")), images, canvas)
}

/// gweb's `citations` list from its JSON result (gweb 0.4.1 and later), when it
/// has one: the sources behind the answer's [^n] footnotes.
fn gweb_citations(output: &str) -> Option<serde_json::Value> {
    serde_json::from_str::<serde_json::Value>(output.trim())
        .ok()?
        .get("citations")
        .filter(|v| v.is_array())
        .cloned()
}

/// The model fields a chat CLI's JSON result may carry, in run-JSON order: cgpt's
/// requested and served model plus its fallback flag, and gweb's served-model
/// fields when it has them.
const MODEL_FIELDS: &[&str] = &[
    "requestedModel",
    "servedModel",
    "servedModelId",
    "servedModelDisplay",
    "fallback",
    "fallbackFrom",
    "fallbackTo",
];

/// The MODEL_FIELDS present in a chat CLI's JSON result, in MODEL_FIELDS order, or
/// None when it has none.
fn model_fields(output: &str) -> Option<Vec<(String, serde_json::Value)>> {
    let value = serde_json::from_str::<serde_json::Value>(output.trim()).ok()?;
    let fields: Vec<(String, serde_json::Value)> = MODEL_FIELDS
        .iter()
        .filter_map(|key| value.get(*key).map(|v| (key.to_string(), v.clone())))
        .collect();
    (!fields.is_empty()).then_some(fields)
}

fn chat_command(opts: &Options, target: &str) -> Result<TargetCommand, String> {
    reject_any(target, "--system-prompt", &opts.system_prompt)?;
    reject_nonempty(
        target,
        "--append-system-prompt",
        &opts.append_system_prompts,
    )?;
    reject_nonempty(target, "--add-dir", &opts.add_dirs)?;
    reject_nonempty(target, "--allowed-tools", &opts.allowed_tools)?;
    reject_nonempty(target, "--disallowed-tools", &opts.disallowed_tools)?;
    reject_nonempty(target, "--tools", &opts.tools)?;
    reject_nonempty(target, "--mcp-config", &opts.mcp_configs)?;
    reject_any(target, "--settings", &opts.settings)?;
    reject_any(target, "--permission-mode", &opts.permission_mode)?;
    if opts.dangerously_skip_permissions {
        unsupported(target, "--dangerously-skip-permissions", "")?;
    }

    // Chat backends all run through the shared `llmcli` binary as
    // `llmcli <site> --lock-wait N -p ...`; the print-mode flags match the
    // old gweb/cgpt/musecli surface one-for-one.
    let site = match target {
        "muse" | "musecli" => "muse",
        "gweb" | "gemini-web" => "gemini",
        "chatgpt" | "cgpt" => "chatgpt",
        _ => target,
    };
    let mut args = vec![
        site.into(),
        "--lock-wait".into(),
        opts.timeout.as_secs().to_string(),
        "-p".into(),
        "--quiet".into(),
    ];
    let chat_id = match &opts.resume {
        Some(Some(id)) => Some(id),
        _ => opts.session_id.as_ref().or(opts.session.as_ref()),
    };
    if let Some(id) = chat_id {
        args.extend(["--chat".into(), id.clone()]);
    } else if opts.continue_latest || opts.resume.is_some() {
        args.push("-c".into());
    }
    if opts.timeout_explicit {
        args.extend(["--timeout".into(), opts.timeout.as_secs().to_string()]);
    }
    Ok(TargetCommand {
        program: env::var("TMUXLET_LLMCLI_BIN")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "llmcli".into()),
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

fn start_tmux_session(name: &str, cwd: &Path, env: &[(String, String)]) -> Result<(), String> {
    run_tmux(new_session_args(name, cwd, env)).map(|_| ())
}

/// `tmux new-session` argv. --env pairs ride as `-e KEY=VALUE`: they land in
/// this session's environment only (the pane's shell inherits them), never in
/// the tmux server's global env, and are never typed into the pane.
fn new_session_args(name: &str, cwd: &Path, env: &[(String, String)]) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> =
        ["new-session", "-d", "-s", name, "-x", "220", "-y", "50"]
            .iter()
            .map(std::ffi::OsString::from)
            .collect();
    args.push("-c".into());
    args.push(cwd.as_os_str().to_owned());
    for (key, value) in env {
        args.push("-e".into());
        args.push(format!("{key}={value}").into());
    }
    args
}

/// The process leading the pane's foreground process group (tmux
/// `pane_current_command`), e.g. `zsh` at a bare prompt or `claude` once the
/// target CLI runs.
fn pane_current_command(tmux_session: &str) -> Option<String> {
    run_tmux([
        OsStr::new("display-message"),
        OsStr::new("-p"),
        OsStr::new("-t"),
        OsStr::new(&tmux_pane_target(tmux_session)),
        OsStr::new("#{pane_current_command}"),
    ])
    .ok()
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty())
}

/// True when the pane's foreground is an interactive shell: anything typed or
/// pasted now would run as shell commands.
fn is_shell_command(name: &str) -> bool {
    let base = name.trim().trim_start_matches('-');
    let base = base.rsplit('/').next().unwrap_or(base);
    matches!(
        base,
        "zsh" | "bash" | "sh" | "fish" | "dash" | "ksh" | "tcsh" | "csh" | "nu" | "login"
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartWatch {
    // Still waiting for the target to take over (or to hold the pane long enough).
    Waiting,
    // The target has held the foreground for TARGET_STABLE_MS.
    Started,
    // The pane is still (or again) a bare shell after the start window.
    NeverStarted,
}

/// Pure start-guard step: `foreground` is the pane's current command,
/// `held_ms` how long a non-shell has held it, `elapsed_ms` since launch.
fn start_watch_step(foreground: Option<&str>, held_ms: u64, elapsed_ms: u64) -> StartWatch {
    let running = foreground.is_some_and(|name| !is_shell_command(name));
    if running && held_ms >= TARGET_STABLE_MS {
        StartWatch::Started
    } else if elapsed_ms >= TARGET_START_SECS * 1000 {
        StartWatch::NeverStarted
    } else {
        StartWatch::Waiting
    }
}

/// Blocks until the target CLI owns the pane's foreground and has held it for
/// TARGET_STABLE_MS. Err when it never starts or exits right away (bad args,
/// missing binary, auth failure), so nothing is ever pasted into the shell.
fn wait_for_target_start(tmux_session: &str, target: &str) -> Result<(), String> {
    let started = Instant::now();
    let mut held_since: Option<Instant> = None;
    let mut last = None;
    loop {
        let foreground = pane_current_command(tmux_session);
        let running = foreground
            .as_deref()
            .is_some_and(|name| !is_shell_command(name));
        if running {
            held_since.get_or_insert_with(Instant::now);
        } else {
            held_since = None;
        }
        let held_ms = held_since.map_or(0, |t| t.elapsed().as_millis() as u64);
        match start_watch_step(
            foreground.as_deref(),
            held_ms,
            started.elapsed().as_millis() as u64,
        ) {
            StartWatch::Started => return Ok(()),
            StartWatch::NeverStarted => {
                return Err(target_not_running_message(target, last.as_deref(), true));
            }
            StartWatch::Waiting => {}
        }
        if !tmux_session_exists(tmux_session) {
            return Err(format!("{target} session ended before the CLI started"));
        }
        last = foreground;
        thread::sleep(Duration::from_millis(250));
    }
}

fn target_not_running_message(target: &str, foreground: Option<&str>, at_start: bool) -> String {
    let shell = foreground.unwrap_or("a shell");
    if at_start {
        format!(
            "{target} did not start (the pane is still at {shell}); check the CLI is installed, \
its flags, and its login; the prompt was not sent; see pane.log"
        )
    } else {
        format!(
            "{target} exited before completing (the pane fell back to {shell}); \
the run was aborted so nothing is typed into the shell; see pane.log"
        )
    }
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

/// The shell line typed into the pane to start `command`. Over LAUNCH_LINE_MAX
/// it is written to `script` and the pane gets `. <script>`: the pane's own
/// shell still runs it, with its aliases and functions (grok is a zsh function),
/// byte for byte, and the typed line names the run dir, which
/// codex_startup_state relies on.
fn launch_line(command: &TargetCommand, script: &Path) -> Result<String, String> {
    let line = std::iter::once(&command.program)
        .chain(&command.args)
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ");
    if line.len() <= LAUNCH_LINE_MAX {
        return Ok(line);
    }
    fs::write(script, format!("{line}\n"))
        .map_err(|e| format!("failed to stage the launch command: {e}"))?;
    Ok(format!(". {}", shell_quote(&script.display().to_string())))
}

fn send_launch_command(tmux_session: &str, line: &str) -> Result<(), String> {
    let target = tmux_pane_target(tmux_session);
    run_tmux([
        OsStr::new("send-keys"),
        OsStr::new("-t"),
        OsStr::new(&target),
        OsStr::new(line),
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
    // Inspect only the current prompt area, not old tools, code, or launch flags.
    pane.lines()
        .rev()
        .filter(|line| !line.trim().is_empty())
        .take(15)
        .any(|line| {
            let text = line.trim().to_ascii_lowercase();
            contains_any(
                &text,
                &[
                    "[y/n]",
                    "[y/n/",
                    "(y/n)",
                    "yes/no",
                    "do you trust the files in this folder",
                    "do you trust this",
                    "trust this workspace",
                    "trust this folder",
                ],
            ) || (text.contains('?')
                && contains_any(
                    &text,
                    &[
                        "allow ",
                        "approve",
                        "proceed",
                        "continue?",
                        "confirm ",
                        "do you want",
                    ],
                ))
                || contains_any(
                    &text,
                    &[
                        "press enter to continue",
                        "press return to continue",
                        "waiting for approval",
                    ],
                )
                || (contains_any(&text, &["1.", "1)", "❯", "›", "> "])
                    && contains_any(
                        &text,
                        &[
                            "yes, proceed",
                            "yes, allow",
                            "yes, approve",
                            "approve once",
                            "allow once",
                        ],
                    ))
        })
}

fn contains_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}

#[derive(Debug, PartialEq, Eq)]
enum CodexStartup {
    Pending,
    Ready,
    Blocked(String),
}

// How lines of codex's chat UI start (lowercased); once one shows, the startup
// screens are behind it. Matched at line start because dialogs print the cwd.
const CODEX_CHAT_CHROME: &[&str] = &[
    ">_ openai codex (v",
    "› ask codex to do anything",
    "? for shortcuts",
    "• working (",
];

// Text that only appears inside a codex startup dialog. Broader words ("folder
// access", "update now", "background server") only choose the hint: codex
// prints "Running without the shared background server" as a plain notice.
const CODEX_DIALOG_PHRASES: &[&str] = &[
    "trust this folder?",
    "run without daemon",
    "incompatible feature settings",
    "restart the shared codex background server",
    "skip until next version",
];

/// Classify codex's startup from its own screen: the pane below the last line
/// naming this run. Everything above that line is the shell echo of the launch
/// command or codex's rendering of the prompt, both of which end in the
/// completion contract naming the run, so a prompt that quotes a menu is never
/// mistaken for one. tmuxlet never answers these dialogs: a trust choice is
/// saved, and the update and daemon menus would upgrade codex or restart the
/// shared server.
fn codex_startup_state(pane: &str, run_id: &str) -> CodexStartup {
    let screen = match pane.rfind(run_id) {
        Some(i) => pane[i..].split_once('\n').map_or("", |(_, rest)| rest),
        None => pane,
    };
    let text = screen.to_ascii_lowercase();
    let lines: Vec<&str> = screen
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if lines.iter().any(|line| {
        let line = line.to_ascii_lowercase();
        CODEX_CHAT_CHROME
            .iter()
            .any(|chrome| line.starts_with(chrome))
    }) {
        return CodexStartup::Ready;
    }
    if !contains_any(&text, CODEX_DIALOG_PHRASES) && !is_selection_menu(&lines) {
        return CodexStartup::Pending;
    }
    let hint = if text.contains("trust this folder") || text.contains("folder access") {
        "pass --force, or run under ~/Sites, so tmuxlet trusts this folder for the run"
    } else if text.contains("daemon") || text.contains("background server") {
        "tmuxlet already passes --no-daemon; upgrade the codex CLI to match the background server"
    } else if text.contains("update") {
        "tmuxlet already sets check_for_update_on_startup=false; update codex manually"
    } else {
        "run codex in this folder once and answer the prompt"
    };
    let relevant: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|line| {
            menu_item(line)
                || line.contains('?')
                || contains_any(&line.to_ascii_lowercase(), CODEX_DIALOG_PHRASES)
        })
        .collect();
    let shown = if relevant.is_empty() {
        &lines
    } else {
        &relevant
    };
    let shown = shown
        .iter()
        .take(4)
        .map(|line| match line.char_indices().nth(100) {
            Some((cut, _)) => format!("{}…", &line[..cut]),
            None => line.to_string(),
        })
        .collect::<Vec<_>>()
        .join(" | ");
    CodexStartup::Blocked(format!(
        "codex startup blocked by interactive prompt: {shown}; hint: {hint}"
    ))
}

/// A numbered choice list with a selection caret: "› 1. Trust and continue",
/// "2. Quit". Plain numbered lists have no caret.
fn is_selection_menu(lines: &[&str]) -> bool {
    lines
        .iter()
        .any(|line| line.starts_with(['›', '❯']) && menu_item(line))
        && lines.iter().filter(|line| menu_item(line)).count() >= 2
}

fn menu_item(line: &str) -> bool {
    let rest = line.trim_start_matches(['›', '❯']).trim_start();
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    digits > 0 && rest[digits..].starts_with(". ")
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

        // Self-heal once: only owned runs old enough to qualify whose panes
        // remain unchanged; permission/trust prompts are always spared.
        if cap_block && !self_reaped && start.elapsed() >= Duration::from_secs(REAP_GRACE_SECS) {
            self_reaped = true;
            let killed = auto_reap_sessions(
                &caller_owner(opts.owner.as_deref()),
                resolve_reap_min_age(opts.reap_min_age)?,
            )?;
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

fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// Legacy runs use metadata creation time, never directory mtime (which changes
// when output files are written). Unknown age is always spared by auto-reap.
fn run_age_secs(id: &str) -> Option<u64> {
    let meta = read_meta(id).ok()?;
    let started = meta["startedAt"].as_u64().or_else(|| {
        fs::metadata(run_paths(id).ok()?.meta)
            .ok()?
            .created()
            .ok()?
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|d| d.as_secs())
    })?;
    epoch_secs().checked_sub(started)
}

fn auto_reap_decision(
    age: Option<u64>,
    min_age: u64,
    prompt: bool,
    unchanged: bool,
) -> Result<&'static str, &'static str> {
    if prompt {
        return Err("waiting on permission/trust prompt");
    }
    if age.is_none_or(|age| age < min_age) {
        return Err("younger than minimum age or unknown start time");
    }
    if !unchanged {
        return Err("pane changed or could not be captured");
    }
    Ok("old with unchanged pane")
}

fn iso_timestamp(epoch: u64) -> String {
    // Gregorian civil date from days since 1970-01-01, in UTC.
    let days = (epoch / 86400) as i64 + 719468;
    let era = days / 146097;
    let doe = days - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    let seconds = epoch % 86400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}

fn reap_log_line(
    timestamp: u64,
    action: &str,
    target: &ReapTarget,
    meta: &serde_json::Value,
    reason: &str,
) -> String {
    // JSON lines escape cwd/owner/reason so each event occupies exactly one line.
    format!(
        "{}\n",
        serde_json::json!({
            "timestamp": iso_timestamp(timestamp), "action": action, "run": target.id,
            "owner": meta["owner"].as_str().unwrap_or("unknown owner"),
            "cwd": meta["cwd"].as_str().unwrap_or("unknown"),
            "target": meta["target"].as_str().unwrap_or("unknown"),
            "age": target.age_secs, "reason": reason,
        })
    )
}

fn log_reap(action: &str, target: &ReapTarget, reason: &str) -> Result<(), String> {
    let meta = read_meta(&target.id).unwrap_or_else(|_| serde_json::json!({}));
    fs::create_dir_all(state_dir())
        .map_err(|e| format!("cannot create reap log directory: {e}"))?;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(state_dir().join("reap.log"))
        .map_err(|e| format!("cannot open reap.log: {e}"))?;
    file.write_all(reap_log_line(epoch_secs(), action, target, &meta, reason).as_bytes())
        .map_err(|e| format!("cannot write reap.log: {e}"))
}

fn select_reap_targets(
    criteria: &ReapCriteria,
    access: &ManagementAccess,
) -> Result<Vec<ReapTarget>, String> {
    let mut targets = Vec::new();
    let mut refusals = Vec::new();
    for session in list_run_sessions() {
        let id = session
            .strip_prefix("tmuxlet-")
            .unwrap_or(&session)
            .to_string();
        let mut target = ReapTarget {
            age_secs: run_age_secs(&id),
            id,
            session,
            reason: "",
        };
        let pane = match capture_pane(&target.session, 200) {
            Ok(pane) => pane,
            Err(e) => {
                log_reap("skipped", &target, &format!("cannot capture pane: {e}"))?;
                continue;
            }
        };
        match reap_decision(
            is_execution_confirmation_blocker(&pane),
            target.age_secs,
            criteria,
            REAP_GRACE_SECS,
        ) {
            Some(reason) => {
                if let Err(e) = check_owner(&target.id, &access.owner, access.force_foreign) {
                    eprintln!("tmuxlet: skipped: {e}");
                    log_reap("skipped", &target, &e)?;
                    refusals.push(e);
                    continue;
                }
                target.reason = reason;
                targets.push(target);
            }
            None => log_reap("skipped", &target, "does not match manual reap criteria")?,
        }
    }
    if !refusals.is_empty() {
        for target in &targets {
            log_reap(
                "skipped",
                target,
                "manual reap refused due to foreign or unreadable runs",
            )?;
        }
        return Err(refusals.join("; "));
    }
    targets.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(targets)
}

fn auto_reap_sessions(owner: &str, min_age: u64) -> Result<usize, String> {
    let mut candidates = Vec::new();
    for session in list_run_sessions() {
        let id = session
            .strip_prefix("tmuxlet-")
            .unwrap_or(&session)
            .to_string();
        let target = ReapTarget {
            age_secs: run_age_secs(&id),
            id,
            session,
            reason: "old with unchanged pane",
        };
        if let Err(e) = check_owner(&target.id, owner, false) {
            log_reap("skipped", &target, &e)?;
            continue;
        }
        let pane = capture_pane(&target.session, 200)
            .ok()
            .filter(|pane| !pane.trim().is_empty());
        let prompt = pane
            .as_ref()
            .is_some_and(|p| is_execution_confirmation_blocker(p));
        if let Err(reason) = auto_reap_decision(target.age_secs, min_age, prompt, pane.is_some()) {
            log_reap("skipped", &target, reason)?;
        } else {
            candidates.push((target, pane.unwrap()));
        }
    }
    if candidates.is_empty() {
        return Ok(0);
    }
    // One shared observation interval, rather than delaying two seconds per run.
    thread::sleep(Duration::from_secs(2));
    let mut killed = 0;
    for (target, before) in candidates {
        let after = capture_pane(&target.session, 200)
            .ok()
            .filter(|pane| !pane.trim().is_empty());
        let prompt = after
            .as_ref()
            .is_some_and(|p| is_execution_confirmation_blocker(p));
        let decision = auto_reap_decision(
            target.age_secs,
            min_age,
            prompt,
            after.as_ref() == Some(&before),
        );
        match decision {
            Ok(_) => {
                // Recheck ownership at the point of mutation as well.
                if let Err(e) = check_owner(&target.id, owner, false) {
                    log_reap("skipped", &target, &e)?;
                    continue;
                }
                if reap_target(&target)? {
                    killed += 1;
                }
            }
            Err(reason) => log_reap("skipped", &target, reason)?,
        }
    }
    Ok(killed)
}

fn kill_session_exact(session: &str) -> bool {
    run_tmux([
        OsStr::new("kill-session"),
        OsStr::new("-t"),
        OsStr::new(&tmux_target(session)),
    ])
    .is_ok()
}

fn reap_target(target: &ReapTarget) -> Result<bool, String> {
    // Ensure the audit destination can be opened before killing anything.
    fs::create_dir_all(state_dir())
        .map_err(|e| format!("cannot create reap log directory: {e}"))?;
    let _log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(state_dir().join("reap.log"))
        .map_err(|e| format!("cannot open reap.log: {e}"))?;
    let killed = kill_session_exact(&target.session);
    log_reap(
        if killed { "reaped" } else { "skipped" },
        target,
        if killed {
            target.reason
        } else {
            "tmux kill failed or session vanished"
        },
    )?;
    Ok(killed)
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
    let (args, access) = management_args(args)?;
    let (criteria, yes, dry_run) = parse_reap_args(&args)?;
    let targets = select_reap_targets(&criteria, &access)?;
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
        for t in &targets {
            log_reap("skipped", t, "dry run")?;
        }
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
            for t in &targets {
                log_reap("skipped", t, "confirmation declined")?;
            }
            println!("Aborted.");
            return Ok(());
        }
    }
    let mut killed = 0;
    for t in &targets {
        if let Err(e) = check_owner(&t.id, &access.owner, access.force_foreign) {
            log_reap("skipped", t, &e)?;
            continue;
        }
        if reap_target(t)? {
            killed += 1;
        }
    }
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
    let (args, access) = management_args(args)?;
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
    let mut skipped = 0;
    for (idx, session) in sessions.iter().enumerate() {
        let id = session.strip_prefix("tmuxlet-").unwrap_or(session);
        if let Err(e) = check_owner(id, &access.owner, access.force_foreign) {
            eprintln!("tmuxlet: skipped: {e}");
            skipped += 1;
            continue;
        }
        // Pace between sessions — never fire send-keys at every session at once.
        if idx > 0 && !interval.is_zero() {
            thread::sleep(interval);
        }
        let id = session.strip_prefix("tmuxlet-").unwrap_or(session);
        let result = if enter {
            send_enter(session)
        } else {
            paste_into_agent(session, &message)
        };
        match result {
            Ok(()) => println!("  -> {} ({}/{})", short_id(id), idx + 1, total),
            // A session that vanished mid-sweep (a run completing) is a benign no-op.
            Err(e) => eprintln!("  -- {} skipped: {e}", short_id(id)),
        }
    }
    println!("Skipped {skipped} foreign or unreadable session(s).");
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
            let text =
                fs::read_to_string(meta).map_err(|e| format!("failed to read metadata: {e}"))?;
            let mut value: serde_json::Value =
                serde_json::from_str(&text).map_err(|e| format!("invalid metadata: {e}"))?;
            if value.get("owner").is_none() {
                value["owner"] = serde_json::json!("unknown owner");
            }
            println!("{value}");
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
    let (args, access) = management_args(args)?;
    let id = args.first().ok_or("usage: tmuxlet send <id> <message>")?;
    check_owner(id, &access.owner, access.force_foreign)?;
    let parts = args.get(1..).unwrap_or_default();
    let parts = parts.strip_prefix(&["--".to_string()]).unwrap_or(parts);
    let message = parts.join(" ");
    if message.is_empty() {
        return Err("usage: tmuxlet send <id> <message>".into());
    }
    paste_into_agent(&tmux_name_from_id(id), &message)
}

/// `send`/`send-all` paste only while an agent CLI owns the pane; at a bare
/// shell the text would run as commands.
fn paste_into_agent(tmux_session: &str, text: &str) -> Result<(), String> {
    match pane_current_command(tmux_session) {
        Some(name) if !is_shell_command(&name) => paste_text(tmux_session, text),
        Some(name) => Err(format!(
            "refusing to paste: the pane is at a bare shell ({name}), not the agent CLI"
        )),
        None => Err("refusing to paste: cannot tell what the pane is running".into()),
    }
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
    let (args, access) = management_args(args)?;
    let id = args.first().ok_or("usage: tmuxlet stop <id>")?;
    if args.len() != 1 {
        return Err("usage: tmuxlet stop <id> [--owner <name>] [--force-foreign]".into());
    }
    check_owner(id, &access.owner, access.force_foreign)?;
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
    let (args, access) = management_args(args)?;
    let parsed = parse_prune_args(&args)?;
    let mut runs = Vec::new();
    let mut skipped_ids = Vec::new();
    for rec in discover_runs() {
        if !parsed.ids.is_empty() && !parsed.ids.contains(&rec.id) {
            runs.push(rec);
            continue;
        }
        match check_owner(&rec.id, &access.owner, access.force_foreign) {
            Ok(()) => runs.push(rec),
            Err(e) => {
                eprintln!("tmuxlet: skipped: {e}");
                skipped_ids.push(rec.id);
            }
        }
    }
    if !skipped_ids.is_empty() {
        println!(
            "Skipped {} foreign or unreadable run(s).",
            skipped_ids.len()
        );
    }
    // Explicit foreign ids were already reported; do not turn skips into unknown ids.
    let ids: Vec<_> = parsed
        .ids
        .iter()
        .filter(|id| !skipped_ids.contains(id))
        .cloned()
        .collect();
    let selected = if !parsed.ids.is_empty() && ids.is_empty() {
        Vec::new()
    } else {
        prune_selection(runs, &ids)?
    };

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
    if result.status == "rate_limited" {
        if result.output_format == OutputFormat::Json {
            println!("{}", result_json(result));
        }
        eprintln!("{}", rate_limit_error(result));
        return Ok(());
    }
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
    match status {
        "completed" => 0,
        "rate_limited" => 6,
        _ => 1,
    }
}

fn write_meta(
    paths: &RunPaths,
    run_id: &str,
    tmux_session: &str,
    opts: &Options,
    command: &TargetCommand,
    env: &[(String, String)],
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
    let mut meta: serde_json::Value = serde_json::from_str(&meta).expect("valid metadata JSON");
    meta["owner"] = serde_json::json!(caller_owner(opts.owner.as_deref()));
    meta["startedAt"] = serde_json::json!(epoch_secs());
    if !env.is_empty() {
        meta["env"] = redacted_env_json(env);
    }
    if let Some(model) = &opts.ollama {
        meta["ollama"] = serde_json::json!(model);
    }
    fs::write(&paths.meta, format!("{meta}\n"))
        .map_err(|e| format!("failed to write metadata: {e}"))?;
    Ok(())
}

fn result_json(result: &RunResult) -> String {
    let base = format!(
        "{{\"id\":\"{}\",\"target\":\"{}\",\"status\":\"{}\",\"output\":\"{}\",\"cwd\":\"{}\",\"tmuxSession\":\"{}\",\"completionSource\":\"{}\",\"elapsedMs\":{}}}",
        json_escape(&result.id),
        json_escape(&result.target),
        json_escape(&result.status),
        json_escape(&result.output),
        json_escape(&result.cwd.display().to_string()),
        json_escape(&result.tmux_session),
        json_escape(&result.completion_source),
        result.elapsed_ms,
    );
    if result.status == "rate_limited" {
        let mut value: serde_json::Value = serde_json::from_str(&base).expect("valid result JSON");
        for (key, field) in rate_limit_error(result).as_object().unwrap() {
            value[key] = field.clone();
        }
        value.to_string()
    } else {
        // Appended as text so the base fields keep their order.
        let mut extra = String::new();
        if let Some(images) = &result.images {
            extra.push_str(&format!(",\"images\":{images}"));
        }
        if let Some(canvas) = &result.canvas {
            extra.push_str(&format!(",\"canvas\":{canvas}"));
        }
        if let Some(citations) = &result.citations {
            extra.push_str(&format!(",\"citations\":{citations}"));
        }
        for (key, value) in result.models.iter().flatten() {
            extra.push_str(&format!(",\"{}\":{value}", json_escape(key)));
        }
        format!("{}{extra}}}", &base[..base.len() - 1])
    }
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

fn help_text() -> String {
    // Indentation is significant for readability; avoid `\` line-continuations
    // (they strip leading whitespace on the next line).
    format!(
        "\
tmuxlet {VERSION}

Usage:
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

Print-mode options:
  -p, --print                              Blocking programmatic mode (required for runs)
      --target <name>                      Target CLI: claude, agy, codex, opencode, pi, cursor, grok, hermes,
                                           or chat backends muse, gweb, chatgpt (default: claude)
      --output-format <text|json>          Result format (default: text; stream-json unsupported)
  -C, --cwd, --cd, --dir <dir>             Working directory
  -m, --model <model>                      Model pass-through where supported
      --thinking <level>                   gweb Thinking level: low, medium, high (or l, m, h)
      --image-out <dir|path>               gweb, chatgpt: save the reply's images there; JSON result gets `images`
      --canvas                             gweb: turn on Gemini's Canvas; JSON result gets `canvas`
      --canvas-out <path>                  gweb: also save the Canvas doc's markdown there (implies --canvas)
      --file, --attach <path>              Attach a file (repeatable) by the target's native mechanism, else
                                           inline text (256 KiB each, 768 KiB total); exit 7 if refused
  -c, --continue                           Continue latest target session where supported
  -r, --resume [id]                        Resume session (optional id) where supported
      --session <id>                       Target-native session selector where supported
      --session-id <id>                    Explicit normalized session id (target-specific)
      --system-prompt <text>               System prompt where supported
      --append-system-prompt <text>        Append system prompt (repeatable) where supported
      --add-dir, --include-directories <p> Extra dirs (CSV ok; repeatable) where supported
      --dangerously-skip-permissions       Full bypass (aliases: --yolo, --force, --dangerously-bypass-approvals-and-sandbox)
      --permission-mode <mode>             Permission mode where supported (Claude/Grok)
      --allowed-tools, --allowedTools <r>  Allow rule/tool (repeatable) where supported
      --disallowed-tools, --disallowedTools <r> Deny rule/tool (repeatable) where supported
      --tools <tools>                      Tools list (repeatable) where supported
      --mcp-config <path>                  MCP config path (repeatable) where supported
      --settings <path>                    Settings file where supported
      --target-arg <arg>                   Raw target CLI escape hatch (repeatable)
      --env KEY=VALUE                      Env var for the target CLI (repeatable), set on the run's own tmux
                                           session, not the server env; *TOKEN*/*KEY*/*SECRET* values redacted
      --ollama <model>                     claude, codex: use a model served by local Ollama (sets --model;
                                           claude gets ANTHROPIC_BASE_URL etc., codex gets --oss)
      --timeout <seconds>                  Print-mode timeout (default: 1800)
      --owner <name>                       Run owner (flag > TMUXLET_OWNER > parent-process-name@caller-cwd)
      --reap-min-age <minutes>             Auto-reap minimum run age (default 10; env TMUXLET_REAP_MIN_AGE_MIN)
      --max-concurrency <n>                Live-session cap (0=unlimited; default 24; env TMUXLET_MAX_CONCURRENCY)
      --memory-pressure-guard, --memory-guard  Queue under host memory pressure (env TMUXLET_MEMORY_GUARD)
  --                                       End options; remaining args are the prompt

Unsupported target combinations fail fast. See README for per-target mapping.
The prompt is never typed into a bare shell: if the target CLI fails to start or exits
before completing, the run aborts with exit 1 and pane.log keeps the reason.

Exit codes: 0 completed, 1 error or run not completed, 6 rate limited (gweb, chatgpt),
7 --file refused (a file the target can't take, or inlined text over the caps).

Subcommands:
  status                                   List run meta.json lines
  read <id> [--lines N]                    Capture pane (default lines: 80)
  send <id> <message>                      Paste into a run's pane
  send-all [message] [--interval s] [--enter]  Paced message/Enter to every live session (default interval: 1s)
  attach <id>                              Attach to the run's tmux session
  stop <id>                                Graceful /exit then kill session
  reap [--blocked] [--older-than s] [--dry-run] [-y|--yes]
                                           Kill stuck/leaked sessions (frees concurrency slots)
  prune [<id>...] [-y|--yes] [--dry-run]   Delete terminal runs + leftover sessions
  bridge complete --run <id> --file <path> Completion contract (used by target CLI)

Management ownership (send, send-all, stop, prune, reap):
  --owner <name>                           Caller identity (same precedence as print mode)
  --force-foreign                          Allow management of another owner's runs
  Legacy runs without owner are allowed with a warning. send-all and prune skip
  foreign runs and report a count; send, stop, and manual reap refuse them.
  status includes owner. Use -- before literal message flags.
  Auto-reap only kills old, unchanged panes and never permission/trust prompts.
  All reap actions/skips append JSON lines to <state-dir>/reap.log.

Environment:
  TMUXLET_OWNER=<name>                     Default caller/run identity
  TMUXLET_REAP_MIN_AGE_MIN=<minutes>       Auto-reap minimum run age (default 10)
  TMUXLET_HOME=<dir>                       State root (default: ~/.tmuxlet)
  TMUXLET_MAX_CONCURRENCY=<n>              Live-session cap (0=unlimited; default 24)
  TMUXLET_ACQUIRE_TIMEOUT=<seconds>        Max wait for a free slot (0=forever; default 300)
  TMUXLET_MEMORY_GUARD=<truthy>            Enable memory-pressure guard
  TMUXLET_MEMORY_MIN_FREE=<percent>        Memory-guard free% threshold (default 15)
  TMUXLET_CLAUDE_PERMISSION_MODE=<mode>    Claude default permission-mode (default: auto)
  TMUXLET_LLMCLI_BIN=<path>                llmcli program for chat backends muse/gweb/chatgpt (default: llmcli)
  TMUXLET_OLLAMA_HOST=<url>                Base URL --ollama gives claude (default: http://localhost:11434)

Examples:
  tmuxlet -p \"say ready\"
  tmuxlet --target codex -p --cwd ~/Sites/app \"say ready\"
  printf 'say ready' | tmuxlet -p -
  TMUXLET_MAX_CONCURRENCY=4 tmuxlet -p \"say ready\"
  tmuxlet -p --ollama minimax-m3.1-flash:cloud \"Reply with exactly: pong\"
  tmuxlet -p --target codex --ollama minimax-m3.1-flash:cloud \"Reply with exactly: pong\"
  tmuxlet -p --target muse \"Reply with exactly: pong\"
  tmuxlet -p --target gweb -m \"3.8 Flash\" \"Reply with exactly: pong\"
  tmuxlet -p --target gweb -m Argon --image-out ./out --output-format json \"blue stars\"
  tmuxlet -p --target gweb --file notes.md --file chart.png \"Summarize both\"
  tmuxlet -p --target gweb --canvas-out ./scene.md --output-format json \"Write a scene in Canvas\"
  tmuxlet -p --target chatgpt --image-out ./out --output-format json --timeout 900 \"blue stars\"
  tmuxlet bridge complete --run <id> --file answer.txt"
    )
}

fn print_help() {
    println!("{}", help_text());
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
    fn help_text_lists_every_print_mode_flag_and_surface() {
        let help = help_text();
        // Every user-facing print-mode option / alias accepted by parse_options.
        let required = [
            "-p",
            "--print",
            "--target",
            "--output-format",
            "-C",
            "--cwd",
            "--cd",
            "--dir",
            "-m",
            "--model",
            "--thinking",
            "--image-out",
            "--canvas",
            "--canvas-out",
            "--attach",
            "-c",
            "--continue",
            "-r",
            "--resume",
            "--session",
            "--session-id",
            "--system-prompt",
            "--append-system-prompt",
            "--add-dir",
            "--include-directories",
            "--dangerously-skip-permissions",
            "--yolo",
            "--force",
            "--dangerously-bypass-approvals-and-sandbox",
            "--permission-mode",
            "--allowed-tools",
            "--allowedTools",
            "--disallowed-tools",
            "--disallowedTools",
            "--tools",
            "--mcp-config",
            "--settings",
            "--target-arg",
            "--timeout",
            "--owner",
            "--force-foreign",
            "--reap-min-age",
            "--max-concurrency",
            "--memory-pressure-guard",
            "--memory-guard",
            // Subcommands / bridge / env
            "status",
            "read",
            "--lines",
            "send",
            "send-all",
            "--interval",
            "--enter",
            "attach",
            "stop",
            "reap",
            "--blocked",
            "--older-than",
            "--dry-run",
            "prune",
            "--yes",
            "bridge complete",
            "--run",
            "--file",
            "TMUXLET_OWNER",
            "TMUXLET_REAP_MIN_AGE_MIN",
            "TMUXLET_HOME",
            "TMUXLET_MAX_CONCURRENCY",
            "TMUXLET_ACQUIRE_TIMEOUT",
            "TMUXLET_MEMORY_GUARD",
            "TMUXLET_MEMORY_MIN_FREE",
            "TMUXLET_CLAUDE_PERMISSION_MODE",
            "TMUXLET_LLMCLI_BIN",
            "muse",
            "gweb",
        ];
        for needle in required {
            assert!(
                help.contains(needle),
                "help_text missing expected surface: {needle}"
            );
        }
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
    fn agy_grant_enables_bypass() {
        let opts = with_grant(parse(&["--target", "agy", "-p", "x"]));
        let cmd = target_command(&opts).unwrap();
        assert!(
            cmd.args
                .contains(&"--dangerously-skip-permissions".to_string())
        );
    }

    #[test]
    fn opencode_grant_uses_bypass_floor() {
        let opts = with_grant(parse(&["--target", "opencode", "-p", "x"]));
        let cmd = target_command(&opts).unwrap();
        assert!(cmd.args.contains(&"--auto".to_string()));
        // opencode 1.18 has neither flag; either one makes it print help and exit.
        assert!(
            !cmd.args
                .contains(&"--dangerously-skip-permissions".to_string())
        );
        assert!(!cmd.args.contains(&"--dir".to_string()));
    }

    #[test]
    fn opencode_launch_uses_project_positional_and_prompt_flag() {
        let mut opts = with_grant(parse(&[
            "--target",
            "opencode",
            "-m",
            "ollama/minimax-m3.1-flash:cloud",
            "-p",
            "x",
        ]));
        opts.cwd = PathBuf::from("/work/app");
        let (launch, paste) = launch_plan(
            "opencode",
            target_command(&opts).unwrap(),
            &fixed_paths(),
            "r",
            "/bin/t",
            "user prompt",
        );
        assert!(paste.is_none(), "opencode must never be pasted into");
        assert_eq!(launch.program, "opencode");
        assert_eq!(launch.args[0], "/work/app");
        assert!(has_pair(
            &launch.args,
            "--model",
            "ollama/minimax-m3.1-flash:cloud"
        ));
        let n = launch.args.len();
        assert_eq!(launch.args[n - 2], "--prompt");
        assert!(launch.args[n - 1].starts_with("user prompt\n\nTMUXLET COMPLETION CONTRACT:"));
    }

    #[test]
    fn env_flag_parses_repeatable_pairs() {
        let opts = parse(&[
            "--env",
            "A=1",
            "--env",
            "EMPTY=",
            "--env",
            "URL=http://x?a=b",
            "-p",
            "x",
        ]);
        assert_eq!(
            opts.env,
            vec![
                ("A".to_string(), "1".to_string()),
                ("EMPTY".to_string(), String::new()),
                ("URL".to_string(), "http://x?a=b".to_string()),
            ]
        );
    }

    #[test]
    fn env_flag_rejects_bad_pairs_without_echoing_values() {
        for bad in ["NOEQUALS", "=v", "1A=v", "A-B=v", "A B=v"] {
            let mut args = vec![
                "--env".to_string(),
                bad.to_string(),
                "-p".into(),
                "x".into(),
            ];
            let err = parse_options(&mut args).unwrap_err();
            assert!(err.starts_with("--env"), "{bad}: {err}");
        }
        let mut args: Vec<String> = ["--env", "sk-supersecretvalue", "-p", "x"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let err = parse_options(&mut args).unwrap_err();
        assert!(!err.contains("supersecret"), "{err}");
    }

    #[test]
    fn secret_env_values_are_redacted() {
        for key in [
            "ANTHROPIC_AUTH_TOKEN",
            "OPENAI_API_KEY",
            "my_secret",
            "DB_PASSWORD",
        ] {
            assert!(is_secret_env_key(key), "{key}");
            assert_eq!(display_env_value(key, "v"), "<redacted>");
        }
        assert!(!is_secret_env_key("ANTHROPIC_BASE_URL"));
        assert_eq!(
            display_env_value("ANTHROPIC_BASE_URL", "http://h"),
            "http://h"
        );
        // An empty secret is shown as empty: it leaks nothing and shows the unset.
        assert_eq!(display_env_value("ANTHROPIC_API_KEY", ""), "");
        let json = redacted_env_json(&[
            ("ANTHROPIC_AUTH_TOKEN".into(), "abc123".into()),
            ("MODE".into(), "x".into()),
        ]);
        assert_eq!(json["ANTHROPIC_AUTH_TOKEN"], "<redacted>");
        assert_eq!(json["MODE"], "x");
        assert!(!json.to_string().contains("abc123"));
    }

    #[test]
    fn env_rides_on_new_session_not_the_launch_line() {
        let env = vec![
            ("K".to_string(), "v 1".to_string()),
            ("E".to_string(), String::new()),
        ];
        let args: Vec<String> = new_session_args("tmuxlet-r", Path::new("/w"), &env)
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(has_pair(&args, "-e", "K=v 1"));
        assert!(has_pair(&args, "-e", "E="));
        assert!(has_pair(&args, "-c", "/w"));
        assert!(!args.iter().any(|a| a == "set-environment" || a == "-g"));

        let mut opts = parse(&["--env", "SECRET_TOKEN=s3cr3t", "-p", "x"]);
        opts.contract_grant = Some(ContractGrant {
            run_root: PathBuf::from("/r"),
            exe: "/bin/t".into(),
        });
        let (launch, _) = launch_plan(
            "claude",
            target_command(&opts).unwrap(),
            &fixed_paths(),
            "r",
            "/bin/t",
            "p",
        );
        assert!(!launch.args.iter().any(|a| a.contains("s3cr3t")));
    }

    #[test]
    fn write_meta_redacts_secret_env() {
        let dir = env::temp_dir().join(format!("tmuxlet-env-meta-{}", make_run_id()));
        fs::create_dir_all(&dir).unwrap();
        let paths = RunPaths {
            root: dir.clone(),
            meta: dir.join("meta.json"),
            prompt: dir.join("prompt.txt"),
            answer: dir.join("answer.txt"),
            complete: dir.join("complete.txt"),
            pane_log: dir.join("pane.log"),
            error: dir.join("error.txt"),
        };
        let opts = parse(&["--ollama", "minimax-m3.1-flash:cloud", "-p", "x"]);
        let env = session_env(&opts);
        let cmd = target_command(&opts).unwrap();
        write_meta(&paths, "id", "session", &opts, &cmd, &env).unwrap();
        let text = fs::read_to_string(&paths.meta).unwrap();
        let meta: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(meta["env"]["ANTHROPIC_AUTH_TOKEN"], "<redacted>");
        assert_eq!(meta["env"]["ANTHROPIC_API_KEY"], "");
        assert_eq!(meta["ollama"], "minimax-m3.1-flash:cloud");
        assert!(!text.contains("\"ANTHROPIC_AUTH_TOKEN\":\"ollama\""));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ollama_claude_sets_documented_env_and_model() {
        let opts = parse(&["--ollama", "minimax-m3.1-flash:cloud", "-p", "x"]);
        assert_eq!(opts.model.as_deref(), Some("minimax-m3.1-flash:cloud"));
        let cmd = target_command(&opts).unwrap();
        assert_eq!(cmd.program, "claude");
        assert!(has_pair(&cmd.args, "--model", "minimax-m3.1-flash:cloud"));
        let env = session_env(&opts);
        let get = |k: &str| {
            env.iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.as_str())
        };
        assert!(get("ANTHROPIC_BASE_URL").is_some_and(|v| v.starts_with("http")));
        assert_eq!(get("ANTHROPIC_AUTH_TOKEN"), Some("ollama"));
        assert_eq!(get("ANTHROPIC_API_KEY"), Some(""));
        assert_eq!(
            get("ANTHROPIC_DEFAULT_HAIKU_MODEL"),
            Some("minimax-m3.1-flash:cloud")
        );
        assert_eq!(
            get("CLAUDE_CODE_SUBAGENT_MODEL"),
            Some("minimax-m3.1-flash:cloud")
        );
    }

    #[test]
    fn ollama_claude_env_uses_given_host() {
        let env = ollama_claude_env("m", "http://h:1");
        assert!(env.contains(&("ANTHROPIC_BASE_URL".into(), "http://h:1".into())));
    }

    #[test]
    fn explicit_env_overrides_ollama_preset_in_place() {
        let opts = parse(&[
            "--ollama",
            "m",
            "--env",
            "ANTHROPIC_BASE_URL=http://other:1",
            "--env",
            "X=1",
            "-p",
            "p",
        ]);
        let env = session_env(&opts);
        assert_eq!(
            env[0],
            ("ANTHROPIC_BASE_URL".into(), "http://other:1".into())
        );
        assert_eq!(
            env.iter()
                .filter(|(k, _)| k == "ANTHROPIC_BASE_URL")
                .count(),
            1
        );
        assert_eq!(env.last().unwrap(), &("X".to_string(), "1".to_string()));
    }

    #[test]
    fn ollama_codex_uses_oss_ollama_provider() {
        let opts = parse(&[
            "--target",
            "codex",
            "--ollama",
            "minimax-m3.1-flash:cloud",
            "-p",
            "x",
        ]);
        let cmd = target_command(&opts).unwrap();
        assert!(cmd.args.contains(&"--oss".to_string()));
        assert!(has_pair(&cmd.args, "--local-provider", "ollama"));
        assert!(has_pair(&cmd.args, "--model", "minimax-m3.1-flash:cloud"));
        // codex needs no session env for its OSS provider.
        assert!(session_env(&opts).is_empty());
    }

    #[test]
    fn plain_claude_and_codex_are_unchanged_without_ollama() {
        let opts = parse(&["-p", "x"]);
        assert!(session_env(&opts).is_empty());
        let cmd = target_command(&opts).unwrap();
        assert!(!cmd.args.iter().any(|a| a.contains("ollama")));
        let opts = parse(&["--target", "codex", "-m", "gpt-5", "-p", "x"]);
        let cmd = target_command(&opts).unwrap();
        assert!(!cmd.args.contains(&"--oss".to_string()));
        assert!(!cmd.args.contains(&"--local-provider".to_string()));
        assert!(session_env(&opts).is_empty());
    }

    #[test]
    fn ollama_rejects_other_targets_and_conflicting_model() {
        let opts = parse(&["--target", "opencode", "--ollama", "m", "-p", "x"]);
        assert!(target_command(&opts).unwrap_err().contains("--ollama"));
        let mut args: Vec<String> = ["--ollama", "a", "--model", "b", "-p", "x"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(parse_options(&mut args).unwrap_err().contains("conflicts"));
        // The same model given twice is fine.
        let opts = parse(&["--ollama", "a", "--model", "a", "-p", "x"]);
        assert_eq!(opts.model.as_deref(), Some("a"));
    }

    #[test]
    fn shell_detection_covers_login_and_paths() {
        for name in ["zsh", "-zsh", "bash", "/bin/sh", "fish", "login"] {
            assert!(is_shell_command(name), "{name}");
        }
        for name in [
            "claude",
            "codex",
            "opencode",
            "node",
            "infisical",
            "2.1.304",
            "pi",
        ] {
            assert!(!is_shell_command(name), "{name}");
        }
    }

    #[test]
    fn start_guard_waits_for_a_stable_target_then_gives_up() {
        assert_eq!(start_watch_step(Some("zsh"), 0, 500), StartWatch::Waiting);
        assert_eq!(
            start_watch_step(Some("opencode"), 300, 800),
            StartWatch::Waiting
        );
        assert_eq!(
            start_watch_step(Some("opencode"), TARGET_STABLE_MS, 4000),
            StartWatch::Started
        );
        assert_eq!(
            start_watch_step(Some("zsh"), 0, TARGET_START_SECS * 1000),
            StartWatch::NeverStarted
        );
        assert_eq!(
            start_watch_step(None, 0, TARGET_START_SECS * 1000),
            StartWatch::NeverStarted
        );
    }

    #[test]
    fn not_running_messages_say_the_prompt_was_withheld() {
        let start = target_not_running_message("opencode", Some("zsh"), true);
        assert!(start.contains("did not start") && start.contains("prompt was not sent"));
        let exit = target_not_running_message("claude", Some("zsh"), false);
        assert!(exit.contains("exited before completing") && exit.contains("aborted"));
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
    fn grok_maps_core_flags_and_session_controls() {
        let opts = parse(&[
            "--target",
            "grok",
            "--model",
            "grok-4",
            "--cwd",
            "/tmp",
            "--continue",
            "-p",
            "x",
        ]);
        let cmd = target_command(&opts).unwrap();
        assert_eq!(cmd.program, "grok");
        assert!(has_pair(&cmd.args, "--model", "grok-4"));
        assert!(has_pair(&cmd.args, "--cwd", "/tmp"));
        assert!(cmd.args.contains(&"--continue".to_string()));

        let opts = parse(&["--target", "grok", "--resume", "-p", "x"]);
        let cmd = target_command(&opts).unwrap();
        assert!(cmd.args.contains(&"--resume".to_string()));
        // value-less --resume stays flag-only (most recent session).
        let resume_idx = cmd.args.iter().position(|a| a == "--resume").unwrap();
        assert!(resume_idx + 1 == cmd.args.len() || cmd.args[resume_idx + 1].starts_with('-'));

        let opts = parse(&["--target", "grok", "--resume", "ses_abc", "-p", "x"]);
        let cmd = target_command(&opts).unwrap();
        assert!(has_pair(&cmd.args, "--resume", "ses_abc"));

        // --session-id is create-new UUID semantics on grok, not resume.
        let opts = parse(&[
            "--target",
            "grok",
            "--session-id",
            "01984d2f-0000-0000-0000-000000000001",
            "-p",
            "x",
        ]);
        let cmd = target_command(&opts).unwrap();
        assert!(has_pair(
            &cmd.args,
            "--session-id",
            "01984d2f-0000-0000-0000-000000000001"
        ));
        assert!(!cmd.args.contains(&"--resume".to_string()));
    }

    #[test]
    fn grok_grant_injects_scoped_allow_rules() {
        let opts = with_grant(parse(&["--target", "grok", "-p", "x"]));
        let cmd = target_command(&opts).unwrap();
        assert_eq!(cmd.program, "grok");
        assert!(has_pair(
            &cmd.args,
            "--allow",
            "Write(//home/u/.tmuxlet/runs/abc/**)"
        ));
        assert!(has_pair(
            &cmd.args,
            "--allow",
            "Edit(//home/u/.tmuxlet/runs/abc/**)"
        ));
        assert!(has_pair(
            &cmd.args,
            "--allow",
            "Bash(/usr/local/bin/tmuxlet bridge complete:*)"
        ));
        assert!(!cmd.args.contains(&"--always-approve".to_string()));
    }

    #[test]
    fn grok_full_bypass_uses_always_approve_and_suppresses_grant() {
        let opts = with_grant(parse(&["--target", "grok", "--yolo", "-p", "x"]));
        let cmd = target_command(&opts).unwrap();
        assert!(cmd.args.contains(&"--always-approve".to_string()));
        assert!(!cmd.args.contains(&"--allow".to_string()));
        assert!(!cmd.args.contains(&"--permission-mode".to_string()));
    }

    #[test]
    fn grok_maps_permission_mode_and_system_prompt() {
        let opts = parse(&[
            "--target",
            "grok",
            "--permission-mode",
            "bypassPermissions",
            "--system-prompt",
            "be terse",
            "-p",
            "x",
        ]);
        let cmd = target_command(&opts).unwrap();
        assert!(has_pair(
            &cmd.args,
            "--permission-mode",
            "bypassPermissions"
        ));
        assert!(has_pair(&cmd.args, "--system-prompt-override", "be terse"));
    }

    #[test]
    fn grok_rejects_unsupported_flags() {
        let opts = parse(&["--target", "grok", "--session", "abc", "-p", "x"]);
        assert!(target_command(&opts).is_err());

        let opts = parse(&["--target", "grok", "--add-dir", "/extra", "-p", "x"]);
        assert!(target_command(&opts).is_err());

        let opts = parse(&["--target", "grok", "--settings", "s.json", "-p", "x"]);
        assert!(target_command(&opts).is_err());
    }

    #[test]
    fn grok_launch_receives_final_prompt_argument() {
        let opts = parse(&["--target", "grok", "--model", "grok-4", "-p", "x"]);
        let (cmd, prompt_in_launch) =
            command_with_prompt(target_command(&opts).unwrap(), &opts.target, "final prompt");
        assert!(prompt_in_launch);
        assert_eq!(cmd.args.last().map(String::as_str), Some("final prompt"));
        assert_eq!(cmd.args.iter().rev().nth(1).map(String::as_str), Some("--"));
        assert_eq!(cmd.program, "grok");
        // Must not use grok's headless -p/--single path.
        assert!(!cmd.args.iter().any(|a| a == "-p" || a == "--single"));
    }

    #[test]
    fn hermes_maps_core_flags_and_session_controls() {
        let opts = parse(&[
            "--target",
            "hermes",
            "--model",
            "hermes-4",
            "--continue",
            "-p",
            "x",
        ]);
        let cmd = target_command(&opts).unwrap();
        assert_eq!(cmd.program, "hermes");
        // Always drives the classic REPL, never the full-screen TUI.
        assert!(cmd.args.contains(&"--cli".to_string()));
        assert!(has_pair(&cmd.args, "--model", "hermes-4"));
        assert!(cmd.args.contains(&"--continue".to_string()));

        // Value-less --resume (latest) maps to --continue (hermes --resume needs an id).
        let opts = parse(&["--target", "hermes", "--resume", "-p", "x"]);
        let cmd = target_command(&opts).unwrap();
        assert!(cmd.args.contains(&"--continue".to_string()));
        assert!(!cmd.args.contains(&"--resume".to_string()));

        // --resume <id> resumes by id/title.
        let opts = parse(&["--target", "hermes", "--resume", "ses_abc", "-p", "x"]);
        let cmd = target_command(&opts).unwrap();
        assert!(has_pair(&cmd.args, "--resume", "ses_abc"));

        // --session-id maps to --resume <id> (no create-with-id flag on hermes).
        let opts = parse(&["--target", "hermes", "--session-id", "ses_xyz", "-p", "x"]);
        let cmd = target_command(&opts).unwrap();
        assert!(has_pair(&cmd.args, "--resume", "ses_xyz"));
        assert!(!cmd.args.contains(&"--continue".to_string()));
    }

    #[test]
    fn hermes_agent_alias_resolves_to_hermes() {
        let opts = parse(&["--target", "hermes-agent", "-p", "x"]);
        let cmd = target_command(&opts).unwrap();
        assert_eq!(cmd.program, "hermes");
    }

    #[test]
    fn hermes_grant_uses_yolo_and_no_scoped_rules() {
        // No grant, no bypass: nothing to auto-approve, so no --yolo.
        let opts = parse(&["--target", "hermes", "-p", "x"]);
        let cmd = target_command(&opts).unwrap();
        assert!(!cmd.args.contains(&"--yolo".to_string()));

        // Contract grant present: broad --yolo (hermes has no scoped allow/deny tier).
        let opts = with_grant(parse(&["--target", "hermes", "-p", "x"]));
        let cmd = target_command(&opts).unwrap();
        assert_eq!(cmd.program, "hermes");
        assert!(cmd.args.contains(&"--yolo".to_string()));
        assert!(
            !cmd.args
                .iter()
                .any(|a| a == "--allow" || a == "--allowed-tools")
        );
    }

    #[test]
    fn hermes_full_bypass_uses_yolo() {
        let opts = with_grant(parse(&["--target", "hermes", "--yolo", "-p", "x"]));
        let cmd = target_command(&opts).unwrap();
        assert!(cmd.args.contains(&"--yolo".to_string()));
    }

    #[test]
    fn hermes_rejects_unsupported_flags() {
        let cases: &[&[&str]] = &[
            &["--target", "hermes", "--session", "abc", "-p", "x"],
            &["--target", "hermes", "--add-dir", "/extra", "-p", "x"],
            &["--target", "hermes", "--settings", "s.json", "-p", "x"],
            &["--target", "hermes", "--permission-mode", "auto", "-p", "x"],
            &[
                "--target",
                "hermes",
                "--system-prompt",
                "be terse",
                "-p",
                "x",
            ],
            &["--target", "hermes", "--allowed-tools", "Bash", "-p", "x"],
            &["--target", "hermes", "--mcp-config", "m.json", "-p", "x"],
        ];
        for case in cases {
            let opts = parse(case);
            assert!(
                target_command(&opts).is_err(),
                "expected error for {case:?}"
            );
        }
    }

    #[test]
    fn hermes_prompt_is_typed_not_positional() {
        // hermes -p is --profile, not a prompt flag: the prompt must be typed into
        // the REPL, never appended as a positional launch argument.
        let opts = parse(&["--target", "hermes", "-p", "x"]);
        let (cmd, prompt_in_launch) =
            command_with_prompt(target_command(&opts).unwrap(), &opts.target, "final prompt");
        assert!(!prompt_in_launch);
        assert_eq!(cmd.program, "hermes");
        assert!(!cmd.args.iter().any(|a| a == "final prompt"));
        assert!(!cmd.args.iter().any(|a| a == "--"));
        assert!(!cmd.args.iter().any(|a| a == "-p" || a == "--profile"));
    }

    fn fixed_paths() -> RunPaths {
        let root = PathBuf::from("/home/u/.tmuxlet/runs/abc");
        RunPaths {
            meta: root.join("meta.json"),
            prompt: root.join("prompt.txt"),
            answer: root.join("answer.txt"),
            complete: root.join("complete.txt"),
            pane_log: root.join("pane.log"),
            error: root.join("error.txt"),
            root,
        }
    }

    fn chat_argv<'a>(target: &'a str, flags: &[&'a str]) -> Vec<&'a str> {
        let mut argv = vec!["--target", target];
        argv.extend(flags);
        argv.extend(["-p", "x"]);
        argv
    }

    #[test]
    fn chat_targets_launch_print_mode_with_lock_wait() {
        // Chat backends run `llmcli <site> ...`; the site is the first arg.
        for (target, site) in [
            ("muse", "muse"),
            ("musecli", "muse"),
            ("gweb", "gemini"),
            ("gemini-web", "gemini"),
            ("chatgpt", "chatgpt"),
            ("cgpt", "chatgpt"),
        ] {
            let opts = with_grant(parse(&["--target", target, "-p", "x"]));
            let cmd = target_command(&opts).unwrap();
            // Only print-mode flags: the contract grant adds nothing, and tmuxlet's
            // default timeout is not forwarded (the CLI keeps its own).
            assert_eq!(cmd.args, [site, "--lock-wait", "1800", "-p", "--quiet"]);
        }
    }

    #[test]
    fn chat_target_aliases_share_a_program() {
        let program = |t: &str| {
            target_command(&parse(&["--target", t, "-p", "x"]))
                .unwrap()
                .program
        };
        assert_eq!(program("muse"), program("musecli"));
        assert_eq!(program("gweb"), program("gemini-web"));
        assert_eq!(program("chatgpt"), program("cgpt"));
        // Every chat backend now runs through the shared `llmcli` binary.
        if env::var_os("TMUXLET_LLMCLI_BIN").is_none() {
            for t in ["muse", "gweb", "chatgpt"] {
                assert_eq!(program(t), "llmcli");
            }
        }
    }

    #[test]
    fn llmcli_site_aliases_resolve_to_chat_backends() {
        // `--target llmcli:<site>` is an alias for the matching chat backend and
        // launches `llmcli <site> --lock-wait ... -p ...`.
        for (alias, site) in [
            ("llmcli:gemini", "gemini"),
            ("llmcli:chatgpt", "chatgpt"),
            ("llmcli:muse", "muse"),
        ] {
            let cmd = target_command(&parse(&["--target", alias, "-p", "x"])).unwrap();
            assert_eq!(cmd.args[0], site, "{alias}");
            assert_eq!(cmd.args[1], "--lock-wait", "{alias}");
            assert!(cmd.args.contains(&"-p".to_string()), "{alias}");
            if env::var_os("TMUXLET_LLMCLI_BIN").is_none() {
                assert_eq!(cmd.program, "llmcli", "{alias}");
            }
        }
    }

    #[test]
    fn chat_targets_map_session_controls_to_chat() {
        for target in ["muse", "gweb", "chatgpt", "cgpt"] {
            for flags in [
                ["--resume", "c1"],
                ["--session-id", "c1"],
                ["--session", "c1"],
            ] {
                let argv = chat_argv(target, &flags);
                let cmd = target_command(&parse(&argv)).unwrap();
                assert!(has_pair(&cmd.args, "--chat", "c1"), "{argv:?}");
                assert!(!cmd.args.contains(&"-c".to_string()), "{argv:?}");
            }
            for flag in ["--continue", "-c", "--resume"] {
                let cmd = target_command(&parse(&chat_argv(target, &[flag]))).unwrap();
                assert!(cmd.args.contains(&"-c".to_string()), "{flag}");
                assert!(!cmd.args.contains(&"--chat".to_string()), "{flag}");
            }
        }
    }

    #[test]
    fn chat_targets_forward_explicit_timeout_and_target_args() {
        let argv = chat_argv("muse", &["--timeout", "90", "--target-arg", "--headed"]);
        let cmd = target_command(&parse(&argv)).unwrap();
        assert!(has_pair(&cmd.args, "--lock-wait", "90"));
        assert!(has_pair(&cmd.args, "--timeout", "90"));
        assert_eq!(cmd.args.last().map(String::as_str), Some("--headed"));
    }

    #[test]
    fn gweb_passes_model_and_muse_rejects_it() {
        let cmd = target_command(&parse(&chat_argv("gweb", &["-m", "3.8 Flash"]))).unwrap();
        assert!(has_pair(&cmd.args, "--model", "3.8 Flash"));

        let err = target_command(&parse(&chat_argv("muse", &["-m", "x"]))).unwrap_err();
        assert!(err.contains("Muse has no model picker"), "{err}");
    }

    #[test]
    fn gweb_passes_thinking_and_other_targets_reject_it() {
        let cmd = target_command(&parse(&chat_argv(
            "gweb",
            &["-m", "pro", "--thinking", "high"],
        )))
        .unwrap();
        assert!(has_pair(&cmd.args, "--model", "pro"));
        assert!(has_pair(&cmd.args, "--thinking", "high"));

        let cmd = target_command(&parse(&chat_argv("gweb", &[]))).unwrap();
        assert!(!cmd.args.contains(&"--thinking".to_string()));

        for target in ["muse", "claude", "codex", "agy"] {
            let argv = chat_argv(target, &["--thinking", "high"]);
            let err = target_command(&parse(&argv)).unwrap_err();
            assert_eq!(
                err,
                format!(
                    "{target} does not support normalized flag --thinking (only gweb has a thinking level)"
                ),
                "{argv:?}"
            );
        }
    }

    #[test]
    fn gweb_forwards_image_out_as_an_absolute_path_with_json_output() {
        let argv = chat_argv("gweb", &["-C", "/work", "--image-out", "out/"]);
        let cmd = target_command(&parse(&argv)).unwrap();
        assert!(
            has_pair(&cmd.args, "--image-out", "/work/out/"),
            "{:?}",
            cmd.args
        );
        assert!(has_pair(&cmd.args, "--output-format", "json"));

        let argv = chat_argv(
            "gemini-web",
            &["--image-out", "/tmp/gweb-image-test/stars.png"],
        );
        let cmd = target_command(&parse(&argv)).unwrap();
        assert!(has_pair(
            &cmd.args,
            "--image-out",
            "/tmp/gweb-image-test/stars.png"
        ));

        if let Some(home) = env::var_os("HOME") {
            let cmd =
                target_command(&parse(&chat_argv("gweb", &["--image-out", "~/pics"]))).unwrap();
            let want = PathBuf::from(home).join("pics").display().to_string();
            assert!(has_pair(&cmd.args, "--image-out", &want));
        }

        let cmd = target_command(&parse(&chat_argv("gweb", &[]))).unwrap();
        assert!(!cmd.args.contains(&"--image-out".to_string()));
        assert!(!cmd.args.contains(&"--output-format".to_string())); // plain runs keep text answers
    }

    #[test]
    fn image_out_is_rejected_for_other_targets() {
        for target in ["muse", "claude", "codex", "agy", "opencode"] {
            let argv = chat_argv(target, &["--image-out", "/tmp/out"]);
            let err = target_command(&parse(&argv)).unwrap_err();
            assert_eq!(
                err,
                format!(
                    "{target} does not support normalized flag --image-out (only gweb and chatgpt save images)"
                ),
                "{argv:?}"
            );
        }
    }

    #[test]
    fn gweb_image_result_turns_gweb_json_into_text_and_images() {
        let image = serde_json::json!({"path": "/tmp/tm/blue-stars-1.png", "width": 2752, "height": 1536,
            "bytes": 3098113, "mime": "image/png", "source_url_kind": "full_size"});
        let gweb = serde_json::json!({"chatId": "16047677facb5f88", "answer": "", "imageCount": 1,
            "images": [image.clone()]});
        let (output, images, _) = gweb_result(&format!("{gweb}\n"), true, false);
        assert_eq!(output, "/tmp/tm/blue-stars-1.png\n");
        assert_eq!(images, Some(serde_json::json!([image])));

        let with_text =
            serde_json::json!({"answer": "Here you go.", "images": [image.clone(), image]});
        let (output, _, _) = gweb_result(&with_text.to_string(), true, false);
        assert_eq!(
            output,
            "Here you go.\n/tmp/tm/blue-stars-1.png\n/tmp/tm/blue-stars-1.png\n"
        );

        for raw in [
            "plain answer\n",
            "{\"answer\": \"no images field\"}",
            "[1, 2]",
        ] {
            assert_eq!(
                gweb_result(raw, true, false),
                (raw.to_string(), None, None),
                "{raw}"
            );
        }
    }

    #[test]
    fn result_json_carries_gweb_images() {
        let images = serde_json::json!([{"path": "/tmp/tm/a-1.png", "width": 2, "height": 1,
            "bytes": 9, "mime": "image/png", "source_url_kind": "full_size"}]);
        let mut result = RunResult {
            id: "img".into(),
            target: "gweb".into(),
            status: "completed".into(),
            output: "/tmp/tm/a-1.png\n".into(),
            images: Some(images.clone()),
            canvas: None,
            citations: None,
            models: None,
            output_format: OutputFormat::Json,
            cwd: PathBuf::from("/tmp"),
            tmux_session: "tmuxlet-img".into(),
            completion_source: "explicit".into(),
            elapsed_ms: 5,
        };
        let json: serde_json::Value = serde_json::from_str(&result_json(&result)).unwrap();
        assert_eq!(json["images"], images);
        assert_eq!(json["output"], "/tmp/tm/a-1.png\n");
        assert_eq!(json["status"], "completed");
        result.images = None;
        let json: serde_json::Value = serde_json::from_str(&result_json(&result)).unwrap();
        assert!(json.get("images").is_none());
    }

    #[test]
    fn chat_targets_reject_agent_only_flags() {
        let cases: &[&[&str]] = &[
            &["--system-prompt", "be terse"],
            &["--append-system-prompt", "be terse"],
            &["--add-dir", "/extra"],
            &["--allowed-tools", "Bash"],
            &["--disallowed-tools", "Bash"],
            &["--tools", "Bash"],
            &["--mcp-config", "m.json"],
            &["--settings", "s.json"],
            &["--permission-mode", "auto"],
            &["--dangerously-skip-permissions"],
        ];
        for target in ["muse", "gweb", "chatgpt"] {
            for flags in cases {
                let argv = chat_argv(target, flags);
                let err = target_command(&parse(&argv)).unwrap_err();
                assert!(
                    err.starts_with(&format!("{target} does not support")),
                    "{argv:?}: {err}"
                );
            }
        }
    }

    #[test]
    fn chat_launch_sends_raw_prompt_file_and_self_completes() {
        let base = target_command(&parse(&chat_argv("muse", &["--continue"]))).unwrap();
        let program = base.program.clone();
        let (launch, paste) = launch_plan(
            "muse",
            base,
            &fixed_paths(),
            "run-1",
            "/bin/tmuxlet",
            "user prompt",
        );
        assert!(paste.is_none());
        // `exec` and `sh` are separate words: the pane's shell is replaced, so the
        // pane exits with the wrapper and a failed run ends `exited`.
        assert_eq!(launch.program, "exec");
        assert_eq!(launch.args[..2], ["sh", "-c"]);
        assert_eq!(
            launch.args[2],
            format!(
                "r=/home/u/.tmuxlet/runs/abc; {program} muse --lock-wait 1800 -p --quiet -c \
--prompt-file \"$r/prompt.txt\" --output-file \"$r/answer.txt\" 2> \"$r/stderr.txt\" \
&& TMUXLET_HOME=\"$r/../..\" /bin/tmuxlet bridge complete --run run-1 --file \"$r/answer.txt\" 2>> \"$r/stderr.txt\" \
|| {{ c=$?; cp \"$r/stderr.txt\" \"$r/pane.log\"; printf '%s\\n' \"$c\" > \"$r/exit-code.txt\"; exit \"$c\"; }}"
            )
        );
        assert!(!launch.args[2].contains("COMPLETION CONTRACT"));
        assert!(!launch.args[2].contains("user prompt"));
    }

    #[test]
    fn chat_wrapper_preserves_backend_exit_and_stderr() {
        for code in [5, 6] {
            let root = env::temp_dir().join(format!("tmuxlet-quota-{}", make_run_id()));
            fs::create_dir_all(&root).unwrap();
            let mut paths = fixed_paths();
            paths.root = root.clone();
            let payload = serde_json::json!({
                "error": "rate_limited", "code": code,
                "message": "You've reached your limit.\nYour limit resets at 1:53 PM",
                "resets_at": "2026-10-16T13:53:00-05:00"
            })
            .to_string();
            let cli = TargetCommand {
                program: "/bin/sh".into(),
                args: vec![
                    "-c".into(),
                    format!(
                        "test \"$GWEB_FORMAT\" = json || exit 99; printf '%s\\n' {} >&2; exit {code}",
                        shell_quote(&payload)
                    ),
                ],
            };
            let launch = chat_launch_command(
                &cli,
                &paths,
                "run",
                "/must-not-call-bridge",
                Some("GWEB_FORMAT"),
            );
            let status = Command::new("sh").args(&launch.args[1..]).status().unwrap();
            assert_eq!(status.code(), Some(code));
            assert_eq!(
                fs::read_to_string(root.join("exit-code.txt"))
                    .unwrap()
                    .trim(),
                code.to_string()
            );
            assert_eq!(
                fs::read_to_string(root.join("pane.log")).unwrap().trim(),
                payload
            );
            for target in ["gweb", "gemini-web", "chatgpt", "cgpt"] {
                assert_eq!(
                    chat_failure_status(target, &root),
                    if code == 6 { "rate_limited" } else { "exited" }
                );
            }
            assert_eq!(chat_failure_status("muse", &root), "exited");
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn rate_limit_json_preserves_original_message_and_nullable_reset() {
        for reset in [
            serde_json::Value::Null,
            serde_json::json!("2026-10-16T13:53:00-05:00"),
        ] {
            let message = "You've reached your limit.\nReset \"soon\" \u{2014} quota exceeded";
            let payload = serde_json::json!({"error": "rate_limited", "code": 6, "message": message, "resets_at": reset});
            let result = RunResult {
                id: "rate-test".into(),
                target: "gweb".into(),
                status: "rate_limited".into(),
                output: format!("{}\n", payload),
                images: None,
                canvas: None,
                citations: None,
                models: None,
                output_format: OutputFormat::Json,
                cwd: PathBuf::from("/tmp"),
                tmux_session: "test".into(),
                completion_source: "fallback".into(),
                elapsed_ms: 1,
            };
            assert_eq!(rate_limit_error(&result), payload);
            let json: serde_json::Value = serde_json::from_str(&result_json(&result)).unwrap();
            assert_eq!(json["status"], "rate_limited");
            for key in ["error", "code", "message", "resets_at"] {
                assert_eq!(json[key], payload[key]);
            }
            assert_eq!(result_status_code(&result.status), 6);
            assert_eq!(result_status_code("exited"), 1);
        }
    }

    #[test]
    fn chatgpt_passes_model_files_and_image_out_with_json_output() {
        let dir = attach_dir();
        let argv = [
            "--target",
            "chatgpt",
            "-C",
            dir.to_str().unwrap(),
            "-m",
            "gpt-6-thinking",
            "--image-out",
            "out/",
            "-p",
            "x",
        ];
        let mut opts = parse(&argv);
        opts.attachments = plan("chatgpt", &["pic.png"], &dir).unwrap();
        let cmd = target_command(&opts).unwrap();
        let pic = dir.join("pic.png").canonicalize().unwrap();
        assert_eq!(
            cmd.args,
            [
                "chatgpt".to_string(),
                "--lock-wait".into(),
                "1800".into(),
                "-p".into(),
                "--quiet".into(),
                "--model".into(),
                "gpt-6-thinking".into(),
                "--file".into(),
                pic.display().to_string(),
                "--image-out".into(),
                format!("{}/out/", dir.display()),
                "--output-format".into(),
                "json".into(),
            ]
        );
        let cmd = target_command(&parse(&chat_argv("cgpt", &[]))).unwrap();
        assert!(!cmd.args.contains(&"--output-format".to_string())); // plain runs keep text answers
        let cmd = target_command(&parse(&chat_argv("cgpt", &["--output-format", "json"]))).unwrap();
        assert!(has_pair(&cmd.args, "--output-format", "json")); // for citations and the model fields
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn chatgpt_rejects_thinking_and_canvas() {
        for (flags, why) in [
            (
                &["--thinking", "high"][..],
                "--thinking (only gweb has a thinking level)",
            ),
            (
                &["--canvas"][..],
                "--canvas (only gweb has Gemini's Canvas)",
            ),
            (
                &["--canvas-out", "c.md"][..],
                "--canvas-out (only gweb has Gemini's Canvas)",
            ),
        ] {
            let err = target_command(&parse(&chat_argv("chatgpt", flags))).unwrap_err();
            assert_eq!(
                err,
                format!("chatgpt does not support normalized flag {why}")
            );
        }
    }

    #[test]
    fn chatgpt_launch_forces_json_errors() {
        let base = target_command(&parse(&chat_argv("chatgpt", &[]))).unwrap();
        let (launch, paste) = launch_plan("cgpt", base, &fixed_paths(), "r", "/bin/t", "p");
        assert!(paste.is_none());
        assert!(
            launch.args[2].contains("; CGPT_FORMAT=json "),
            "{}",
            launch.args[2]
        );
        let base = target_command(&parse(&chat_argv("muse", &[]))).unwrap();
        let (launch, _) = launch_plan("muse", base, &fixed_paths(), "r", "/bin/t", "p");
        assert!(!launch.args[2].contains("_FORMAT=json"));
    }

    #[test]
    fn chat_json_results_carry_model_fields_in_order() {
        let cgpt = serde_json::json!({"chatId": "c", "answer": "pong", "requestedModel": "gpt-6-thinking",
            "servedModel": "gpt-6-t-mini", "servedModelDisplay": null, "fallback": true,
            "fallbackFrom": "gpt-6-thinking", "fallbackTo": "gpt-6-t-mini", "imageCount": 0, "images": []});
        let fields = model_fields(&cgpt.to_string()).unwrap();
        assert_eq!(
            fields
                .iter()
                .map(|(key, _)| key.as_str())
                .collect::<Vec<_>>(),
            [
                "requestedModel",
                "servedModel",
                "servedModelDisplay",
                "fallback",
                "fallbackFrom",
                "fallbackTo"
            ]
        );
        // gweb's served-model fields pass through too; a result without any has none.
        let gweb = serde_json::json!({"answer": "x", "servedModel": "gemini-4-argon", "servedModelId": "id"});
        assert_eq!(model_fields(&gweb.to_string()).unwrap().len(), 2);
        assert!(model_fields(r#"{"answer": "x"}"#).is_none());
        assert!(model_fields("plain text").is_none());

        let result = RunResult {
            id: "m".into(),
            target: "chatgpt".into(),
            status: "completed".into(),
            output: "pong\n".into(),
            images: Some(serde_json::json!([])),
            canvas: None,
            citations: Some(serde_json::json!([])),
            models: Some(fields),
            output_format: OutputFormat::Json,
            cwd: PathBuf::from("/tmp"),
            tmux_session: "t".into(),
            completion_source: "explicit".into(),
            elapsed_ms: 1,
        };
        let text = result_json(&result);
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(json["requestedModel"], "gpt-6-thinking");
        assert_eq!(json["servedModel"], "gpt-6-t-mini");
        assert_eq!(json["fallback"], true);
        assert!(text.find("\"citations\"").unwrap() < text.find("\"requestedModel\"").unwrap());
    }

    #[test]
    fn chatgpt_image_result_turns_cgpt_json_into_text_and_images() {
        let image = serde_json::json!({"path": "/tmp/tm/stars-1.png", "width": 1254, "height": 1254,
            "bytes": 1200158, "mime": "image/png", "original": true, "sourceWidth": 1254,
            "sourceHeight": 1254, "source": "original"});
        let cgpt = serde_json::json!({"chatId": "c", "answer": "", "imageCount": 1, "images": [image.clone()],
            "servedModel": "gpt-5-4-thinking"});
        let (output, images, canvas) = gweb_result(&format!("{cgpt}\n"), true, false);
        assert_eq!(output, "/tmp/tm/stars-1.png\n");
        assert_eq!(images, Some(serde_json::json!([image])));
        assert!(canvas.is_none());
    }

    #[test]
    fn strict_model_fallback_keeps_its_own_error() {
        let payload = serde_json::json!({"error": "model_fallback", "code": 6,
            "message": "Asked for gpt-6-thinking, but ChatGPT answered with gpt-6-t-mini.",
            "requestedModel": "gpt-6-thinking", "servedModel": "gpt-6-t-mini", "hint": "h"});
        let result = RunResult {
            id: "f".into(),
            target: "chatgpt".into(),
            status: "rate_limited".into(),
            output: format!("{payload}\n"),
            images: None,
            canvas: None,
            citations: None,
            models: None,
            output_format: OutputFormat::Json,
            cwd: PathBuf::from("/tmp"),
            tmux_session: "t".into(),
            completion_source: "fallback".into(),
            elapsed_ms: 1,
        };
        let json: serde_json::Value = serde_json::from_str(&result_json(&result)).unwrap();
        assert_eq!(json["error"], "model_fallback");
        assert_eq!(json["code"], 6);
        assert_eq!(json["servedModel"], "gpt-6-t-mini");
        assert_eq!(json["requestedModel"], "gpt-6-thinking");
    }

    #[test]
    fn agent_launch_wraps_prompt_in_contract() {
        let contract = "user prompt\n\nTMUXLET COMPLETION CONTRACT:";
        let base = target_command(&parse(&["-p", "x"])).unwrap();
        let (launch, paste) =
            launch_plan("claude", base, &fixed_paths(), "r", "/bin/t", "user prompt");
        assert!(paste.is_none());
        assert!(launch.args.last().unwrap().starts_with(contract));

        let base = target_command(&parse(&["--target", "pi", "-p", "x"])).unwrap();
        let (_, paste) = launch_plan("pi", base, &fixed_paths(), "r", "/bin/t", "user prompt");
        assert!(paste.unwrap().starts_with(contract));
    }

    #[test]
    fn agy_maps_session_controls() {
        let opts = parse(&["--target", "agy", "--continue", "-p", "x"]);
        let cmd = target_command(&opts).unwrap();
        assert!(cmd.args.contains(&"--continue".to_string()));

        let opts = parse(&["--target", "agy", "--resume", "-p", "x"]);
        let cmd = target_command(&opts).unwrap();
        assert!(cmd.args.contains(&"--continue".to_string()));

        let opts = parse(&["--target", "agy", "--resume", "abc", "-p", "x"]);
        let cmd = target_command(&opts).unwrap();
        assert!(has_pair(&cmd.args, "--conversation", "abc"));

        let opts = parse(&["--target", "agy", "--session-id", "sid", "-p", "x"]);
        let cmd = target_command(&opts).unwrap();
        assert!(has_pair(&cmd.args, "--conversation", "sid"));
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
    fn codex_always_skips_daemon_and_update_check() {
        let cmd = target_command(&parse(&["--target", "codex", "-p", "x"])).unwrap();
        assert_eq!(
            cmd.args[..3],
            ["--no-daemon", "-c", "check_for_update_on_startup=false"]
        );

        let opts = parse(&["--target", "codex", "--continue", "-p", "x"]);
        let cmd = target_command(&opts).unwrap();
        assert_eq!(
            cmd.args[..5],
            [
                "resume",
                "--last",
                "--no-daemon",
                "-c",
                "check_for_update_on_startup=false"
            ]
        );
    }

    #[test]
    fn codex_trusts_cwd_only_with_force_or_under_sites() {
        let root = env::temp_dir().join(make_run_id());
        let home = root.join("home");
        let app = home.join("Sites/app");
        let lookalike = home.join("SitesOld/app");
        let elsewhere = root.join("tmp.d");
        for dir in [&app, &lookalike, &elsewhere] {
            fs::create_dir_all(dir).unwrap();
        }
        let canonical = |dir: &Path| fs::canonicalize(dir).unwrap();

        assert_eq!(
            codex_trusted_dir(&app, Some(&home), false),
            Some(canonical(&app))
        );
        assert_eq!(codex_trusted_dir(&lookalike, Some(&home), false), None);
        assert_eq!(codex_trusted_dir(&elsewhere, Some(&home), false), None);
        assert_eq!(codex_trusted_dir(&elsewhere, None, false), None);
        assert_eq!(
            codex_trusted_dir(&elsewhere, Some(&home), true),
            Some(canonical(&elsewhere))
        );

        let cwd = elsewhere.display().to_string();
        let opts = parse(&["--target", "codex", "--force", "--cwd", &cwd, "-p", "x"]);
        let cmd = target_command(&opts).unwrap();
        let trust = format!(
            "projects={{\"{}\"={{trust_level=\"trusted\"}}}}",
            canonical(&elsewhere).display()
        );
        assert!(has_pair(&cmd.args, "-c", &trust), "{:?}", cmd.args);

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn toml_string_escapes_backslashes_and_quotes() {
        assert_eq!(toml_string(r#"/a\b "c".d"#), r#""/a\\b \"c\".d""#);
        assert_eq!(toml_string("/private/tmp/x"), "\"/private/tmp/x\"");
    }

    #[test]
    fn codex_startup_dialogs_block_with_a_hint() {
        let trust = "\n  Folder access\n  /private/tmp/x\n\n  \
Trust this folder? Codex can read, edit, and run files here, subject to your permission settings.\n\n\
› 1. Trust and continue\n  2. Quit\n\n  enter continue · esc quit\n";
        assert_eq!(
            codex_startup_state(trust, "run-1"),
            CodexStartup::Blocked(
                "codex startup blocked by interactive prompt: Trust this folder? Codex can read, \
edit, and run files here, subject to your permission settings. | › 1. Trust and continue | \
2. Quit; hint: pass --force, or run under ~/Sites, so tmuxlet trusts this folder for the run"
                    .into()
            )
        );

        // The dialog prints the cwd; chat-UI words inside a path don't count.
        let odd_cwd = trust.replace("/private/tmp/x", "/tmp/? for shortcuts/context left");
        assert!(matches!(
            codex_startup_state(&odd_cwd, "run-1"),
            CodexStartup::Blocked(_)
        ));

        let daemon = "  Background server has incompatible feature settings\n\
  Restart will use these shared feature settings:\n    memories = true\n\n\
› 1. Restart with these settings\n  2. Run without daemon this time\n";
        let CodexStartup::Blocked(err) = codex_startup_state(daemon, "run-1") else {
            panic!("daemon prompt not detected");
        };
        assert!(err.contains("Background server has incompatible feature settings"));
        assert!(err.ends_with("hint: tmuxlet already passes --no-daemon; upgrade the codex CLI to match the background server"));

        let update = "  ✨ Update available! 0.159.3 -> 0.163.0\n\n\
› 1. Update now (runs `brew upgrade codex`)\n  2. Skip\n  3. Skip until next version\n";
        let CodexStartup::Blocked(err) = codex_startup_state(update, "run-1") else {
            panic!("update prompt not detected");
        };
        assert!(err.ends_with("update codex manually"), "{err}");

        let unknown = "  Pick a reasoning level\n\n› 1. Low\n  2. High\n";
        assert_eq!(
            codex_startup_state(unknown, "run-1"),
            CodexStartup::Blocked(
                "codex startup blocked by interactive prompt: › 1. Low | 2. High; \
hint: run codex in this folder once and answer the prompt"
                    .into()
            )
        );
    }

    #[test]
    fn codex_startup_ignores_chat_ui_notices_and_quoted_prompts() {
        let idle = "  >_ OpenAI Codex (v0.159.3)\n\n› Ask Codex to do anything\n\n• pong\n";
        assert_eq!(codex_startup_state(idle, "run-1"), CodexStartup::Ready);
        assert_eq!(
            codex_startup_state("› Ask Codex to do anything\n\n• pong\n", "run-1"),
            CodexStartup::Ready
        );

        for pending in [
            "",
            "Running without the shared background server: disabled by --no-daemon\n",
            "Steps:\n1. Build\n2. Test\n",
        ] {
            assert_eq!(
                codex_startup_state(pending, "run-1"),
                CodexStartup::Pending,
                "{pending}"
            );
        }

        // The shell echo of the launch command (and codex's transcript of the
        // prompt) ends in the contract naming the run; anything quoted above
        // that line is prompt text, not a dialog.
        let echo = "% codex --no-daemon -- 'Folder access\nquote> Trust this folder?\n\
quote> › 1. Trust and continue\nquote>   2. Quit\nquote> › Ask Codex to do anything\n\
quote> tmuxlet bridge complete --run run-1 --file /h/.tmuxlet/runs/run-1/answer.txt\n\
quote> Do not wait for the user after the final response is written.'\n";
        assert_eq!(codex_startup_state(echo, "run-1"), CodexStartup::Pending);
        let transcript = "› 1. Trust and continue\n  2. Quit\n  bridge complete --run run-1\n\n\
• Working (0s • esc to interrupt)\n\n› Ask Codex to do anything\n";
        assert_eq!(
            codex_startup_state(transcript, "run-1"),
            CodexStartup::Ready
        );
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
        assert!(cmd.args.contains(&"--continue".to_string()));
        assert!(!cmd.args.contains(&"resume".to_string()));

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
        let opts = parse(&["--target", "pi", "-p", "x"]);
        let (cmd, prompt_in_launch) =
            command_with_prompt(target_command(&opts).unwrap(), &opts.target, "final prompt");
        assert!(!prompt_in_launch);
        assert!(!cmd.args.contains(&"final prompt".to_string()));
    }

    #[test]
    fn agy_launch_embeds_prompt_via_prompt_interactive() {
        for target in ["agy", "gemini"] {
            let opts = parse(&["--target", target, "-p", "x"]);
            let (cmd, prompt_in_launch) =
                command_with_prompt(target_command(&opts).unwrap(), &opts.target, "final prompt");
            assert!(prompt_in_launch);
            assert!(has_pair(&cmd.args, "--prompt-interactive", "final prompt"));
            // agy must NOT use positional `--` prompt delivery.
            assert!(!cmd.args.contains(&"--".to_string()));
        }
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
    fn agy_maps_bypass_to_skip_permissions() {
        let opts = parse(&["--target", "agy", "--force", "-p", "x"]);
        let cmd = agy_command(&opts).unwrap();
        assert!(
            cmd.args
                .contains(&"--dangerously-skip-permissions".to_string())
        );
    }

    #[test]
    fn gemini_alias_and_agy_map_core_flags() {
        for target in ["agy", "gemini"] {
            let opts = parse(&[
                "--target",
                target,
                "--model",
                "gemini-x",
                "--add-dir",
                "/tmp/w",
                "-p",
                "x",
            ]);
            let cmd = target_command(&opts).unwrap();
            assert_eq!(cmd.program, "agy");
            assert!(has_pair(&cmd.args, "--model", "gemini-x"));
            assert!(has_pair(&cmd.args, "--add-dir", "/tmp/w"));
        }
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
        let opts = parse(&["--target", "agy", "--system-prompt", "sys", "-p", "x"]);
        assert!(agy_command(&opts).is_err());
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
    fn bare_dash_is_a_prompt_part_not_an_option() {
        let opts = parse(&["-p", "-"]);
        assert_eq!(opts.prompt_parts, vec!["-".to_string()]);
    }

    #[test]
    fn unknown_dash_options_are_still_rejected() {
        let mut args: Vec<String> = ["-p", "--not-a-flag"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(parse_options(&mut args).is_err());
        let mut args: Vec<String> = ["-p", "-x"].iter().map(|s| s.to_string()).collect();
        assert!(parse_options(&mut args).is_err());
    }

    #[test]
    fn dash_prompt_part_selects_the_stdin_branch() {
        // prompt_from reads stdin for a bare "-" and for an empty prompt; both
        // are distinct from a literal prompt that merely contains a dash.
        let opts = parse(&["-p", "--", "-", "extra"]);
        assert_eq!(opts.prompt_parts.join(" "), "- extra");
        let opts = parse(&["-p", "-"]);
        assert_eq!(opts.prompt_parts.join(" "), "-");
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
    #[test]
    fn owner_precedence_and_foreign_access() {
        assert_eq!(pick_owner(Some("flag"), Some("env"), "parent@cwd"), "flag");
        assert_eq!(pick_owner(None, Some("env"), "parent@cwd"), "env");
        assert_eq!(pick_owner(None, None, "parent@cwd"), "parent@cwd");
        assert!(!is_foreign(Some("mine"), "mine", false));
        assert!(is_foreign(Some("theirs"), "mine", false));
        assert!(!is_foreign(Some("theirs"), "mine", true));
        assert!(!is_foreign(None, "mine", false)); // legacy runs remain accessible
    }

    #[test]
    fn ownership_flags_and_literal_messages_parse() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let (rest, access) = management_args(&args(&[
            "--owner",
            "worker",
            "id",
            "--force-foreign",
            "hello",
        ]))
        .unwrap();
        assert_eq!(rest, args(&["id", "hello"]));
        assert_eq!(access.owner, "worker");
        assert!(access.force_foreign);
        let (rest, access) = management_args(&args(&[
            "--owner", "worker", "id", "--", "--owner", "literal",
        ]))
        .unwrap();
        assert_eq!(rest, args(&["id", "--", "--owner", "literal"]));
        assert!(!access.force_foreign);
        assert!(management_args(&args(&["--owner"])).is_err());
        let opts = parse(&["-p", "--owner", "worker", "--reap-min-age", "12", "hello"]);
        assert_eq!(opts.owner.as_deref(), Some("worker"));
        assert_eq!(opts.reap_min_age, Some(12));
    }

    #[test]
    fn reap_age_configuration_precedence_and_validation() {
        assert_eq!(pick_reap_min_age(None, None).unwrap(), 600);
        assert_eq!(pick_reap_min_age(None, Some("3")).unwrap(), 180);
        assert_eq!(pick_reap_min_age(Some(2), Some("invalid")).unwrap(), 120);
        assert_eq!(pick_reap_min_age(Some(0), None).unwrap(), 0);
        for invalid in ["-1", "abc", "1.5", "18446744073709551615"] {
            assert!(parse_minutes(invalid).is_err());
            assert!(pick_reap_min_age(None, Some(invalid)).is_err());
        }
        assert!(pick_reap_min_age(Some(u64::MAX), None).is_err());
    }

    #[test]
    fn auto_reap_requires_known_age_and_stable_pane_and_spares_prompts() {
        assert_eq!(
            auto_reap_decision(Some(600), 600, false, true),
            Ok("old with unchanged pane")
        );
        for age in [None, Some(0), Some(599)] {
            assert!(auto_reap_decision(age, 600, false, true).is_err());
        }
        assert!(auto_reap_decision(Some(9999), 600, false, false).is_err());
        for unchanged in [true, false] {
            assert_eq!(
                auto_reap_decision(Some(9999), 600, true, unchanged),
                Err("waiting on permission/trust prompt")
            );
        }
    }

    #[test]
    fn confirmation_detection_uses_prompt_shapes_and_recent_nonempty_lines() {
        for prompt in [
            "Do you trust the files in this folder?",
            "Allow this command?",
            "[Y/n]",
            "1. Yes, proceed (y)",
            "› Approve once",
        ] {
            assert!(is_execution_confirmation_blocker(prompt), "{prompt}");
        }
        for ordinary in [
            "Reading Bash output: allow, confirm, permission",
            "fn allow_permission() { confirm(); }",
            "The command requires approval handling in its source.",
        ] {
            assert!(!is_execution_confirmation_blocker(ordinary), "{ordinary}");
        }
        let recent = format!("Allow this command?\n{}", "working\n\n".repeat(15));
        assert!(!is_execution_confirmation_blocker(&recent));
        let recent = format!("{}\nAllow this command?", "working\n".repeat(20));
        assert!(is_execution_confirmation_blocker(&recent));
    }

    #[test]
    fn reap_log_is_one_json_line_with_iso_timestamp_and_context() {
        let target = ReapTarget {
            id: "run-1".into(),
            session: "tmuxlet-run-1".into(),
            reason: "old",
            age_secs: Some(600),
        };
        let meta =
            serde_json::json!({"owner": "worker\nname", "cwd": "/tmp/a\nb", "target": "codex"});
        let line = reap_log_line(
            0,
            "skipped",
            &target,
            &meta,
            "waiting on permission/trust prompt",
        );
        assert_eq!(line.lines().count(), 1);
        assert!(line.ends_with('\n'));
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["timestamp"], "1970-01-01T00:00:00Z");
        assert_eq!(value["action"], "skipped");
        assert_eq!(value["run"], "run-1");
        assert_eq!(value["owner"], "worker\nname");
        assert_eq!(value["cwd"], "/tmp/a\nb");
        assert_eq!(value["target"], "codex");
        assert_eq!(value["age"], 600);
        assert_eq!(value["reason"], "waiting on permission/trust prompt");
        let legacy: serde_json::Value = serde_json::from_str(&reap_log_line(
            0,
            "reaped",
            &target,
            &serde_json::json!({}),
            "old",
        ))
        .unwrap();
        assert_eq!(legacy["owner"], "unknown owner");
        assert_eq!(iso_timestamp(951782400), "2000-02-29T00:00:00Z");
        assert_eq!(iso_timestamp(1767225599), "2025-12-31T23:59:59Z");
    }

    #[test]
    fn metadata_records_owner_and_run_start() {
        let root = env::temp_dir().join(make_run_id());
        fs::create_dir_all(&root).unwrap();
        let paths = RunPaths {
            root: root.clone(),
            meta: root.join("meta.json"),
            prompt: root.join("prompt.txt"),
            answer: root.join("answer.txt"),
            complete: root.join("complete.txt"),
            pane_log: root.join("pane.log"),
            error: root.join("error.txt"),
        };
        let opts = Options {
            owner: Some("worker".into()),
            ..Options::default()
        };
        let before = epoch_secs();
        write_meta(
            &paths,
            "id",
            "session",
            &opts,
            &TargetCommand {
                program: "codex".into(),
                args: vec![],
            },
            &[],
        )
        .unwrap();
        let meta: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&paths.meta).unwrap()).unwrap();
        assert_eq!(meta["owner"], "worker");
        assert!(meta["startedAt"].as_u64().unwrap() >= before);
        assert!(meta["startedAt"].as_u64().unwrap() <= epoch_secs());
        fs::remove_dir_all(root).unwrap();
    }

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

    /// A fresh dir holding notes.md (text), pic.png, doc.pdf, and blob.bin.
    fn attach_dir() -> PathBuf {
        let dir = fs::canonicalize(env::temp_dir())
            .unwrap()
            .join(format!("tmuxlet-attach-{}", make_run_id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("notes.md"), "# Notes\nline two").unwrap();
        fs::write(dir.join("pic.png"), PNG).unwrap();
        fs::write(dir.join("doc.pdf"), b"%PDF-1.7\n%\xe2\xe3\n").unwrap();
        fs::write(dir.join("blob.bin"), b"\x00\x01\x02binary").unwrap();
        dir
    }

    fn plan(target: &str, files: &[&str], cwd: &Path) -> Result<Vec<Attachment>, (u8, String)> {
        let files: Vec<String> = files.iter().map(|f| f.to_string()).collect();
        plan_attachments(target, &files, cwd)
    }

    fn routes(target: &str, dir: &Path) -> Vec<Option<Delivery>> {
        ["notes.md", "pic.png", "doc.pdf", "blob.bin"]
            .iter()
            .map(|f| plan(target, &[f], dir).ok().map(|p| p[0].delivery))
            .collect()
    }

    #[test]
    fn file_and_attach_are_repeatable_and_canvas_flags_parse() {
        let opts = parse(&[
            "-p",
            "--file",
            "a.md",
            "--attach",
            "b.png",
            "--file",
            "c.txt",
            "--canvas",
            "--canvas-out",
            "out/s.md",
            "x",
        ]);
        assert_eq!(opts.files, ["a.md", "b.png", "c.txt"]);
        assert!(opts.canvas);
        assert_eq!(opts.canvas_out.as_deref(), Some("out/s.md"));
        assert_eq!(opts.prompt_parts, ["x"]);
    }

    #[test]
    fn file_kinds_by_content() {
        assert_eq!(file_kind(PNG, true), FileKind::Image);
        assert_eq!(
            file_kind(b"\xff\xd8\xff\xe0\0\x10JFIF", true),
            FileKind::Image
        );
        assert_eq!(file_kind(b"GIF89a\x01\0", true), FileKind::Image);
        assert_eq!(file_kind(b"RIFF\x24\0\0\0WEBPVP8 ", true), FileKind::Image);
        assert_eq!(file_kind(b"%PDF-1.4\n", true), FileKind::Pdf);
        assert_eq!(
            file_kind("caf\u{e9}\ttab\r\n\x0c\x1b[0m".as_bytes(), true),
            FileKind::Text
        );
        assert_eq!(file_kind(b"", true), FileKind::Text);
        assert_eq!(file_kind(b"text\0nul", true), FileKind::Binary);
        assert_eq!(file_kind(b"text\x01ctl", true), FileKind::Binary);
        assert_eq!(file_kind(b"\xff\xfe latin junk", true), FileKind::Binary);
        let cut = &"caf\u{e9}".as_bytes()[..4]; // a UTF-8 sequence cut off by the sniff window
        assert_eq!(file_kind(cut, false), FileKind::Text);
        assert_eq!(file_kind(cut, true), FileKind::Binary);
    }

    #[test]
    fn each_target_takes_files_by_its_native_mechanism_or_inlines_text() {
        use Delivery::{Flag, Inline, Mention};
        let dir = attach_dir();
        let cases: &[(&str, [Option<Delivery>; 4])] = &[
            // target            text            image          pdf            binary
            (
                "claude",
                [Some(Mention), Some(Mention), Some(Mention), None],
            ),
            ("codex", [Some(Inline), Some(Flag), None, None]),
            ("pi", [Some(Flag), Some(Flag), None, None]),
            ("hermes", [Some(Mention), None, None, None]),
            ("gweb", [Some(Flag), Some(Flag), Some(Flag), Some(Flag)]),
            (
                "gemini-web",
                [Some(Flag), Some(Flag), Some(Flag), Some(Flag)],
            ),
            ("chatgpt", [Some(Flag), Some(Flag), Some(Flag), Some(Flag)]),
            ("cgpt", [Some(Flag), Some(Flag), Some(Flag), Some(Flag)]),
            ("cursor", [Some(Inline), None, None, None]),
            ("agy", [Some(Inline), None, None, None]),
            ("opencode", [Some(Inline), None, None, None]),
            ("grok", [Some(Inline), None, None, None]),
            ("muse", [Some(Inline), None, None, None]),
        ];
        for (target, want) in cases {
            assert_eq!(routes(target, &dir), want.to_vec(), "{target}");
        }
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn refused_files_exit_7_and_say_which_targets_take_them() {
        let dir = attach_dir();
        let pic = dir.join("pic.png").display().to_string();
        let (code, message) = plan("cursor", &["pic.png"], &dir).unwrap_err();
        assert_eq!(code, EXIT_ATTACHMENT_REFUSED);
        assert_eq!(
            message,
            format!(
                "--file {pic}: cursor can't take image files: it has no native way to attach one, and only \
text is inlined (targets that take image files: claude, codex, pi, gweb, chatgpt)"
            )
        );
        let (_, message) = plan("codex", &["doc.pdf"], &dir).unwrap_err();
        assert!(
            message.ends_with("(targets that take PDF files: claude, gweb, chatgpt)"),
            "{message}"
        );
        let (_, message) = plan("claude", &["blob.bin"], &dir).unwrap_err();
        assert!(
            message.ends_with("(targets that take binary files: gweb, chatgpt)"),
            "{message}"
        );
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn file_paths_resolve_against_cwd_and_must_be_regular_files() {
        let dir = attach_dir();
        fs::create_dir_all(dir.join("sub")).unwrap();
        let planned = plan("gweb", &["notes.md", "sub/../pic.png"], &dir).unwrap();
        assert_eq!(
            planned.iter().map(|a| a.path.clone()).collect::<Vec<_>>(),
            [
                dir.join("notes.md").display().to_string(),
                dir.join("pic.png").display().to_string()
            ]
        );
        assert_eq!(planned[0].kind, FileKind::Text);
        let absolute = dir.join("notes.md").display().to_string();
        assert_eq!(plan("gweb", &[&absolute], Path::new("/")).unwrap().len(), 1);
        let (code, message) = plan("gweb", &["missing.md"], &dir).unwrap_err();
        assert_eq!(code, 1);
        assert!(message.starts_with("--file missing.md: "), "{message}");
        assert_eq!(
            plan("gweb", &["sub"], &dir).unwrap_err(),
            (1, "--file sub: not a regular file".to_string())
        );
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn inline_caps_are_raw_bytes_per_file_and_in_total() {
        let dir = attach_dir();
        let max = INLINE_FILE_MAX_BYTES as usize;
        fs::write(dir.join("max.txt"), "a".repeat(max)).unwrap();
        fs::write(dir.join("over.txt"), "a".repeat(max + 1)).unwrap();
        assert_eq!(
            plan("codex", &["max.txt"], &dir).unwrap()[0]
                .text
                .as_ref()
                .unwrap()
                .len(),
            max
        );
        let (code, message) = plan("codex", &["over.txt"], &dir).unwrap_err();
        assert_eq!(code, EXIT_ATTACHMENT_REFUSED);
        assert!(
            message.ends_with("codex gets text files inlined into the prompt, at most 256 KiB each; this one is 257 KiB"),
            "{message}"
        );
        assert!(plan("gweb", &["over.txt"], &dir).is_ok()); // native: no inline cap
        assert!(plan("claude", &["over.txt"], &dir).is_ok()); // @-mention: no inline cap
        let (code, message) =
            plan("muse", &["max.txt", "max.txt", "max.txt", "notes.md"], &dir).unwrap_err();
        assert_eq!(code, EXIT_ATTACHMENT_REFUSED);
        assert!(message.ends_with("the cap is 768 KiB"), "{message}");
        // UTF-8 in the sniffed head but not after it: refused, not inlined half-read.
        let mut tricky = vec![b'a'; SNIFF_BYTES as usize + 10];
        tricky.extend(b"\xff\xfe");
        fs::write(dir.join("tricky.txt"), tricky).unwrap();
        let (code, message) = plan("grok", &["tricky.txt"], &dir).unwrap_err();
        assert_eq!(code, EXIT_ATTACHMENT_REFUSED);
        assert!(
            message.contains("grok can't take binary files"),
            "{message}"
        );
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn mention_paths_with_odd_characters_inline_text_or_refuse() {
        let dir = attach_dir();
        fs::write(dir.join("my notes.md"), "spaced").unwrap();
        fs::write(dir.join("my pic.png"), PNG).unwrap();
        for target in ["claude", "hermes"] {
            let planned = plan(target, &["my notes.md"], &dir).unwrap();
            assert_eq!(planned[0].delivery, Delivery::Inline, "{target}");
            assert_eq!(planned[0].text.as_deref(), Some("spaced"));
        }
        let (code, message) = plan("claude", &["my pic.png"], &dir).unwrap_err();
        assert_eq!(code, EXIT_ATTACHMENT_REFUSED);
        assert!(
            message
                .contains("claude attaches image files by @-mention, which can't carry this path")
        );
        assert_eq!(
            plan("pi", &["my pic.png"], &dir).unwrap()[0].delivery,
            Delivery::Flag
        ); // argv: any path
        assert!(mention_safe("/Users/kk/a-b_c.d/e+f,g@h%i~j=k.md"));
        assert!(
            !mention_safe("/a b")
                && !mention_safe("/a\"b")
                && !mention_safe("/a:b")
                && !mention_safe("/a#b")
        );
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn the_prompt_gets_mentions_then_framed_inline_files() {
        let attachment = |path: &str, delivery, text: Option<&str>| Attachment {
            path: path.into(),
            kind: FileKind::Text,
            delivery,
            text: text.map(Into::into),
        };
        let files = [
            attachment("/w/a.md", Delivery::Mention, None),
            attachment("/w/b.png", Delivery::Mention, None),
            attachment("/w/c.txt", Delivery::Inline, Some("one\ntwo")),
            attachment("/w/d e.txt", Delivery::Inline, Some("done\n")),
            attachment("/w/f.png", Delivery::Flag, None),
        ];
        assert_eq!(
            prompt_with_attachments("Compare them.", "claude", &files),
            "Compare them.\n\nAttached files:\n@/w/a.md\n@/w/b.png\
\n\n<<<FILE path=\"/w/c.txt\" bytes=7>>>\none\ntwo\n<<<END FILE>>>\
\n\n<<<FILE path=\"/w/d e.txt\" bytes=5>>>\ndone\n<<<END FILE>>>"
        );
        assert_eq!(
            prompt_with_attachments("Read it.", "hermes", &files[..1]),
            "Read it.\n\nAttached files:\n@file:/w/a.md"
        );
        assert_eq!(
            prompt_with_attachments("Just this.", "codex", &files[4..]),
            "Just this."
        );
        assert_eq!(prompt_with_attachments("Plain.", "claude", &[]), "Plain.");
    }

    #[test]
    fn flag_files_become_each_targets_launch_arguments() {
        let dir = attach_dir();
        let with_files = |target: &str, files: &[&str]| {
            let mut opts = parse(&["--target", target, "-C", dir.to_str().unwrap(), "-p", "x"]);
            opts.attachments = plan(target, files, &dir).unwrap();
            target_command(&opts).unwrap()
        };
        let pic = dir.join("pic.png").display().to_string();
        let notes = dir.join("notes.md").display().to_string();
        let pdf = dir.join("doc.pdf").display().to_string();

        let codex = with_files("codex", &["pic.png", "notes.md"]);
        assert!(has_pair(&codex.args, "--image", &pic), "{:?}", codex.args);
        assert!(!codex.args.contains(&notes)); // inlined into the prompt instead
        let (launch, _) = command_with_prompt(codex, "codex", "prompt");
        let dash = launch.args.iter().position(|a| a == "--").unwrap();
        assert!(launch.args.iter().position(|a| *a == pic).unwrap() < dash); // -i can't swallow the prompt
        assert_eq!(launch.args[dash + 1..], ["prompt"]);

        let gweb = with_files("gweb", &["notes.md", "pic.png", "doc.pdf"]);
        for path in [&notes, &pic, &pdf] {
            assert!(has_pair(&gweb.args, "--file", path), "{:?}", gweb.args);
        }
        assert!(!gweb.args.contains(&"--output-format".to_string())); // files alone keep text answers

        let pi = with_files("pi", &["notes.md", "pic.png"]);
        assert!(
            pi.args.ends_with(&[format!("@{notes}"), format!("@{pic}")]),
            "{:?}",
            pi.args
        );

        for target in [
            "claude", "hermes", "muse", "cursor", "opencode", "grok", "agy",
        ] {
            let cmd = with_files(target, &["notes.md"]);
            assert!(
                !cmd.args.iter().any(|a| a.contains("notes.md")),
                "{target}: {:?}",
                cmd.args
            );
        }
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_long_launch_line_is_sourced_from_a_script() {
        let dir = attach_dir();
        let script = dir.join("launch.sh");
        let short = TargetCommand {
            program: "claude".into(),
            args: vec!["--".into(), "say ready".into()],
        };
        assert_eq!(
            launch_line(&short, &script).unwrap(),
            "claude -- 'say ready'"
        );
        assert!(!script.exists());
        let prompt = format!("{}\nit's done\n\n", "x".repeat(LAUNCH_LINE_MAX));
        let long = TargetCommand {
            program: "codex".into(),
            args: vec![
                "--image".into(),
                "/a b/pic.png".into(),
                "--".into(),
                prompt.clone(),
            ],
        };
        let line = launch_line(&long, &script).unwrap();
        assert_eq!(
            line,
            format!(". {}", shell_quote(&script.display().to_string()))
        );
        assert!(line.len() <= LAUNCH_LINE_MAX);
        let text = fs::read_to_string(&script).unwrap();
        assert_eq!(
            text,
            format!("codex --image '/a b/pic.png' -- {}\n", shell_quote(&prompt))
        );
        // The shell reads back the exact arguments, trailing newlines and quotes included.
        let echoed = Command::new("sh")
            .arg("-c")
            .arg(format!(
                "codex() {{ printf '%s\\0' \"$@\"; }}; . {}",
                shell_quote(&script.display().to_string())
            ))
            .output()
            .unwrap();
        let args: Vec<&[u8]> = echoed.stdout.split(|b| *b == 0).collect();
        assert_eq!(
            args[..4],
            [&b"--image"[..], b"/a b/pic.png", b"--", prompt.as_bytes()]
        );
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_chat_launch_with_many_files_stays_under_the_tty_limit() {
        let dir = attach_dir();
        let argv = chat_argv("gweb", &["-C", dir.to_str().unwrap(), "--timeout", "600"]);
        let mut opts = parse(&argv);
        opts.attachments = plan("gweb", &["notes.md"; 10], &dir).unwrap();
        let base = target_command(&opts).unwrap();
        let paths = RunPaths {
            root: dir.clone(),
            ..fixed_paths()
        };
        let (launch, _) = launch_plan("gweb", base, &paths, "run-1", "/bin/tmuxlet", "p");
        let script = dir.join("launch.sh");
        let line = launch_line(&launch, &script).unwrap();
        assert_eq!(
            line,
            format!(". {}", shell_quote(&script.display().to_string()))
        );
        let text = fs::read_to_string(&script).unwrap();
        assert!(text.starts_with("exec sh -c "), "{text}");
        assert_eq!(text.matches(" --file ").count(), 11); // 10 attachments + bridge complete's
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn gweb_forwards_canvas_with_json_output() {
        let cmd = target_command(&parse(&chat_argv("gweb", &["--canvas"]))).unwrap();
        assert!(cmd.args.contains(&"--canvas".to_string()));
        assert!(has_pair(&cmd.args, "--output-format", "json"));
        let argv = chat_argv("gweb", &["-C", "/work", "--canvas-out", "out/scene.md"]);
        let cmd = target_command(&parse(&argv)).unwrap();
        assert!(
            has_pair(&cmd.args, "--canvas-out", "/work/out/scene.md"),
            "{:?}",
            cmd.args
        );
        assert!(!cmd.args.contains(&"--canvas".to_string())); // --canvas-out implies it
        let argv = chat_argv("gweb", &["--canvas", "--image-out", "/tmp/i/"]);
        let cmd = target_command(&parse(&argv)).unwrap();
        assert_eq!(
            cmd.args.iter().filter(|a| *a == "--output-format").count(),
            1
        );
    }

    #[test]
    fn gweb_json_runs_ask_gweb_for_json_to_carry_its_citations() {
        let cmd = target_command(&parse(&chat_argv("gweb", &["--output-format", "json"]))).unwrap();
        assert!(
            has_pair(&cmd.args, "--output-format", "json"),
            "{:?}",
            cmd.args
        );
        let cmd = target_command(&parse(&chat_argv("gweb", &[]))).unwrap();
        assert!(!cmd.args.contains(&"--output-format".to_string())); // text runs keep gweb's text
        let cmd = target_command(&parse(&chat_argv("muse", &["--output-format", "json"]))).unwrap();
        assert!(
            !cmd.args.contains(&"--output-format".to_string()),
            "{:?}",
            cmd.args
        );

        let citations = serde_json::json!([{"n": 1, "label": "MD", "file": "outline.md"},
            {"n": 2, "label": "Wikipedia", "title": "Reykjavík - Wikipedia", "url": "https://en.wikipedia.org/wiki/Reykjav%C3%ADk"}]);
        let answer = "Mara runs[^1]. Reykjavík[^2].\n\n[^1]: outline.md\n[^2]: Reykjavík - Wikipedia, https://en.wikipedia.org/wiki/Reykjav%C3%ADk";
        let gweb =
            serde_json::json!({"answer": answer, "citations": citations.clone(), "images": []})
                .to_string();
        assert_eq!(
            gweb_citations(&format!("{gweb}\n")),
            Some(citations.clone())
        );
        assert_eq!(gweb_result(&gweb, false, false).0, format!("{answer}\n")); // the footnoted answer
        for old in [
            "plain answer\n",
            "{\"answer\": \"x\"}",
            "{\"citations\": null}",
        ] {
            assert_eq!(gweb_citations(old), None, "{old}");
        }
        let result = RunResult {
            id: "ct".into(),
            target: "gweb".into(),
            status: "completed".into(),
            output: format!("{answer}\n"),
            images: None,
            canvas: None,
            citations: Some(citations.clone()),
            models: None,
            output_format: OutputFormat::Json,
            cwd: PathBuf::from("/tmp"),
            tmux_session: "tmuxlet-ct".into(),
            completion_source: "explicit".into(),
            elapsed_ms: 5,
        };
        let json: serde_json::Value = serde_json::from_str(&result_json(&result)).unwrap();
        assert_eq!(
            (json["citations"].clone(), json["output"].clone()),
            (citations, serde_json::json!(format!("{answer}\n")))
        );
    }

    #[test]
    fn canvas_flags_are_rejected_for_other_targets() {
        for target in [
            "muse", "claude", "codex", "agy", "opencode", "pi", "grok", "hermes", "cursor",
        ] {
            for flags in [&["--canvas"][..], &["--canvas-out", "/tmp/s.md"][..]] {
                let err = target_command(&parse(&chat_argv(target, flags))).unwrap_err();
                assert!(
                    err.ends_with(&format!(
                        "normalized flag {} (only gweb has Gemini's Canvas)",
                        flags[0]
                    )),
                    "{target}: {err}"
                );
            }
        }
    }

    #[test]
    fn gweb_result_passes_canvas_through_and_prints_its_path_or_markdown() {
        let doc = serde_json::json!({"title": "Rain", "markdown": "# Rain\n\nOn tin.", "chars": 15,
            "path": "/tmp/gweb-canvas-test/scene.md"});
        let gweb = serde_json::json!({"answer": "Here is your scene.", "images": [], "canvas": doc.clone()});
        let (output, images, canvas) = gweb_result(&gweb.to_string(), false, true);
        assert_eq!(
            output,
            "Here is your scene.\n/tmp/gweb-canvas-test/scene.md\n"
        );
        assert_eq!((images, canvas), (None, Some(doc.clone())));
        let mut unsaved = gweb.clone();
        unsaved["canvas"]["path"] = serde_json::Value::Null;
        assert_eq!(
            gweb_result(&unsaved.to_string(), false, true).0,
            "Here is your scene.\n# Rain\n\nOn tin.\n"
        );
        let mut none = gweb.clone();
        none["canvas"] = serde_json::Value::Null;
        assert_eq!(
            gweb_result(&none.to_string(), false, true),
            (
                "Here is your scene.\n".to_string(),
                None,
                Some(serde_json::Value::Null)
            )
        );
        let old = serde_json::json!({"answer": "x", "images": []}).to_string();
        assert_eq!(gweb_result(&old, false, true), (old.clone(), None, None)); // no canvas field: unchanged
    }

    #[test]
    fn result_json_carries_gweb_canvas_after_images() {
        let canvas =
            serde_json::json!({"title": "Rain", "markdown": "# Rain", "chars": 6, "path": null});
        let result = RunResult {
            id: "cv".into(),
            target: "gweb".into(),
            status: "completed".into(),
            output: "ok\n# Rain\n".into(),
            images: Some(serde_json::json!([])),
            canvas: Some(canvas.clone()),
            citations: None,
            models: None,
            output_format: OutputFormat::Json,
            cwd: PathBuf::from("/tmp"),
            tmux_session: "tmuxlet-cv".into(),
            completion_source: "explicit".into(),
            elapsed_ms: 5,
        };
        let text = result_json(&result);
        assert!(
            text.starts_with("{\"id\":\"cv\",\"target\":\"gweb\""),
            "{text}"
        );
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            (json["canvas"].clone(), json["images"].clone()),
            (canvas, serde_json::json!([]))
        );
    }
}
