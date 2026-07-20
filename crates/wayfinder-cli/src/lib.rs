use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value as JsonValue};
use sha2::{Digest, Sha256};
use wayfinder_internal_core::calibrate::{
    calibrate, load_dataset, CalibrationOptions, CalibrationResult,
};
use wayfinder_internal_core::complexity::{
    binary_tiers, explain_score, score_complexity, ComplexityScore, FeatureWeights, RoutingConfig,
    DEFAULT_WEIGHTS, FEATURE_ORDER,
};
use wayfinder_internal_core::config::{find_config_file, load_routing_config, CONFIG_FILE};
use wayfinder_internal_core::feedback::{read_labels, record_label, DEFAULT_LOG};
use wayfinder_internal_core::judge::{HeuristicJudge, Judge, OnboardOutputs};
use wayfinder_internal_core::onboard::OnboardSummary;
use wayfinder_internal_core::sufficiency::{
    evaluate_with_options, EvaluateOptions, DEFAULT_CV_FOLDS, DEFAULT_KAPPA_FLOOR,
};
use wayfinder_internal_core::vkeys;
use wayfinder_internal_gateway::bootstrap::{
    key_status, missing_keys, render_config, render_env_example, resolve_keys,
    suggest_key_commands, DEFAULT_PRESET, PRESETS,
};
use wayfinder_internal_gateway::recalibrate::{recalibrate, DEFAULT_MIN_LABELS};
use wayfinder_internal_gateway::service::{
    agent_path, detect_platform, launchd_plist, systemd_unit, systemd_unit_path, ServicePlatform,
    LAUNCHD_LABEL, SYSTEMD_UNIT_NAME,
};
use wayfinder_internal_gateway::{
    gateway_config_from_toml, invoke_messages, load_gateway_models,
    serve_summary as gateway_serve_summary, GatewayModel, RelayMessage, ServeOptions,
};
use wayfinder_internal_tui::{run_chat, ChatOptions, HELP};
use wayfinder_internal_ui::{serve_summary as ui_serve_summary, UiOptions};

const EXIT_CONFIG: i32 = 1;
const EXIT_USAGE: i32 = 2;
const COMMAND_LIST: &str =
    "route, calibrate, serve, service, chat, ui, webchat, onboard, judge, recalibrate, init, doctor, keys";

#[derive(Debug, PartialEq, Eq)]
pub struct CliError {
    message: String,
    exit_code: i32,
    stdout: String,
    stderr: String,
}

impl CliError {
    fn new(message: impl Into<String>) -> Self {
        Self::usage(message)
    }

    fn usage(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            exit_code: EXIT_USAGE,
            stdout: String::new(),
            stderr: String::new(),
        }
    }

    fn config(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            exit_code: EXIT_CONFIG,
            stdout: String::new(),
            stderr: String::new(),
        }
    }

    fn with_output(
        message: impl Into<String>,
        exit_code: i32,
        stdout: impl Into<String>,
        stderr: impl Into<String>,
    ) -> Self {
        Self {
            message: message.into(),
            exit_code,
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }

    pub fn exit_code(&self) -> i32 {
        self.exit_code
    }

    pub fn stdout(&self) -> &str {
        &self.stdout
    }

    pub fn stderr(&self) -> &str {
        &self.stderr
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for CliError {}

#[derive(Debug, PartialEq)]
pub enum CliCommand {
    Serve(ServeOptions),
    Service(ServiceOptions),
    Chat(ChatOptions),
    Ui(UiOptions),
    Webchat(WebchatOptions),
    Init(InitOptions),
    Doctor(DoctorOptions),
    Keys(KeysOptions),
    Route(RouteOptions),
    Calibrate(CalibrateOptions),
    Recalibrate(RecalibrateOptions),
    Onboard(OnboardOptions),
    Judge(JudgeOptions),
    Help(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct WebchatOptions {
    pub host: String,
    pub port: u16,
    pub dry_run: bool,
    pub timeout_seconds: Option<f64>,
    pub no_open: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceOptions {
    pub action: String,
    pub host: String,
    pub port: u16,
    pub print: bool,
    /// Baked into the generated unit as `serve --config PATH`.
    pub config: Option<PathBuf>,
}

impl Default for ServiceOptions {
    fn default() -> Self {
        Self {
            action: "status".to_owned(),
            host: "127.0.0.1".to_owned(),
            port: 8088,
            print: false,
            config: None,
        }
    }
}

impl Default for WebchatOptions {
    fn default() -> Self {
        let serve = ServeOptions::default();
        Self {
            host: serve.host,
            port: serve.port,
            dry_run: false,
            timeout_seconds: None,
            no_open: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InitOptions {
    pub interactive: bool,
    pub preset: String,
    pub path: PathBuf,
    pub force: bool,
    pub print: bool,
}

impl Default for InitOptions {
    fn default() -> Self {
        Self {
            interactive: false,
            preset: DEFAULT_PRESET.to_owned(),
            path: PathBuf::from(CONFIG_FILE),
            force: false,
            print: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DoctorOptions {
    pub dir: PathBuf,
}

impl Default for DoctorOptions {
    fn default() -> Self {
        Self {
            dir: PathBuf::from("."),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeysOptions {
    pub action: String,
    pub id: String,
    pub tags: Vec<String>,
}

impl Default for KeysOptions {
    fn default() -> Self {
        Self {
            action: "new".to_owned(),
            id: "team-1".to_owned(),
            tags: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RouteOptions {
    pub prompt: String,
    pub threshold: Option<f64>,
    pub json: bool,
    pub explain: bool,
    pub input: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CalibrateOptions {
    pub dataset: PathBuf,
    pub mode: String,
    pub models: Option<String>,
    pub out: Option<PathBuf>,
    pub iterations: usize,
    pub l2: f64,
    pub objective: String,
    pub target_savings: Option<f64>,
    pub costs: Option<String>,
    pub weights: Option<String>,
}

impl Default for CalibrateOptions {
    fn default() -> Self {
        Self {
            dataset: PathBuf::new(),
            mode: "threshold".to_owned(),
            models: None,
            out: None,
            iterations: 100,
            l2: 0.01,
            objective: "accuracy".to_owned(),
            target_savings: None,
            costs: None,
            weights: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RecalibrateOptions {
    pub log: PathBuf,
    pub out: PathBuf,
    pub mode: String,
    pub min_labels: usize,
}

impl Default for RecalibrateOptions {
    fn default() -> Self {
        Self {
            log: PathBuf::from("wayfinder-router-feedback.jsonl"),
            out: PathBuf::from("wayfinder-router.toml"),
            mode: "threshold".to_owned(),
            min_labels: DEFAULT_MIN_LABELS,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct OnboardOptions {
    pub prompts: PathBuf,
    pub arms: Option<String>,
    pub log: PathBuf,
    pub calibrate: bool,
    pub mode: String,
}

impl Default for OnboardOptions {
    fn default() -> Self {
        Self {
            prompts: PathBuf::new(),
            arms: None,
            log: PathBuf::from(DEFAULT_LOG),
            calibrate: false,
            mode: "threshold".to_owned(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct JudgeOptions {
    pub prompts: PathBuf,
    pub arms: Option<String>,
    pub gold: Option<PathBuf>,
    pub log: PathBuf,
    pub mode: String,
    pub kappa_floor: f64,
    pub folds: usize,
    pub limit: Option<usize>,
    pub save_comparisons: Option<PathBuf>,
}

impl Default for JudgeOptions {
    fn default() -> Self {
        Self {
            prompts: PathBuf::new(),
            arms: None,
            gold: None,
            log: PathBuf::from(DEFAULT_LOG),
            mode: "threshold".to_owned(),
            kappa_floor: DEFAULT_KAPPA_FLOOR,
            folds: DEFAULT_CV_FOLDS,
            limit: None,
            save_comparisons: None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommandOutput {
    pub stdout: String,
    pub stderr: String,
}

#[derive(Clone, Debug, PartialEq)]
struct ParsedWeights {
    weights: FeatureWeights,
    supplied: BTreeMap<String, f64>,
}

/// Usage block for `chat --help`, joined with the slash-command [`HELP`] summary.
const CHAT_USAGE: &str = "\
usage: wayfinder-router chat [OPTIONS] [PROMPT]

Open the interactive chat on a terminal. When a prompt is passed as arguments or
stdin is piped, print the routing transcript instead so scripting and CI work.

options:
  --theme <name>       color theme: auto, dark, or light
  --threshold <0..1>   local/cloud routing cut
  --why                expand the routing decision for each prompt
  --dry-run            skip the gateway call
  --no-stream          disable token-by-token streaming
  --base-url <url>     gateway base url for the thin client
  --thread-dir <dir>   directory for saved conversation threads
  --help, -h           show this help";

const ROUTE_USAGE: &str = "\
usage: wayfinder-router route <prompt|-> [OPTIONS]

Score a prompt and recommend a model. Pass '-' to read the prompt from stdin.

options:
  --threshold <0..1>   force a binary local/cloud routing cut
  --json               emit JSON instead of human-readable text
  --explain            show each feature contribution in human output
  --help, -h           show this help";

const TOP_LEVEL_USAGE: &str = "\
usage: wayfinder-router <command> [OPTIONS]

commands:
  route        score a prompt and recommend a model
  calibrate    turn labeled JSONL into routing config
  serve        run the OpenAI-compatible gateway
  service      install, remove, or inspect the local gateway service
  chat         open the terminal chat UI
  ui           run the local calibration and configuration UI
  webchat      run the gateway and open /demo
  onboard      A/B sample prompts to bootstrap labels
  judge        auto-label prompts behind trust checks
  recalibrate  re-fit config from the feedback log
  init         scaffold wayfinder-router.toml
  doctor       check config and key readiness
  keys         mint virtual gateway keys

options:
  --help, -h   show this help";

const UI_USAGE: &str = "\
usage: wayfinder-router ui [OPTIONS]

Run the local calibration, explain, and configure UI.

options:
  --host <host>   bind host
  --port <port>   bind port
  --help, -h      show this help";

const WEBCHAT_USAGE: &str = "\
usage: wayfinder-router webchat [OPTIONS]

Run the gateway and open the browser demo at /demo.

options:
  --host <host>   bind host
  --port <port>   bind port
  --dry-run       show routing decisions without upstream calls
  --timeout <n>   upstream request timeout in seconds
  --no-open       do not open the demo in a browser
  --help, -h      show this help";

const SERVICE_USAGE: &str = "\
usage: wayfinder-router service <install|uninstall|status> [OPTIONS]

Run the gateway as an always-on local service.

options:
  --host <host>   gateway host
  --port <port>   gateway port
  --config <path> config file to bake into the unit, so the service loads a fixed
                  file regardless of its working directory
  --print         print the generated unit file instead of installing it
  --help, -h      show this help";

const INIT_USAGE: &str = "\
usage: wayfinder-router init [OPTIONS]

Scaffold a wayfinder-router.toml and matching .env.example.

options:
  -i, --interactive          accept the interactive init flag
  --preset <name>            hybrid, openai, or gemini
  --path <path>              config path to write
  --force                    overwrite existing files
  --print                    print config instead of writing
  --help, -h                 show this help";

const DOCTOR_USAGE: &str = "\
usage: wayfinder-router doctor [OPTIONS]

Check the nearest wayfinder-router.toml and model key readiness.

options:
  --dir <path>   where to start the config search
  --help, -h     show this help";

const KEYS_USAGE: &str = "\
usage: wayfinder-router keys new [OPTIONS]

Mint a virtual API key for the gateway.

options:
  --id <id>      key id for the [gateway.keys.<id>] block
  --tag <tag>    attribution tag, repeatable
  --help, -h     show this help";

const CALIBRATE_USAGE: &str = "\
usage: wayfinder-router calibrate <dataset> [OPTIONS]

Turn a labeled JSONL dataset into a wayfinder-router.toml fragment.

options:
  --mode <name>          threshold, tiers, or classifier
  --models <a,b,c>       comma-separated model order
  --out <path>           write the config fragment instead of stdout
  --iterations <n>       max classifier Newton iterations
  --l2 <number>          classifier L2 regularization
  --objective <name>     accuracy, knee, or cost-quality
  --target-savings <n>   cost saved target for cost-quality
  --costs <pairs>        per-arm costs, for example local=0.2,cloud=1.0
  --weights <pairs>      feature weights, for example reasoning_term_count=5
  --help, -h             show this help";

const RECALIBRATE_USAGE: &str = "\
usage: wayfinder-router recalibrate [OPTIONS]

Re-fit the routing config from the feedback log.

options:
  --log <path>        feedback label log to read
  --out <path>        config file to update
  --mode <name>       threshold, tiers, or classifier
  --min-labels <n>    skip without writing below this many labels
  --help, -h          show this help";

const ONBOARD_USAGE: &str = "\
usage: wayfinder-router onboard <prompts> [OPTIONS]

A/B local vs hosted on sample prompts to bootstrap labels.

options:
  --arms <a,b>      two gateway model names to compare
  --log <path>      label log to append to
  --calibrate       calibrate a config from the log when done
  --mode <name>     threshold, tiers, or classifier
  --help, -h        show this help";

const JUDGE_USAGE: &str = "\
usage: wayfinder-router judge <prompts> [OPTIONS]

Auto-label prompts by comparing two tiers, gated by trust checks.

options:
  --arms <a,b>             two gateway model names in cheap,expensive order
  --gold <path>            human-labeled {\"text\",\"label\"} JSONL set
  --log <path>             label log to append to
  --mode <name>            threshold, tiers, or classifier
  --kappa-floor <n>        minimum judge-vs-gold Cohen's kappa
  --folds <n>              cross-validation folds for the lift gate
  --limit <n>              judge at most this many prompts
  --save-comparisons <p>   write prompts, responses, and verdicts JSONL
  --help, -h               show this help";

fn chat_help() -> String {
    format!("{CHAT_USAGE}\n\n{HELP}")
}

pub fn run<I>(args: I) -> Result<String, CliError>
where
    I: IntoIterator,
    I::Item: Into<String>,
{
    run_with_input(args, None)
}

pub fn run_with_input<I>(args: I, stdin: Option<String>) -> Result<String, CliError>
where
    I: IntoIterator,
    I::Item: Into<String>,
{
    Ok(execute(parse_with_input(args, stdin)?)?.stdout)
}

pub fn run_output<I>(args: I, stdin: Option<String>) -> Result<CommandOutput, CliError>
where
    I: IntoIterator,
    I::Item: Into<String>,
{
    execute(parse_with_input(args, stdin)?)
}

pub fn parse<I>(args: I) -> Result<CliCommand, CliError>
where
    I: IntoIterator,
    I::Item: Into<String>,
{
    parse_with_input(args, None)
}

pub fn parse_with_input<I>(args: I, stdin: Option<String>) -> Result<CliCommand, CliError>
where
    I: IntoIterator,
    I::Item: Into<String>,
{
    let mut args = args.into_iter().map(Into::into);
    match args.next().as_deref() {
        Some("--help" | "-h" | "help") => Ok(CliCommand::Help(TOP_LEVEL_USAGE.to_owned())),
        Some("serve") => Ok(CliCommand::Serve(parse_serve(args)?)),
        Some("service") => match parse_service(args)? {
            None => Ok(CliCommand::Help(SERVICE_USAGE.to_owned())),
            Some(options) => Ok(CliCommand::Service(options)),
        },
        Some("chat") => match parse_chat(args)? {
            None => Ok(CliCommand::Help(chat_help())),
            Some(mut options) => {
                if options.input.is_none() {
                    options.input = stdin.and_then(non_empty);
                }
                Ok(CliCommand::Chat(options))
            }
        },
        Some("ui") => match parse_ui(args)? {
            None => Ok(CliCommand::Help(UI_USAGE.to_owned())),
            Some(options) => Ok(CliCommand::Ui(options)),
        },
        Some("webchat") => match parse_webchat(args)? {
            None => Ok(CliCommand::Help(WEBCHAT_USAGE.to_owned())),
            Some(options) => Ok(CliCommand::Webchat(options)),
        },
        Some("init") => match parse_init(args)? {
            None => Ok(CliCommand::Help(INIT_USAGE.to_owned())),
            Some(options) => Ok(CliCommand::Init(options)),
        },
        Some("doctor") => match parse_doctor(args)? {
            None => Ok(CliCommand::Help(DOCTOR_USAGE.to_owned())),
            Some(options) => Ok(CliCommand::Doctor(options)),
        },
        Some("keys") => match parse_keys(args)? {
            None => Ok(CliCommand::Help(KEYS_USAGE.to_owned())),
            Some(options) => Ok(CliCommand::Keys(options)),
        },
        Some("route") => match parse_route(args)? {
            None => Ok(CliCommand::Help(ROUTE_USAGE.to_owned())),
            Some(mut options) => {
                if options.prompt == "-" {
                    options.input = stdin;
                }
                Ok(CliCommand::Route(options))
            }
        },
        Some("calibrate") => match parse_calibrate(args)? {
            None => Ok(CliCommand::Help(CALIBRATE_USAGE.to_owned())),
            Some(options) => Ok(CliCommand::Calibrate(options)),
        },
        Some("recalibrate") => match parse_recalibrate(args)? {
            None => Ok(CliCommand::Help(RECALIBRATE_USAGE.to_owned())),
            Some(options) => Ok(CliCommand::Recalibrate(options)),
        },
        Some("onboard") => match parse_onboard(args)? {
            None => Ok(CliCommand::Help(ONBOARD_USAGE.to_owned())),
            Some(options) => Ok(CliCommand::Onboard(options)),
        },
        Some("judge") => match parse_judge(args)? {
            None => Ok(CliCommand::Help(JUDGE_USAGE.to_owned())),
            Some(options) => Ok(CliCommand::Judge(options)),
        },
        Some(command) => Err(CliError::new(format!(
            "unknown command '{command}' (expected {COMMAND_LIST})"
        ))),
        None => Err(CliError::new(format!("expected command: {COMMAND_LIST}"))),
    }
}

pub fn execute(command: CliCommand) -> Result<CommandOutput, CliError> {
    match command {
        CliCommand::Serve(options) => Ok(CommandOutput {
            stdout: gateway_serve_summary(&options),
            stderr: String::new(),
        }),
        CliCommand::Service(options) => execute_service(options),
        CliCommand::Chat(options) => Ok(CommandOutput {
            stdout: run_chat(&options).map_err(|err| CliError::config(err.to_string()))?,
            stderr: String::new(),
        }),
        CliCommand::Ui(options) => Ok(CommandOutput {
            stdout: ui_serve_summary(&options),
            stderr: String::new(),
        }),
        CliCommand::Webchat(options) => Ok(CommandOutput {
            stdout: webchat_summary(&options),
            stderr: String::new(),
        }),
        CliCommand::Init(options) => execute_init(options),
        CliCommand::Doctor(options) => execute_doctor(options),
        CliCommand::Keys(options) => execute_keys(options),
        CliCommand::Route(options) => execute_route(options),
        CliCommand::Calibrate(options) => execute_calibrate(options),
        CliCommand::Recalibrate(options) => execute_recalibrate(options),
        CliCommand::Onboard(options) => execute_onboard(options),
        CliCommand::Judge(options) => execute_judge(options),
        CliCommand::Help(text) => Ok(CommandOutput {
            stdout: format!("{text}\n"),
            stderr: String::new(),
        }),
    }
}

fn parse_serve<I>(args: I) -> Result<ServeOptions, CliError>
where
    I: IntoIterator<Item = String>,
{
    let mut options = ServeOptions::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--host" => options.host = next_value(&mut args, "--host")?,
            "--port" => {
                options.port = next_value(&mut args, "--port")?
                    .parse()
                    .map_err(|_| CliError::new("--port must be an integer"))?;
            }
            "--dry-run" => options.dry_run = true,
            "--timeout" => {
                options.timeout_seconds = Some(
                    next_value(&mut args, "--timeout")?
                        .parse()
                        .map_err(|_| CliError::new("--timeout must be a number"))?,
                );
            }
            "--config" => {
                options.config = Some(PathBuf::from(next_value(&mut args, "--config")?));
            }
            other => return Err(CliError::new(format!("unknown serve option '{other}'"))),
        }
    }
    Ok(options)
}

fn parse_service<I>(args: I) -> Result<Option<ServiceOptions>, CliError>
where
    I: IntoIterator<Item = String>,
{
    let mut args = args.into_iter();
    let Some(action) = args.next() else {
        return Err(CliError::new(
            "service requires an action: install, uninstall, or status",
        ));
    };
    if matches!(action.as_str(), "--help" | "-h") {
        return Ok(None);
    }
    if !matches!(action.as_str(), "install" | "uninstall" | "status") {
        return Err(CliError::new(
            "service action must be install, uninstall, or status",
        ));
    }
    let mut options = ServiceOptions {
        action,
        ..ServiceOptions::default()
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => return Ok(None),
            "--host" => options.host = next_value(&mut args, "--host")?,
            "--port" => {
                options.port = next_value(&mut args, "--port")?
                    .parse()
                    .map_err(|_| CliError::new("--port must be an integer"))?;
            }
            "--print" => options.print = true,
            "--config" => {
                options.config = Some(PathBuf::from(next_value(&mut args, "--config")?));
            }
            other => return Err(CliError::new(format!("unknown service option '{other}'"))),
        }
    }
    Ok(Some(options))
}

fn parse_ui<I>(args: I) -> Result<Option<UiOptions>, CliError>
where
    I: IntoIterator<Item = String>,
{
    let mut options = UiOptions::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => return Ok(None),
            "--host" => options.host = next_value(&mut args, "--host")?,
            "--port" => {
                options.port = next_value(&mut args, "--port")?
                    .parse()
                    .map_err(|_| CliError::new("--port must be an integer"))?;
            }
            other => return Err(CliError::new(format!("unknown ui option '{other}'"))),
        }
    }
    Ok(Some(options))
}

fn parse_webchat<I>(args: I) -> Result<Option<WebchatOptions>, CliError>
where
    I: IntoIterator<Item = String>,
{
    let mut options = WebchatOptions::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => return Ok(None),
            "--host" => options.host = next_value(&mut args, "--host")?,
            "--port" => {
                options.port = next_value(&mut args, "--port")?
                    .parse()
                    .map_err(|_| CliError::new("--port must be an integer"))?;
            }
            "--dry-run" => options.dry_run = true,
            "--timeout" => {
                options.timeout_seconds = Some(
                    next_value(&mut args, "--timeout")?
                        .parse()
                        .map_err(|_| CliError::new("--timeout must be a number"))?,
                );
            }
            "--no-open" => options.no_open = true,
            other => return Err(CliError::new(format!("unknown webchat option '{other}'"))),
        }
    }
    Ok(Some(options))
}

pub fn webchat_serve_options(options: &WebchatOptions) -> ServeOptions {
    ServeOptions {
        host: options.host.clone(),
        port: options.port,
        dry_run: options.dry_run,
        timeout_seconds: options.timeout_seconds,
        config: None,
    }
}

pub fn webchat_summary(options: &WebchatOptions) -> String {
    let note = if options.dry_run {
        "  (dry-run: routing decisions only, no model calls)"
    } else {
        ""
    };
    format!(
        "wayfinder-router webchat -> {}{note}  (Ctrl-C to stop)\n",
        demo_url(&options.host, options.port)
    )
}

pub fn demo_url(host: &str, port: u16) -> String {
    let display = if matches!(host, "0.0.0.0" | "::" | "") {
        "127.0.0.1"
    } else {
        host
    };
    format!("http://{display}:{port}/demo")
}

/// The arguments a service manager should launch the gateway with.
///
/// When `config` is given it is appended as `--config PATH`, so a service-managed gateway
/// loads a fixed file rather than walking up from a working directory it does not control.
pub fn resolve_serve_args(host: &str, port: u16, config: Option<&Path>) -> Vec<String> {
    let executable = std::env::current_exe()
        .ok()
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from("wayfinder-router"));
    let mut args = vec![
        executable.to_string_lossy().into_owned(),
        "serve".to_owned(),
        "--host".to_owned(),
        host.to_owned(),
        "--port".to_owned(),
        port.to_string(),
    ];
    if let Some(config) = config {
        args.push("--config".to_owned());
        args.push(config.to_string_lossy().into_owned());
    }
    args
}

fn execute_service(options: ServiceOptions) -> Result<CommandOutput, CliError> {
    let platform = detect_platform(None);
    if platform == ServicePlatform::Other {
        return Err(CliError::with_output(
            "unsupported service platform",
            EXIT_USAGE,
            "",
            "wayfinder-router: service supports macOS (launchd) and Linux (systemd user units); elsewhere run `wayfinder-router serve` yourself.\n",
        ));
    }

    let program_args = resolve_serve_args(&options.host, options.port, options.config.as_deref());
    let endpoint = format!("http://{}:{}/v1", options.host, options.port);
    let (unit_text, unit_file, manager) = match platform {
        ServicePlatform::Macos => (
            launchd_plist(&program_args, LAUNCHD_LABEL, "~/Library/Logs"),
            agent_path(None),
            which("launchctl"),
        ),
        ServicePlatform::Linux => (
            systemd_unit(&program_args, "Wayfinder router gateway"),
            systemd_unit_path(None),
            which("systemctl"),
        ),
        ServicePlatform::Other => unreachable!("other handled above"),
    };

    match options.action.as_str() {
        "install" => {
            if options.print {
                return Ok(CommandOutput {
                    stdout: unit_text,
                    stderr: String::new(),
                });
            }
            if let Some(parent) = unit_file.parent() {
                fs::create_dir_all(parent).map_err(|err| {
                    CliError::usage(format!("cannot create {}: {err}", parent.display()))
                })?;
            }
            fs::write(&unit_file, unit_text).map_err(|err| {
                CliError::usage(format!("cannot write {}: {err}", unit_file.display()))
            })?;
            let mut stderr = String::new();
            match (platform, manager.as_deref()) {
                (ServicePlatform::Macos, Some(manager)) => {
                    let uid = current_uid();
                    let bootstrap = run_manager(
                        manager,
                        &[
                            "bootstrap",
                            &format!("gui/{uid}"),
                            unit_file.to_string_lossy().as_ref(),
                        ],
                    );
                    let loaded = if bootstrap.success {
                        bootstrap
                    } else {
                        run_manager(
                            manager,
                            &["load", "-w", unit_file.to_string_lossy().as_ref()],
                        )
                    };
                    let probe =
                        run_manager(manager, &["print", &format!("gui/{uid}/{LAUNCHD_LABEL}")]);
                    if !probe.success {
                        let detail = loaded.detail();
                        stderr.push_str(&format!(
                            "wayfinder-router: launchctl could not load {}{}\n",
                            unit_file.display(),
                            detail_suffix(&detail)
                        ));
                        return Err(CliError::with_output(
                            "launchctl could not load service",
                            EXIT_CONFIG,
                            "",
                            stderr,
                        ));
                    }
                    stderr.push_str(&format!(
                        "wayfinder-router: installed and loaded {}\n",
                        unit_file.display()
                    ));
                }
                (ServicePlatform::Linux, Some(manager)) => {
                    let _ = run_manager(manager, &["--user", "daemon-reload"]);
                    let enabled =
                        run_manager(manager, &["--user", "enable", "--now", SYSTEMD_UNIT_NAME]);
                    if !enabled.success {
                        let detail = enabled.detail();
                        stderr.push_str(&format!(
                            "wayfinder-router: systemctl could not enable {}{}\n",
                            SYSTEMD_UNIT_NAME,
                            detail_suffix(&detail)
                        ));
                        return Err(CliError::with_output(
                            "systemctl could not enable service",
                            EXIT_CONFIG,
                            "",
                            stderr,
                        ));
                    }
                    stderr.push_str(&format!(
                        "wayfinder-router: installed and started {}\n",
                        unit_file.display()
                    ));
                }
                (ServicePlatform::Macos, None) => {
                    stderr.push_str(&format!(
                        "wayfinder-router: wrote {}; start it with:\n  launchctl bootstrap gui/$(id -u) {}\n",
                        unit_file.display(),
                        unit_file.display()
                    ));
                }
                (ServicePlatform::Linux, None) => {
                    stderr.push_str(&format!(
                        "wayfinder-router: wrote {}; start it with:\n  systemctl --user enable --now {}\n",
                        unit_file.display(),
                        SYSTEMD_UNIT_NAME
                    ));
                }
                (ServicePlatform::Other, _) => unreachable!("other handled above"),
            }
            stderr.push_str(&format!(
                "wayfinder-router: point your apps at OPENAI_BASE_URL={endpoint}\n"
            ));
            Ok(CommandOutput {
                stdout: String::new(),
                stderr,
            })
        }
        "uninstall" => {
            if let (ServicePlatform::Macos, Some(manager)) = (platform, manager.as_deref()) {
                let uid = current_uid();
                let _ = run_manager(manager, &["bootout", &format!("gui/{uid}/{LAUNCHD_LABEL}")]);
                let _ = run_manager(
                    manager,
                    &["unload", "-w", unit_file.to_string_lossy().as_ref()],
                );
            }
            if let (ServicePlatform::Linux, Some(manager)) = (platform, manager.as_deref()) {
                let _ = run_manager(manager, &["--user", "disable", "--now", SYSTEMD_UNIT_NAME]);
            }
            let existed = unit_file.is_file();
            if existed {
                fs::remove_file(&unit_file).map_err(|err| {
                    CliError::usage(format!("cannot remove {}: {err}", unit_file.display()))
                })?;
            }
            let stderr = if existed {
                format!("wayfinder-router: removed {}\n", unit_file.display())
            } else {
                format!(
                    "wayfinder-router: nothing to remove ({} not present)\n",
                    unit_file.display()
                )
            };
            Ok(CommandOutput {
                stdout: String::new(),
                stderr,
            })
        }
        "status" => {
            let installed = unit_file.is_file();
            let mut stderr = format!(
                "unit file: {} ({})\nendpoint:  {endpoint}\n",
                unit_file.display(),
                if installed { "present" } else { "absent" }
            );
            if let (Some(manager), true) = (manager.as_deref(), installed) {
                match platform {
                    ServicePlatform::Macos => {
                        let uid = current_uid();
                        let probe =
                            run_manager(manager, &["print", &format!("gui/{uid}/{LAUNCHD_LABEL}")]);
                        stderr.push_str(&format!(
                            "launchd:   {}\n",
                            if probe.success {
                                "loaded"
                            } else {
                                "not loaded"
                            }
                        ));
                    }
                    ServicePlatform::Linux => {
                        let probe =
                            run_manager(manager, &["--user", "is-active", SYSTEMD_UNIT_NAME]);
                        stderr.push_str(&format!(
                            "systemd:   {}\n",
                            non_empty_line(&probe.stdout).unwrap_or("unknown")
                        ));
                    }
                    ServicePlatform::Other => {}
                }
            }
            stderr.push_str(&format!(
                "health:    {}\n",
                probe_health(&options.host, options.port)
            ));
            if !installed {
                stderr.push_str(&format!(
                    "\ninstall with: wayfinder-router service install --port {}\n",
                    options.port
                ));
            }
            Ok(CommandOutput {
                stdout: String::new(),
                stderr,
            })
        }
        _ => Err(CliError::new(
            "service action must be install, uninstall, or status",
        )),
    }
}

#[derive(Debug)]
struct ProcessResult {
    success: bool,
    stdout: String,
    stderr: String,
}

impl ProcessResult {
    fn detail(&self) -> String {
        if self.stderr.trim().is_empty() {
            self.stdout.trim().to_owned()
        } else {
            self.stderr.trim().to_owned()
        }
    }
}

fn run_manager(program: &str, args: &[&str]) -> ProcessResult {
    match Command::new(program).args(args).output() {
        Ok(output) => ProcessResult {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        },
        Err(err) => ProcessResult {
            success: false,
            stdout: String::new(),
            stderr: err.to_string(),
        },
    }
}

fn detail_suffix(detail: &str) -> String {
    if detail.is_empty() {
        String::new()
    } else {
        format!(": {detail}")
    }
}

fn non_empty_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|line| !line.is_empty())
}

fn which(program: &str) -> Option<String> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|path| path.join(program))
        .find(|path| path.is_file())
        .map(|path| path.to_string_lossy().into_owned())
}

fn current_uid() -> String {
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|uid| !uid.is_empty())
        .unwrap_or_else(|| "0".to_owned())
}

fn probe_health(host: &str, port: u16) -> String {
    let Ok(mut addrs) = (host, port).to_socket_addrs() else {
        return "unreachable (service not running?)".to_owned();
    };
    let Some(addr) = addrs.next() else {
        return "unreachable (service not running?)".to_owned();
    };
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_millis(500)) else {
        return "unreachable (service not running?)".to_owned();
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
    let request =
        format!("GET /healthz HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).is_err() {
        return "unreachable (service not running?)".to_owned();
    }
    let mut response = String::new();
    if stream.read_to_string(&mut response).is_err() {
        return "unreachable (service not running?)".to_owned();
    }
    if !response.starts_with("HTTP/1.1 200") && !response.starts_with("HTTP/1.0 200") {
        let status = response
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("unknown");
        return format!("status {status}");
    }
    if response.contains("\"offline\":true") {
        "ok (200, offline routing on)".to_owned()
    } else {
        "ok (200)".to_owned()
    }
}

fn parse_chat<I>(args: I) -> Result<Option<ChatOptions>, CliError>
where
    I: IntoIterator<Item = String>,
{
    let mut options = ChatOptions::default();
    let mut args = args.into_iter();
    let mut prompt_parts = Vec::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => return Ok(None),
            "--theme" => options.theme = next_value(&mut args, "--theme")?,
            "--threshold" => {
                options.threshold = Some(
                    next_value(&mut args, "--threshold")?
                        .parse()
                        .map_err(|_| CliError::new("--threshold must be a number"))?,
                );
            }
            "--why" => options.show_why = true,
            "--dry-run" => options.dry_run = true,
            "--no-stream" => options.stream = false,
            "--base-url" => options.base_url = Some(next_value(&mut args, "--base-url")?),
            "--thread-dir" => {
                options.thread_dir = Some(next_value(&mut args, "--thread-dir")?.into())
            }
            "--" => {
                prompt_parts.extend(args);
                break;
            }
            other if other.starts_with('-') => {
                return Err(CliError::new(format!("unknown chat option '{other}'")));
            }
            text => prompt_parts.push(text.to_string()),
        }
    }
    if !prompt_parts.is_empty() {
        options.input = Some(prompt_parts.join(" "));
    }
    Ok(Some(options))
}

fn parse_init<I>(args: I) -> Result<Option<InitOptions>, CliError>
where
    I: IntoIterator<Item = String>,
{
    let mut options = InitOptions::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => return Ok(None),
            "-i" | "--interactive" => options.interactive = true,
            "--preset" => options.preset = next_value(&mut args, "--preset")?,
            "--path" => options.path = next_value(&mut args, "--path")?.into(),
            "--force" => options.force = true,
            "--print" => options.print = true,
            other => return Err(CliError::new(format!("unknown init option '{other}'"))),
        }
    }
    Ok(Some(options))
}

fn parse_doctor<I>(args: I) -> Result<Option<DoctorOptions>, CliError>
where
    I: IntoIterator<Item = String>,
{
    let mut options = DoctorOptions::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => return Ok(None),
            "--dir" => options.dir = next_value(&mut args, "--dir")?.into(),
            other => return Err(CliError::new(format!("unknown doctor option '{other}'"))),
        }
    }
    Ok(Some(options))
}

fn parse_keys<I>(args: I) -> Result<Option<KeysOptions>, CliError>
where
    I: IntoIterator<Item = String>,
{
    let mut options = KeysOptions::default();
    let mut args = args.into_iter();
    let Some(action) = args.next() else {
        return Err(CliError::new("keys requires an action (currently: new)"));
    };
    if action == "--help" || action == "-h" {
        return Ok(None);
    }
    if action != "new" {
        return Err(CliError::new("keys action must be new"));
    }
    options.action = action;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => return Ok(None),
            "--id" => options.id = next_value(&mut args, "--id")?,
            "--tag" => options.tags.push(next_value(&mut args, "--tag")?),
            other => return Err(CliError::new(format!("unknown keys option '{other}'"))),
        }
    }
    Ok(Some(options))
}

fn parse_route<I>(args: I) -> Result<Option<RouteOptions>, CliError>
where
    I: IntoIterator<Item = String>,
{
    let mut args = args.into_iter();
    let mut prompt = None;
    let mut threshold = None;
    let mut json = false;
    let mut explain = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => return Ok(None),
            "--threshold" => {
                threshold = Some(
                    next_value(&mut args, "--threshold")?
                        .parse()
                        .map_err(|_| CliError::new("--threshold must be a number"))?,
                );
            }
            "--json" => json = true,
            "--explain" => explain = true,
            "--" => {
                prompt = Some(next_value(&mut args, "route prompt")?);
                if args.next().is_some() {
                    return Err(CliError::new("route accepts exactly one prompt"));
                }
                break;
            }
            other if other.starts_with('-') && other != "-" => {
                return Err(CliError::new(format!("unknown route option '{other}'")));
            }
            text => {
                if prompt.is_some() {
                    return Err(CliError::new("route accepts exactly one prompt"));
                }
                prompt = Some(text.to_owned());
            }
        }
    }
    let prompt = prompt.ok_or_else(|| CliError::new("route requires a prompt file or '-'"))?;
    Ok(Some(RouteOptions {
        prompt,
        threshold,
        json,
        explain,
        input: None,
    }))
}

fn parse_calibrate<I>(args: I) -> Result<Option<CalibrateOptions>, CliError>
where
    I: IntoIterator<Item = String>,
{
    let mut options = CalibrateOptions::default();
    let mut dataset = None;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => return Ok(None),
            "--mode" => {
                options.mode = parse_choice(
                    "--mode",
                    next_value(&mut args, "--mode")?,
                    &["threshold", "tiers", "classifier"],
                )?
            }
            "--models" => options.models = Some(next_value(&mut args, "--models")?),
            "--out" => options.out = Some(next_value(&mut args, "--out")?.into()),
            "--iterations" => {
                options.iterations = next_value(&mut args, "--iterations")?
                    .parse()
                    .map_err(|_| CliError::new("--iterations must be an integer"))?;
            }
            "--l2" => {
                options.l2 = next_value(&mut args, "--l2")?
                    .parse()
                    .map_err(|_| CliError::new("--l2 must be a number"))?;
            }
            "--objective" => {
                options.objective = parse_choice(
                    "--objective",
                    next_value(&mut args, "--objective")?,
                    &["accuracy", "knee", "cost-quality"],
                )?
            }
            "--target-savings" => {
                options.target_savings = Some(
                    next_value(&mut args, "--target-savings")?
                        .parse()
                        .map_err(|_| CliError::new("--target-savings must be a number"))?,
                );
            }
            "--costs" => options.costs = Some(next_value(&mut args, "--costs")?),
            "--weights" => options.weights = Some(next_value(&mut args, "--weights")?),
            "--" => {
                dataset = Some(next_value(&mut args, "calibrate dataset")?);
                if args.next().is_some() {
                    return Err(CliError::new("calibrate accepts exactly one dataset"));
                }
                break;
            }
            other if other.starts_with('-') => {
                return Err(CliError::new(format!("unknown calibrate option '{other}'")));
            }
            text => {
                if dataset.is_some() {
                    return Err(CliError::new("calibrate accepts exactly one dataset"));
                }
                dataset = Some(text.to_owned());
            }
        }
    }
    options.dataset = dataset
        .ok_or_else(|| CliError::new("calibrate requires a dataset"))?
        .into();
    Ok(Some(options))
}

fn parse_recalibrate<I>(args: I) -> Result<Option<RecalibrateOptions>, CliError>
where
    I: IntoIterator<Item = String>,
{
    let mut options = RecalibrateOptions::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => return Ok(None),
            "--log" => options.log = next_value(&mut args, "--log")?.into(),
            "--out" => options.out = next_value(&mut args, "--out")?.into(),
            "--mode" => {
                options.mode = parse_choice(
                    "--mode",
                    next_value(&mut args, "--mode")?,
                    &["threshold", "tiers", "classifier"],
                )?
            }
            "--min-labels" => {
                options.min_labels = next_value(&mut args, "--min-labels")?
                    .parse()
                    .map_err(|_| CliError::new("--min-labels must be an integer"))?;
            }
            other => {
                return Err(CliError::new(format!(
                    "unknown recalibrate option '{other}'"
                )))
            }
        }
    }
    Ok(Some(options))
}

fn parse_onboard<I>(args: I) -> Result<Option<OnboardOptions>, CliError>
where
    I: IntoIterator<Item = String>,
{
    let mut options = OnboardOptions::default();
    let mut prompts = None;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => return Ok(None),
            "--arms" => options.arms = Some(next_value(&mut args, "--arms")?),
            "--log" => options.log = next_value(&mut args, "--log")?.into(),
            "--calibrate" => options.calibrate = true,
            "--mode" => {
                options.mode = parse_choice(
                    "--mode",
                    next_value(&mut args, "--mode")?,
                    &["threshold", "tiers", "classifier"],
                )?
            }
            "--" => {
                prompts = Some(next_value(&mut args, "onboard prompts")?);
                if args.next().is_some() {
                    return Err(CliError::new("onboard accepts exactly one prompts file"));
                }
                break;
            }
            other if other.starts_with('-') => {
                return Err(CliError::new(format!("unknown onboard option '{other}'")));
            }
            text => {
                if prompts.is_some() {
                    return Err(CliError::new("onboard accepts exactly one prompts file"));
                }
                prompts = Some(text.to_owned());
            }
        }
    }
    options.prompts = prompts
        .ok_or_else(|| CliError::new("onboard requires a prompts file"))?
        .into();
    Ok(Some(options))
}

fn parse_judge<I>(args: I) -> Result<Option<JudgeOptions>, CliError>
where
    I: IntoIterator<Item = String>,
{
    let mut options = JudgeOptions::default();
    let mut prompts = None;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => return Ok(None),
            "--arms" => options.arms = Some(next_value(&mut args, "--arms")?),
            "--gold" => options.gold = Some(next_value(&mut args, "--gold")?.into()),
            "--log" => options.log = next_value(&mut args, "--log")?.into(),
            "--mode" => {
                options.mode = parse_choice(
                    "--mode",
                    next_value(&mut args, "--mode")?,
                    &["threshold", "tiers", "classifier"],
                )?
            }
            "--kappa-floor" => {
                options.kappa_floor = next_value(&mut args, "--kappa-floor")?
                    .parse()
                    .map_err(|_| CliError::new("--kappa-floor must be a number"))?;
            }
            "--folds" => {
                options.folds = next_value(&mut args, "--folds")?
                    .parse()
                    .map_err(|_| CliError::new("--folds must be an integer"))?;
            }
            "--limit" => {
                options.limit = Some(
                    next_value(&mut args, "--limit")?
                        .parse()
                        .map_err(|_| CliError::new("--limit must be an integer"))?,
                );
            }
            "--save-comparisons" => {
                options.save_comparisons = Some(next_value(&mut args, "--save-comparisons")?.into())
            }
            "--" => {
                prompts = Some(next_value(&mut args, "judge prompts")?);
                if args.next().is_some() {
                    return Err(CliError::new("judge accepts exactly one prompts file"));
                }
                break;
            }
            other if other.starts_with('-') => {
                return Err(CliError::new(format!("unknown judge option '{other}'")));
            }
            text => {
                if prompts.is_some() {
                    return Err(CliError::new("judge accepts exactly one prompts file"));
                }
                prompts = Some(text.to_owned());
            }
        }
    }
    options.prompts = prompts
        .ok_or_else(|| CliError::new("judge requires a prompts file"))?
        .into();
    Ok(Some(options))
}

fn execute_route(options: RouteOptions) -> Result<CommandOutput, CliError> {
    if let Some(threshold) = options.threshold {
        if !(0.0..=1.0).contains(&threshold) {
            return Err(CliError::usage(
                "--threshold must be a number between 0.0 and 1.0",
            ));
        }
    }

    let (text, start_dir) = if options.prompt == "-" {
        (options.input.unwrap_or_default(), PathBuf::from("."))
    } else {
        let path = PathBuf::from(&options.prompt);
        if !path.is_file() {
            return Err(CliError::usage(format!(
                "file not found: {}",
                options.prompt
            )));
        }
        let text = fs::read_to_string(&path)
            .map_err(|err| CliError::usage(format!("cannot read {}: {err}", path.display())))?;
        let start_dir = path
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        (text, start_dir)
    };

    let mut config =
        load_routing_config(&start_dir).map_err(|err| CliError::config(err.to_string()))?;
    if let Some(threshold) = options.threshold {
        config = RoutingConfig {
            weights: config.weights,
            tiers: binary_tiers(threshold),
            ..RoutingConfig::default()
        };
    }
    let result = score_complexity(&text, &config);
    let stdout = if options.json {
        format!(
            "{}\n",
            serde_json::to_string_pretty(&result)
                .map_err(|err| CliError::config(err.to_string()))?
        )
    } else {
        let weights = options.explain.then_some(config.weights);
        format!("{}\n", render_human(&result, weights))
    };
    Ok(CommandOutput {
        stdout,
        stderr: String::new(),
    })
}

fn execute_calibrate(options: CalibrateOptions) -> Result<CommandOutput, CliError> {
    if !options.dataset.is_file() {
        return Err(CliError::usage(format!(
            "file not found: {}",
            options.dataset.display()
        )));
    }
    let models_order = options.models.as_ref().map(|models| {
        models
            .split(',')
            .map(|model| model.trim().to_owned())
            .collect::<Vec<_>>()
    });
    let costs = parse_costs(options.costs.as_deref())?;
    let parsed_weights = parse_weights_arg(options.weights.as_deref())?;
    let weights = parsed_weights.as_ref().map(|parsed| parsed.weights);
    let samples =
        load_dataset(&options.dataset).map_err(|err| CliError::config(err.to_string()))?;
    let mut result = calibrate(
        &samples,
        &options.mode,
        CalibrationOptions {
            models_order,
            iterations: options.iterations,
            l2: options.l2,
            objective: options.objective,
            costs,
            target_savings: options.target_savings,
            weights,
        },
    )
    .map_err(|err| CliError::config(err.to_string()))?;
    if let Some(parsed) = &parsed_weights {
        result.toml = replace_weights_block(&result.toml, &parsed.supplied);
    }

    render_calibration_output(result, options.out)
}

fn render_calibration_output(
    result: CalibrationResult,
    out: Option<PathBuf>,
) -> Result<CommandOutput, CliError> {
    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(out) = out {
        fs::write(&out, &result.toml)
            .map_err(|err| CliError::usage(format!("cannot write {}: {err}", out.display())))?;
        stderr.push_str(&format!("wayfinder-router: wrote {}\n", out.display()));
    } else {
        stdout.push_str(&result.toml);
        stdout.push('\n');
    }
    stderr.push_str(&format!(
        "wayfinder-router: {}\n",
        summary_bits(&result.summary)
    ));
    Ok(CommandOutput { stdout, stderr })
}

fn execute_recalibrate(options: RecalibrateOptions) -> Result<CommandOutput, CliError> {
    let result = recalibrate(
        &options.log,
        &options.out,
        &options.mode,
        options.min_labels,
    )
    .map_err(|err| CliError::config(err.to_string()))?;
    let stderr = if !result.written {
        format!(
            "wayfinder-router: skipped — {}\n",
            result
                .reason
                .unwrap_or_else(|| "no reason provided".to_owned())
        )
    } else {
        let summary = result
            .summary
            .as_ref()
            .map(summary_bits)
            .unwrap_or_default();
        format!(
            "wayfinder-router: recalibrated {} from {} labels — {}\n",
            options.out.display(),
            result.label_count,
            summary
        )
    };
    Ok(CommandOutput {
        stdout: String::new(),
        stderr,
    })
}

fn execute_onboard(options: OnboardOptions) -> Result<CommandOutput, CliError> {
    let models = load_gateway_models(&PathBuf::from("."))
        .map_err(|err| CliError::config(err.to_string()))?;
    wayfinder_internal_gateway::bootstrap::resolve_keys(&models);
    let arms = resolve_arms(
        options.arms.as_deref(),
        models.keys().cloned(),
        "onboard needs two gateway models (e.g. local and hosted); configure [gateway.models.*] or pass --arms local,cloud",
    )?;
    ensure_arms_configured(&arms, &models)?;
    let primary = arms[0].clone();
    let fallback = arms[1].clone();

    execute_onboard_selected(
        options,
        arms,
        |arm, prompt| invoke_gateway_model(models.get(arm).expect("arm was validated"), prompt),
        |prompt, outputs| {
            eprintln!("\n--- prompt ---\n{prompt}\n");
            eprintln!("[{primary}]\n{}\n", outputs[&primary]);
            eprintln!("[{fallback}]\n{}\n", outputs[&fallback]);
            eprint!("Is '{primary}' good enough? [y/N] ");
            let _ = std::io::stderr().flush();
            let mut answer = String::new();
            let _ = std::io::stdin().read_line(&mut answer);
            let answer = answer.trim().to_ascii_lowercase();
            if answer == "y" || answer == "yes" {
                Some(primary.clone())
            } else {
                Some(fallback.clone())
            }
        },
    )
}

fn execute_judge(options: JudgeOptions) -> Result<CommandOutput, CliError> {
    let models = load_gateway_models(&PathBuf::from("."))
        .map_err(|err| CliError::config(err.to_string()))?;
    wayfinder_internal_gateway::bootstrap::resolve_keys(&models);
    let arms = resolve_arms(
        options.arms.as_deref(),
        models.keys().cloned(),
        "judge needs two gateway models in cheap,expensive order; configure [gateway.models.*] or pass --arms cheap,expensive",
    )?;
    ensure_arms_configured(&arms, &models)?;

    execute_judge_selected(options, arms, |arm, prompt| {
        invoke_gateway_model(models.get(arm).expect("arm was validated"), prompt)
    })
}

fn execute_init(options: InitOptions) -> Result<CommandOutput, CliError> {
    if options.interactive {
        return Err(CliError::usage(
            "interactive init is not supported by the Rust CLI yet",
        ));
    }

    let preset = PRESETS
        .get(options.preset.as_str())
        .ok_or_else(|| unknown_preset_error(&options.preset))?;
    let config_text = render_config(preset);
    if options.print {
        return Ok(CommandOutput {
            stdout: config_text,
            stderr: String::new(),
        });
    }

    let target = options.path;
    if target.exists() && !options.force {
        let stderr = format!(
            "wayfinder-router: {} already exists — use --force to overwrite, or run `wayfinder-router doctor` to check it\n",
            target.display()
        );
        return Err(CliError::with_output(
            stderr.trim_end().to_owned(),
            EXIT_USAGE,
            "",
            stderr,
        ));
    }
    fs::write(&target, &config_text)
        .map_err(|err| CliError::usage(format!("cannot write {}: {err}", target.display())))?;

    let mut stdout = format!(
        "✓ wrote {}  (preset: {} — {})\n",
        target.display(),
        preset.name,
        preset.summary
    );
    let mut stderr = String::new();

    if !preset.env_vars.is_empty() {
        let env_path = target
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .join(".env.example");
        if env_path.exists() && !options.force {
            stdout.push_str(&format!(
                "· kept existing {} (use --force to overwrite)\n",
                env_path.display()
            ));
        } else if let Err(err) = fs::write(&env_path, render_env_example(preset)) {
            stderr.push_str(&format!(
                "wayfinder-router: cannot write {}: {err}\n",
                env_path.display()
            ));
        } else {
            stdout.push_str(&format!(
                "✓ wrote {}  (env-var names only — no secrets)\n",
                env_path.display()
            ));
        }
    }

    let gateway = gateway_config_from_toml(&config_text, "<init>")
        .map_err(|err| CliError::config(err.to_string()))?;
    let statuses = key_status(&gateway.models);
    stdout.push('\n');
    stdout.push_str(&render_key_report(&statuses));
    stdout.push('\n');
    stdout.push('\n');
    let missing = missing_keys(&statuses);
    if !missing.is_empty() {
        stdout.push_str(
            "set your key(s) — read from the environment at request time, never stored:\n",
        );
        stdout.push_str(&render_key_remedies(&missing));
        stdout.push('\n');
    }
    stdout.push_str(
        "next:  wayfinder-router chat        # or `wayfinder-router doctor` to re-check\n",
    );
    Ok(CommandOutput { stdout, stderr })
}

fn execute_doctor(options: DoctorOptions) -> Result<CommandOutput, CliError> {
    let Some(path) = find_config_file(&options.dir) else {
        let stderr = "no wayfinder-router.toml found — run `wayfinder-router init` to create one\n";
        return Err(CliError::with_output(
            stderr.trim_end(),
            EXIT_USAGE,
            "",
            stderr,
        ));
    };
    let routing =
        load_routing_config(&options.dir).map_err(|err| CliError::config(err.to_string()))?;
    let models =
        load_gateway_models(&options.dir).map_err(|err| CliError::config(err.to_string()))?;

    let mut stdout = format!(
        "config:  {}\nrouting: {}\n",
        path.display(),
        summarize_routing(&routing)
    );
    if models.is_empty() {
        stdout.push_str(
            "models:  none configured — add [gateway.models] (see `wayfinder-router init`)\n",
        );
        stdout.push_str("(chat / serve will show routing decisions only)\n");
        return Ok(CommandOutput {
            stdout,
            stderr: String::new(),
        });
    }

    let cmd_errors = resolve_keys(&models);
    let statuses = key_status(&models);
    stdout.push('\n');
    stdout.push_str(&render_key_report(&statuses));
    stdout.push('\n');
    stdout.push('\n');
    if !cmd_errors.is_empty() {
        stdout.push_str("key command(s) failed:\n");
        for (name, reason) in cmd_errors {
            stdout.push_str(&format!("  {name}: {reason}\n"));
        }
        stdout.push('\n');
    }
    let missing = missing_keys(&statuses);
    if !missing.is_empty() {
        stdout.push_str("not ready — set the missing key(s):\n");
        stdout.push_str(&render_key_remedies(&missing));
        return Err(CliError::with_output(
            stdout.trim_end().to_owned(),
            EXIT_CONFIG,
            stdout,
            "",
        ));
    }
    stdout.push_str("ready:  wayfinder-router chat\n");
    Ok(CommandOutput {
        stdout,
        stderr: String::new(),
    })
}

fn execute_keys(options: KeysOptions) -> Result<CommandOutput, CliError> {
    if options.action != "new" {
        return Err(CliError::usage("keys action must be new"));
    }
    let generated = vkeys::generate(vkeys::KEY_PREFIX);
    let key_hash = vkeys::hash_key(&generated.plaintext);
    let mut lines = vec![
        "# Paste into wayfinder-router.toml (only the hash is stored — never the key):".to_owned(),
        format!("[gateway.keys.{}]", toml_key(&options.id)),
        format!("hash = \"{key_hash}\""),
    ];
    if !options.tags.is_empty() {
        let tags = options
            .tags
            .iter()
            .map(|tag| toml_string(tag))
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(format!("tags = [{tags}]"));
    }
    lines.push(String::new());
    lines.push(
        "# Give this key to the caller; it is shown once and cannot be recovered:".to_owned(),
    );
    lines.push(generated.plaintext);
    Ok(CommandOutput {
        stdout: format!("{}\n", lines.join("\n")),
        stderr: String::new(),
    })
}

fn toml_key(key: &str) -> String {
    if key
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        key.to_owned()
    } else {
        toml_string(key)
    }
}

fn toml_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_owned())
}

fn unknown_preset_error(preset: &str) -> CliError {
    let choices = PRESETS.keys().copied().collect::<Vec<_>>().join(", ");
    let stderr = format!("wayfinder-router: unknown preset '{preset}' (choose: {choices})\n");
    CliError::with_output(stderr.trim_end().to_owned(), EXIT_USAGE, "", stderr)
}

fn render_key_report(statuses: &[wayfinder_internal_gateway::bootstrap::KeyStatus]) -> String {
    let mut lines = vec!["models".to_owned()];
    for status in statuses {
        let key = match (&status.env_var, status.ok, &status.cmd) {
            (None, _, _) => "keyless ✓".to_owned(),
            (Some(env_var), true, Some(_)) => format!("{env_var} ✓ set (via command)"),
            (Some(env_var), true, None) => format!("{env_var} ✓ set"),
            (Some(env_var), false, _) => format!("{env_var} ✗ not set"),
        };
        lines.push(format!(
            "  {:<7} {:<24} {:<30} {}",
            status.name, status.model, status.base_url, key
        ));
    }
    lines.join("\n")
}

fn render_key_remedies(missing: &[String]) -> String {
    let mut lines = Vec::new();
    for var in missing {
        lines.push(format!("  export {var}=\"...\""));
        for suggestion in suggest_key_commands(var) {
            lines.push(format!(
                "  · or store it safely and add:  api_key_cmd = \"{suggestion}\""
            ));
        }
    }
    if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    }
}

fn summarize_routing(config: &RoutingConfig) -> String {
    if let Some(classifier) = &config.classifier {
        return format!("classifier ({} models)", classifier.models.len());
    }
    if config.tiers.is_empty() {
        return "defaults".to_owned();
    }
    config
        .tiers
        .iter()
        .map(|tier| format!("{} ≥{:.2}", tier.model, tier.min_score))
        .collect::<Vec<_>>()
        .join(" · ")
}

#[cfg(test)]
fn execute_onboard_with_invoker<I, R, J>(
    options: OnboardOptions,
    available_arms: I,
    mut run_model: R,
    judge: J,
) -> Result<CommandOutput, CliError>
where
    I: IntoIterator<Item = String>,
    R: FnMut(&str, &str) -> String,
    J: FnMut(&str, &wayfinder_internal_core::judge::OnboardOutputs) -> Option<String>,
{
    let arms = resolve_arms(
        options.arms.as_deref(),
        available_arms,
        "onboard needs two gateway models (e.g. local and hosted); configure [gateway.models.*] or pass --arms local,cloud",
    )?;
    execute_onboard_selected(
        options,
        arms,
        |arm, prompt| Ok(run_model(arm, prompt)),
        judge,
    )
}

#[cfg(test)]
fn execute_onboard_with_invoker_result<I, R, J>(
    options: OnboardOptions,
    available_arms: I,
    run_model: R,
    judge: J,
) -> Result<CommandOutput, CliError>
where
    I: IntoIterator<Item = String>,
    R: FnMut(&str, &str) -> Result<String, String>,
    J: FnMut(&str, &wayfinder_internal_core::judge::OnboardOutputs) -> Option<String>,
{
    let arms = resolve_arms(
        options.arms.as_deref(),
        available_arms,
        "onboard needs two gateway models (e.g. local and hosted); configure [gateway.models.*] or pass --arms local,cloud",
    )?;
    execute_onboard_selected(options, arms, map_string_error(run_model), judge)
}

fn execute_onboard_selected<R, J>(
    options: OnboardOptions,
    arms: Vec<String>,
    run_model: R,
    judge: J,
) -> Result<CommandOutput, CliError>
where
    R: FnMut(&str, &str) -> Result<String, CliError>,
    J: FnMut(&str, &wayfinder_internal_core::judge::OnboardOutputs) -> Option<String>,
{
    ensure_file(&options.prompts)?;
    let prompts = load_prompts(&options.prompts)?;
    let summary = run_onboarding_fallible(prompts, &arms, run_model, judge, &options.log)?;
    let mut output = CommandOutput {
        stdout: String::new(),
        stderr: format!(
            "wayfinder-router: judged {} prompts -> {}\nwayfinder-router: labels appended to {}\n",
            summary.judged,
            render_label_counts_map(&summary.label_counts),
            options.log.display()
        ),
    };

    if options.calibrate {
        let samples =
            load_dataset(&options.log).map_err(|err| CliError::config(err.to_string()))?;
        let result = calibrate(&samples, &options.mode, CalibrationOptions::default())
            .map_err(|err| CliError::config(err.to_string()))?;
        let calibrated = render_calibration_output(result, None)?;
        output.stdout.push_str(&calibrated.stdout);
        output.stderr.push_str(&calibrated.stderr);
    }

    Ok(output)
}

#[cfg(test)]
fn execute_judge_with_invoker<I, R>(
    options: JudgeOptions,
    available_arms: I,
    mut run_model: R,
) -> Result<CommandOutput, CliError>
where
    I: IntoIterator<Item = String>,
    R: FnMut(&str, &str) -> String,
{
    let arms = resolve_arms(
        options.arms.as_deref(),
        available_arms,
        "judge needs two gateway models in cheap,expensive order; configure [gateway.models.*] or pass --arms cheap,expensive",
    )?;
    execute_judge_selected(options, arms, |arm, prompt| Ok(run_model(arm, prompt)))
}

fn execute_judge_selected<R>(
    options: JudgeOptions,
    arms: Vec<String>,
    mut run_model: R,
) -> Result<CommandOutput, CliError>
where
    R: FnMut(&str, &str) -> Result<String, CliError>,
{
    ensure_file(&options.prompts)?;
    if let Some(gold) = &options.gold {
        ensure_file(gold)?;
    }

    let cheap = arms[0].clone();
    let expensive = arms[1].clone();
    let judge_impl = HeuristicJudge::new();
    let mut gold_pairs = Vec::<(String, String)>::new();
    let mut gold_abstained = 0usize;

    if let Some(gold) = &options.gold {
        for row in read_labels(gold).map_err(|err| CliError::config(err.to_string()))? {
            let outputs = run_all_arms(&arms, &row.text, &mut run_model)?;
            let verdict = judge_impl.judge(&row.text, &outputs[&cheap], &outputs[&expensive]);
            write_comparison_if_requested(
                options.save_comparisons.as_deref(),
                &row.text,
                &outputs,
                &cheap,
                &expensive,
                &judge_impl,
                &verdict,
            )?;
            match verdict.sufficient {
                Some(true) => gold_pairs.push((cheap.clone(), row.label)),
                Some(false) => gold_pairs.push((expensive.clone(), row.label)),
                None => gold_abstained += 1,
            }
        }
    }

    let mut prompts = load_prompts(&options.prompts)?;
    if let Some(limit) = options.limit {
        prompts.truncate(limit);
    }

    let mut summary = OnboardSummary::default();
    for prompt in prompts {
        let outputs = run_all_arms(&arms, &prompt, &mut run_model)?;
        let verdict = judge_impl.judge(&prompt, &outputs[&cheap], &outputs[&expensive]);
        write_comparison_if_requested(
            options.save_comparisons.as_deref(),
            &prompt,
            &outputs,
            &cheap,
            &expensive,
            &judge_impl,
            &verdict,
        )?;
        let label = match verdict.sufficient {
            Some(true) => cheap.clone(),
            Some(false) => expensive.clone(),
            None => {
                summary.abstained += 1;
                continue;
            }
        };
        record_label(&options.log, &prompt, &label)
            .map_err(|err| CliError::config(err.to_string()))?;
        summary.judged += 1;
        *summary.label_counts.entry(label).or_default() += 1;
    }

    let mut stderr = format!(
        "wayfinder-router: judged {} prompts ({} abstained) -> {}\nwayfinder-router: labels appended to {}\n",
        summary.judged,
        summary.abstained,
        render_label_counts_map(&summary.label_counts),
        options.log.display()
    );

    let samples = load_dataset(&options.log).map_err(|err| CliError::config(err.to_string()))?;
    let report = evaluate_with_options(
        &gold_pairs,
        &samples,
        EvaluateOptions {
            kappa_floor: options.kappa_floor,
            k: options.folds,
            gold_abstained,
            ..EvaluateOptions::default()
        },
    );
    stderr.push_str(&report.render());
    stderr.push('\n');
    if !report.passed {
        let banner = judge_provenance_banner_with_title(
            "refused config",
            judge_impl.version(),
            &options,
            samples.len(),
            &report,
        );
        stderr.push_str(&banner);
        stderr.push('\n');
        stderr.push_str(
            "wayfinder-router: refusing to emit a config - trust gates failed (labels were still recorded to the log)\n",
        );
        return Err(CliError::config(stderr));
    }

    let result = calibrate(&samples, &options.mode, CalibrationOptions::default())
        .map_err(|err| CliError::config(err.to_string()))?;
    let mut stdout =
        judge_provenance_banner(judge_impl.version(), &options, samples.len(), &report);
    stdout.push('\n');
    stdout.push_str(&result.toml);
    stdout.push('\n');
    stderr.push_str(&format!(
        "wayfinder-router: {}\n",
        summary_bits(&result.summary)
    ));
    Ok(CommandOutput { stdout, stderr })
}

fn invoke_gateway_model(model: &GatewayModel, prompt: &str) -> Result<String, CliError> {
    invoke_messages(
        model,
        &[RelayMessage::new("user", prompt)],
        Duration::from_secs(60),
    )
    .map_err(|err| CliError::config(err.to_string()))
}

fn run_all_arms<R>(
    arms: &[String],
    prompt: &str,
    run_model: &mut R,
) -> Result<OnboardOutputs, CliError>
where
    R: FnMut(&str, &str) -> Result<String, CliError>,
{
    let mut outputs = OnboardOutputs::new();
    for arm in arms {
        outputs.insert(arm.clone(), run_model(arm, prompt)?);
    }
    Ok(outputs)
}

fn run_onboarding_fallible<P, S, R, J>(
    prompts: P,
    arms: &[String],
    mut run_model: R,
    mut judge: J,
    log_path: &Path,
) -> Result<OnboardSummary, CliError>
where
    P: IntoIterator<Item = S>,
    S: AsRef<str>,
    R: FnMut(&str, &str) -> Result<String, CliError>,
    J: FnMut(&str, &OnboardOutputs) -> Option<String>,
{
    if arms.len() < 2 {
        return Err(CliError::usage(
            "onboarding needs at least two arms (e.g. a local and a hosted model)",
        ));
    }

    let mut summary = OnboardSummary::default();
    for prompt in prompts {
        let prompt = prompt.as_ref();
        let outputs = run_all_arms(arms, prompt, &mut run_model)?;
        let Some(label) = judge(prompt, &outputs) else {
            summary.abstained += 1;
            continue;
        };
        if !arms.iter().any(|arm| arm == &label) {
            return Err(CliError::usage(format!(
                "judge returned an unknown arm: {label:?}"
            )));
        }
        record_label(log_path, prompt, &label).map_err(|err| CliError::config(err.to_string()))?;
        summary.judged += 1;
        *summary.label_counts.entry(label).or_default() += 1;
    }
    Ok(summary)
}

#[cfg(test)]
fn map_string_error<R>(mut run_model: R) -> impl FnMut(&str, &str) -> Result<String, CliError>
where
    R: FnMut(&str, &str) -> Result<String, String>,
{
    move |arm, prompt| run_model(arm, prompt).map_err(CliError::config)
}

fn resolve_arms<I>(
    raw: Option<&str>,
    available_arms: I,
    too_few_message: &str,
) -> Result<Vec<String>, CliError>
where
    I: IntoIterator<Item = String>,
{
    let mut arms = if let Some(raw) = raw {
        raw.split(',')
            .map(str::trim)
            .filter(|arm| !arm.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>()
    } else {
        available_arms.into_iter().collect::<Vec<_>>()
    };
    arms.truncate(2);
    if arms.len() < 2 {
        return Err(CliError::usage(too_few_message));
    }
    Ok(arms)
}

fn ensure_arms_configured(
    arms: &[String],
    models: &BTreeMap<String, GatewayModel>,
) -> Result<(), CliError> {
    let missing = arms
        .iter()
        .filter(|arm| !models.contains_key(*arm))
        .cloned()
        .collect::<Vec<_>>();
    if missing.is_empty() {
        return Ok(());
    }
    Err(CliError::usage(format!(
        "no [gateway.models] entry for: {}",
        missing.join(", ")
    )))
}

fn ensure_file(path: &Path) -> Result<(), CliError> {
    if path.is_file() {
        return Ok(());
    }
    Err(CliError::usage(format!(
        "file not found: {}",
        path.display()
    )))
}

fn load_prompts(path: &Path) -> Result<Vec<String>, CliError> {
    let text = fs::read_to_string(path)
        .map_err(|err| CliError::usage(format!("cannot read {}: {err}", path.display())))?;
    let mut prompts = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('{') {
            let row = serde_json::from_str::<JsonValue>(line).map_err(|err| {
                CliError::config(format!(
                    "{}:{}: invalid JSON: {err}",
                    path.display(),
                    index + 1
                ))
            })?;
            if let Some(text) = row.get("text").and_then(JsonValue::as_str) {
                prompts.push(text.to_owned());
                continue;
            }
        }
        prompts.push(line.to_owned());
    }
    Ok(prompts)
}

fn write_comparison_if_requested(
    path: Option<&Path>,
    prompt: &str,
    outputs: &OnboardOutputs,
    cheap: &str,
    expensive: &str,
    judge: &HeuristicJudge,
    verdict: &wayfinder_internal_core::judge::Verdict,
) -> Result<(), CliError> {
    let Some(path) = path else {
        return Ok(());
    };
    let row = json!({
        "text": prompt,
        "cheap": {
            "arm": cheap,
            "response": outputs.get(cheap).cloned().unwrap_or_default(),
        },
        "expensive": {
            "arm": expensive,
            "response": outputs.get(expensive).cloned().unwrap_or_default(),
        },
        "verdict": {
            "sufficient": verdict.sufficient,
            "comparator": verdict.comparator,
            "reason": verdict.reason,
        },
        "judge_version": judge.version(),
    });
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|err| CliError::usage(format!("cannot write {}: {err}", path.display())))?;
    writeln!(
        file,
        "{}",
        serde_json::to_string(&row).expect("comparison row should serialize")
    )
    .map_err(|err| CliError::usage(format!("cannot write {}: {err}", path.display())))
}

fn judge_provenance_banner(
    judge_version: &str,
    options: &JudgeOptions,
    sample_count: usize,
    report: &wayfinder_internal_core::sufficiency::GateReport,
) -> String {
    judge_provenance_banner_with_title(
        "trusted config",
        judge_version,
        options,
        sample_count,
        report,
    )
}

fn judge_provenance_banner_with_title(
    title: &str,
    judge_version: &str,
    options: &JudgeOptions,
    sample_count: usize,
    report: &wayfinder_internal_core::sufficiency::GateReport,
) -> String {
    format!(
        "# wayfinder-router judge: {title} (WF-ADR-0037)\n\
         # judge={} mode={} samples={}\n\
         # kappa={:.2} (floor {:.2}, gold n={}) cv_acc={:.2} baseline={:.2} lift={:+.2}\n\
         # prompts={} gold={} tool={} generated={}",
        judge_version,
        options.mode,
        sample_count,
        report.kappa,
        report.kappa_floor,
        report.n_gold,
        report.cv_accuracy,
        report.majority_baseline,
        report.lift,
        file_hash(&options.prompts),
        options
            .gold
            .as_ref()
            .map(|path| file_hash(path))
            .unwrap_or_else(|| "none".to_owned()),
        env!("CARGO_PKG_VERSION"),
        generated_timestamp(),
    )
}

fn file_hash(path: &Path) -> String {
    let Ok(bytes) = fs::read(path) else {
        return "none".to_owned();
    };
    let digest = Sha256::digest(bytes);
    format!("{digest:x}").chars().take(12).collect()
}

fn generated_timestamp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    format!("unix:{seconds}")
}

fn render_label_counts_map(counts: &BTreeMap<String, usize>) -> String {
    counts
        .iter()
        .map(|(label, count)| format!("{label}={count}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn render_human(result: &ComplexityScore, weights: Option<FeatureWeights>) -> String {
    let mut lines = vec![
        format!("Recommended Model: {}", result.recommendation),
        format!(
            "Complexity Score: {:.2}  (mode: {})",
            result.score, result.mode
        ),
    ];
    if let Some(tiers) = &result.tiers {
        lines.push(String::new());
        lines.push("Tiers:".to_owned());
        for tier in tiers {
            let marker = if tier.model == result.recommendation {
                " <-"
            } else {
                ""
            };
            lines.push(format!(
                "  >= {:.2}  {}{}",
                tier.min_score, tier.model, marker
            ));
        }
    }
    if let Some(models) = &result.models {
        lines.push(String::new());
        lines.push(format!("Candidates: {}", models.join(", ")));
    }
    if let Some(weights) = weights {
        lines.push(String::new());
        lines.push("Score Breakdown (feature: value  norm x weight = contribution):".to_owned());
        for fc in explain_score(&result.features, weights) {
            lines.push(format!(
                "  {:<18} {:>5}  {:.2} x {:<4} = {:.3}",
                fc.name, fc.value, fc.normalized, fc.weight, fc.contribution
            ));
        }
    } else {
        lines.push(String::new());
        lines.push("Contributing Features:".to_owned());
        for name in FEATURE_ORDER {
            let value = result.features.get(name).unwrap_or(0);
            lines.push(format!("  {}: {value}", feature_title(name)));
        }
    }
    lines.join("\n")
}

fn feature_title(name: &str) -> String {
    name.split('_')
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => format!("{}{}", first.to_ascii_uppercase(), chars.as_str()),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn parse_costs(raw: Option<&str>) -> Result<Option<BTreeMap<String, f64>>, CliError> {
    let Some(raw) = raw.filter(|raw| !raw.is_empty()) else {
        return Ok(None);
    };
    let mut costs = BTreeMap::new();
    for item in raw.split(',') {
        let Some((label, value)) = item.split_once('=') else {
            return Err(CliError::config(format!(
                "--costs must be label=number pairs, got {:?}",
                item
            )));
        };
        let label = label.trim();
        let value = value.trim();
        if label.is_empty() || value.is_empty() {
            return Err(CliError::config(format!(
                "--costs must be label=number pairs, got {:?}",
                item
            )));
        }
        let value = value.parse().map_err(|_| {
            CliError::config(format!("--costs value for {label:?} must be a number"))
        })?;
        costs.insert(label.to_owned(), value);
    }
    Ok(Some(costs))
}

fn parse_weights_arg(raw: Option<&str>) -> Result<Option<ParsedWeights>, CliError> {
    let Some(raw) = raw.filter(|raw| !raw.is_empty()) else {
        return Ok(None);
    };
    let mut supplied = BTreeMap::new();
    let mut weights = FeatureWeights {
        word_count: 0.0,
        heading_count: 0.0,
        max_heading_depth: 0.0,
        list_item_count: 0.0,
        link_count: 0.0,
        code_block_count: 0.0,
        table_row_count: 0.0,
        reasoning_term_count: 0.0,
        math_symbol_count: 0.0,
        constraint_term_count: 0.0,
        question_count: 0.0,
    };
    for item in raw.split(',') {
        let Some((name, value)) = item.split_once('=') else {
            return Err(CliError::config(format!(
                "--weights must be feature=number pairs, got {:?}",
                item
            )));
        };
        let name = name.trim();
        let value = value.trim();
        if name.is_empty() || value.is_empty() {
            return Err(CliError::config(format!(
                "--weights must be feature=number pairs, got {:?}",
                item
            )));
        }
        let value = value.parse().map_err(|_| {
            CliError::config(format!("--weights value for {name:?} must be a number"))
        })?;
        if !weights.set(name, value) {
            return Err(CliError::config(format!(
                "'{name}' is not a known feature (one of {})",
                FEATURE_ORDER.join(", ")
            )));
        }
        supplied.insert(name.to_owned(), value);
    }
    Ok(Some(ParsedWeights { weights, supplied }))
}

fn replace_weights_block(toml: &str, supplied: &BTreeMap<String, f64>) -> String {
    if !toml.starts_with("[routing]\nweights = {") {
        return toml.to_owned();
    }
    let Some(split) = toml.find("\n\n") else {
        return toml.to_owned();
    };
    format!(
        "{}{}",
        weights_block_from_supplied(supplied),
        &toml[split + 2..]
    )
}

fn weights_block_from_supplied(supplied: &BTreeMap<String, f64>) -> String {
    let inner = supplied
        .iter()
        .filter(|(name, value)| {
            DEFAULT_WEIGHTS
                .get(name)
                .is_some_and(|default| default != **value)
        })
        .map(|(name, value)| format!("{name} = {}", fmt_float(*value)))
        .collect::<Vec<_>>()
        .join(", ");
    if inner.is_empty() {
        String::new()
    } else {
        format!("[routing]\nweights = {{ {inner} }}\n\n")
    }
}

fn fmt_float(value: f64) -> String {
    let rounded = round_to(value, 6);
    let mut text = python_exponent_format(format!("{rounded:?}"));
    if !text.contains('.') && !text.contains('e') {
        text.push_str(".0");
    }
    text
}

fn python_exponent_format(text: String) -> String {
    let Some((mantissa, exponent)) = text.split_once('e') else {
        return text;
    };
    let exponent = exponent
        .parse::<i32>()
        .expect("Rust float debug exponent should be an integer");
    let sign = if exponent < 0 { '-' } else { '+' };
    format!("{mantissa}e{sign}{:02}", exponent.abs())
}

fn round_to(value: f64, places: i32) -> f64 {
    let factor = 10_f64.powi(places);
    let scaled = value * factor;
    let floor = scaled.floor();
    let fraction = scaled - floor;
    let rounded = if (fraction - 0.5).abs() <= 1e-12 {
        if (floor as i64) % 2 == 0 {
            floor
        } else {
            floor + 1.0
        }
    } else if fraction < 0.5 {
        floor
    } else {
        floor + 1.0
    };
    let rounded = rounded / factor;
    if rounded == 0.0 && value.is_sign_negative() {
        -0.0
    } else {
        rounded
    }
}

fn summary_bits(summary: &JsonValue) -> String {
    let JsonValue::Object(values) = summary else {
        return py_value(summary);
    };

    let mut keys = match (
        values.get("mode").and_then(JsonValue::as_str),
        values.get("objective").and_then(JsonValue::as_str),
    ) {
        (Some("threshold"), Some("knee")) => vec![
            "mode",
            "objective",
            "threshold",
            "models",
            "accuracy",
            "quality_recovered",
            "cost_savings",
            "samples",
        ],
        (Some("threshold"), Some("cost-quality")) => vec![
            "mode",
            "objective",
            "threshold",
            "models",
            "accuracy",
            "cost_savings",
            "target_savings",
            "samples",
        ],
        (Some("threshold"), _) => vec!["mode", "threshold", "models", "accuracy", "samples"],
        (Some("tiers"), _) => vec!["mode", "models", "breakpoints", "accuracy", "samples"],
        (Some("classifier"), _) => vec!["mode", "models", "iterations", "accuracy", "samples"],
        _ => Vec::new(),
    };
    for key in values.keys() {
        if !keys.contains(&key.as_str()) {
            keys.push(key);
        }
    }

    keys.into_iter()
        .filter_map(|key| {
            values
                .get(key)
                .map(|value| format!("{key}={}", py_value(value)))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn py_value(value: &JsonValue) -> String {
    match value {
        JsonValue::Null => "None".to_owned(),
        JsonValue::Bool(value) => {
            if *value {
                "True".to_owned()
            } else {
                "False".to_owned()
            }
        }
        JsonValue::Number(value) => value.to_string(),
        JsonValue::String(value) => value.clone(),
        JsonValue::Array(values) => {
            let inner = values.iter().map(py_repr).collect::<Vec<_>>().join(", ");
            format!("[{inner}]")
        }
        JsonValue::Object(_) => py_repr(value),
    }
}

fn py_repr(value: &JsonValue) -> String {
    match value {
        JsonValue::String(value) => format!("'{}'", value.replace('\'', "\\'")),
        JsonValue::Array(values) => {
            let inner = values.iter().map(py_repr).collect::<Vec<_>>().join(", ");
            format!("[{inner}]")
        }
        JsonValue::Object(values) => {
            let inner = values
                .iter()
                .map(|(key, value)| format!("'{key}': {}", py_repr(value)))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{{{inner}}}")
        }
        _ => py_value(value),
    }
}

fn next_value<I>(args: &mut I, flag: &str) -> Result<String, CliError>
where
    I: Iterator<Item = String>,
{
    args.next()
        .ok_or_else(|| CliError::new(format!("{flag} requires a value")))
}

fn parse_choice(flag: &str, value: String, choices: &[&str]) -> Result<String, CliError> {
    if choices.contains(&value.as_str()) {
        return Ok(value);
    }
    Err(CliError::usage(format!(
        "{flag} must be one of {}",
        choices.join(", ")
    )))
}

fn non_empty(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        parse, run, run_output, CalibrateOptions, CliCommand, JudgeOptions, OnboardOptions,
        RecalibrateOptions, ServeOptions, ServiceOptions, WebchatOptions,
    };
    use wayfinder_internal_ui::UiOptions;

    #[test]
    fn run_accepts_serve_shape() {
        let output = run([
            "serve",
            "--host",
            "0.0.0.0",
            "--port",
            "9000",
            "--dry-run",
            "--timeout",
            "10",
        ])
        .expect("serve args should parse");

        assert!(output.contains("serve"));
        assert!(output.contains("0.0.0.0:9000"));
        assert!(output.contains("dry-run"));
    }

    #[test]
    fn parse_service_accepts_install_print_shape() {
        let command = parse([
            "service",
            "install",
            "--host",
            "127.0.0.1",
            "--port",
            "8088",
            "--print",
        ])
        .expect("service args should parse");

        assert_eq!(
            command,
            CliCommand::Service(ServiceOptions {
                action: "install".to_owned(),
                host: "127.0.0.1".to_owned(),
                port: 8088,
                print: true,
                config: None,
            })
        );
    }

    #[test]
    fn service_install_print_emits_platform_unit() {
        let output = run_output(["service", "install", "--port", "8088", "--print"], None)
            .expect("service install --print should succeed");

        assert!(output.stderr.is_empty());
        assert!(
            output.stdout.contains("wayfinder-router")
                && output.stdout.contains("serve")
                && output.stdout.contains("8088")
        );
        assert!(
            output.stdout.starts_with("<?xml version=\"1.0\"")
                || output.stdout.starts_with("[Unit]\n")
        );
    }

    #[test]
    fn service_status_reports_unit_and_endpoint() {
        let output = run_output(
            ["service", "status", "--host", "127.0.0.1", "--port", "8088"],
            None,
        )
        .expect("service status should succeed on supported platforms");

        assert!(output.stdout.is_empty());
        assert!(output.stderr.contains("unit file:"));
        assert!(output
            .stderr
            .contains("endpoint:  http://127.0.0.1:8088/v1"));
        assert!(output.stderr.contains("health:"));
    }

    #[test]
    fn resolve_serve_args_targets_serve() {
        let args = super::resolve_serve_args("127.0.0.1", 8088, None);

        assert!(args.first().is_some_and(|arg| !arg.is_empty()));
        assert_eq!(
            &args[args.len() - 5..],
            ["serve", "--host", "127.0.0.1", "--port", "8088"]
        );
    }

    #[test]
    fn resolve_serve_args_bakes_in_an_explicit_config_path() {
        // A service manager launches the gateway from a working directory it does not
        // control, so the unit has to name the config file outright.
        let args = super::resolve_serve_args(
            "127.0.0.1",
            8088,
            Some(Path::new("/etc/wayfinder/wayfinder-router.toml")),
        );

        assert_eq!(
            &args[args.len() - 7..],
            [
                "serve",
                "--host",
                "127.0.0.1",
                "--port",
                "8088",
                "--config",
                "/etc/wayfinder/wayfinder-router.toml"
            ]
        );
    }

    #[test]
    fn parse_service_accepts_a_config_path() {
        let command = super::parse(["service", "install", "--config", "/tmp/wf.toml"])
            .expect("service args should parse");

        assert_eq!(
            command,
            CliCommand::Service(ServiceOptions {
                action: "install".to_owned(),
                config: Some(PathBuf::from("/tmp/wf.toml")),
                ..ServiceOptions::default()
            })
        );
    }

    #[test]
    fn parse_serve_accepts_a_config_path() {
        let command =
            super::parse(["serve", "--config", "/tmp/wf.toml"]).expect("serve args should parse");

        assert_eq!(
            command,
            CliCommand::Serve(ServeOptions {
                config: Some(PathBuf::from("/tmp/wf.toml")),
                ..ServeOptions::default()
            })
        );
    }

    #[test]
    fn parse_ui_accepts_host_and_port() {
        let command =
            parse(["ui", "--host", "0.0.0.0", "--port", "9001"]).expect("ui should parse");

        assert_eq!(
            command,
            CliCommand::Ui(UiOptions {
                host: "0.0.0.0".to_owned(),
                port: 9001,
            })
        );
    }

    #[test]
    fn run_ui_prints_server_summary() {
        let output = run(["ui", "--port", "9010"]).expect("ui args should parse");

        assert!(output.contains("wayfinder-router ui listening"));
        assert!(output.contains("127.0.0.1:9010"));
    }

    #[test]
    fn parse_webchat_accepts_gateway_options_and_no_open() {
        let command = parse([
            "webchat",
            "--host",
            "0.0.0.0",
            "--port",
            "9000",
            "--dry-run",
            "--timeout",
            "2.5",
            "--no-open",
        ])
        .expect("webchat should parse");

        assert_eq!(
            command,
            CliCommand::Webchat(WebchatOptions {
                host: "0.0.0.0".to_owned(),
                port: 9000,
                dry_run: true,
                timeout_seconds: Some(2.5),
                no_open: true,
            })
        );
    }

    #[test]
    fn run_webchat_dry_run_prints_demo_summary_without_starting_server() {
        let output = run(["webchat", "--port", "9000", "--dry-run"]).expect("webchat should run");

        assert!(output.contains("wayfinder-router webchat"));
        assert!(output.contains("http://127.0.0.1:9000/demo"));
        assert!(output.contains("dry-run"));
        assert!(output.contains("Ctrl-C to stop"));
    }

    #[test]
    fn demo_url_maps_wildcard_hosts_to_loopback() {
        assert_eq!(
            super::demo_url("0.0.0.0", 9000),
            "http://127.0.0.1:9000/demo"
        );
        assert_eq!(super::demo_url("::", 8088), "http://127.0.0.1:8088/demo");
        assert_eq!(
            super::demo_url("example.internal", 80),
            "http://example.internal:80/demo"
        );
    }

    #[test]
    fn top_level_help_lists_all_ported_commands() {
        let output = run(["--help"]).expect("top-level help should succeed");

        for command in [
            "route",
            "calibrate",
            "serve",
            "service",
            "chat",
            "ui",
            "webchat",
            "onboard",
            "judge",
            "recalibrate",
            "init",
            "doctor",
            "keys",
        ] {
            assert!(output.contains(command), "help missing {command}");
        }
    }

    #[test]
    fn unknown_command_lists_all_ported_commands() {
        let err = parse(["bogus"]).expect_err("unknown command should fail");

        for command in [
            "route",
            "calibrate",
            "serve",
            "service",
            "chat",
            "ui",
            "webchat",
            "onboard",
            "judge",
            "recalibrate",
            "init",
            "doctor",
            "keys",
        ] {
            assert!(err.to_string().contains(command), "error missing {command}");
        }
    }

    #[test]
    fn run_accepts_chat_shape() {
        let output = run([
            "chat",
            "--theme",
            "dark",
            "--threshold",
            "0.3",
            "--why",
            "--dry-run",
            "--no-stream",
            "--base-url",
            "http://127.0.0.1:8088",
        ])
        .expect("chat args should parse");

        assert!(output.contains("wayfinder-router chat"));
        assert!(output.contains("theme: dark"));
        assert!(output.contains("dry-run"));
    }

    #[test]
    fn run_chat_accepts_prompt_text_from_args() {
        let output = run(["chat", "--dry-run", "What", "is", "DNS?"])
            .expect("chat prompt args should parse");

        assert!(output.contains("prompt: What is DNS?"));
        assert!(output.contains("route: local"));
        assert!(output.contains("gateway: skipped"));
    }

    #[test]
    fn run_chat_accepts_prompt_text_from_stdin() {
        let output = super::run_with_input(
            ["chat", "--dry-run", "--why"],
            Some("What is DNS?\n".to_string()),
        )
        .expect("chat stdin should parse");

        assert!(output.contains("prompt: What is DNS?"));
        assert!(output.contains("why:"));
    }

    #[test]
    fn chat_help_prints_usage_and_command_summary() {
        let output = run(["chat", "--help"]).expect("chat --help should succeed");

        assert!(output.contains("usage: wayfinder-router chat"));
        assert!(output.contains("--theme"));
        assert!(output.contains("--thread-dir"));
        assert!(output.contains("commands"));
        assert!(output.contains("/why"));
    }

    #[test]
    fn chat_help_short_flag_matches_long_flag() {
        let long = run(["chat", "--help"]).expect("chat --help should succeed");
        let short = run(["chat", "-h"]).expect("chat -h should succeed");

        assert_eq!(long, short);
    }

    #[test]
    fn parse_route_accepts_prompt_and_flags() {
        let command = parse([
            "route",
            "prompt.md",
            "--threshold",
            "0.25",
            "--json",
            "--explain",
        ])
        .expect("route should parse");
        let CliCommand::Route(options) = command else {
            panic!("expected route command");
        };

        assert_eq!(options.prompt, "prompt.md");
        assert_eq!(options.threshold, Some(0.25));
        assert!(options.json);
        assert!(options.explain);
    }

    #[test]
    fn run_route_accepts_stdin() {
        let output = run_output(["route", "-", "--explain"], Some("Say hello.".to_owned()))
            .expect("route stdin should run");

        assert!(output.stdout.contains("Recommended Model: local"));
        assert!(output.stdout.contains("Score Breakdown"));
        assert!(output.stdout.contains("word_count"));
        assert!(output.stderr.is_empty());
    }

    #[test]
    fn run_route_json_uses_schema_version() {
        let output = run_output(["route", "-", "--json"], Some("Say hello.".to_owned()))
            .expect("route json should run");

        assert!(output.stdout.contains("\"schema_version\": \"3\""));
        assert!(output.stdout.contains("\"recommendation\": \"local\""));
    }

    #[test]
    fn parse_calibrate_accepts_all_flags() {
        let command = parse([
            "calibrate",
            "data.jsonl",
            "--mode",
            "classifier",
            "--models",
            "local,cloud",
            "--out",
            "wayfinder-router.toml",
            "--iterations",
            "12",
            "--l2",
            "0.2",
            "--objective",
            "accuracy",
            "--target-savings",
            "0.4",
            "--costs",
            "local=0.1,cloud=1.0",
            "--weights",
            "reasoning_term_count=5",
        ])
        .expect("calibrate should parse");

        assert_eq!(
            command,
            CliCommand::Calibrate(CalibrateOptions {
                dataset: "data.jsonl".into(),
                mode: "classifier".to_owned(),
                models: Some("local,cloud".to_owned()),
                out: Some("wayfinder-router.toml".into()),
                iterations: 12,
                l2: 0.2,
                objective: "accuracy".to_owned(),
                target_savings: Some(0.4),
                costs: Some("local=0.1,cloud=1.0".to_owned()),
                weights: Some("reasoning_term_count=5".to_owned()),
            })
        );
    }

    #[test]
    fn parse_recalibrate_accepts_flags() {
        let command = parse([
            "recalibrate",
            "--log",
            "labels.jsonl",
            "--out",
            "router.toml",
            "--mode",
            "tiers",
            "--min-labels",
            "5",
        ])
        .expect("recalibrate should parse");

        assert_eq!(
            command,
            CliCommand::Recalibrate(RecalibrateOptions {
                log: "labels.jsonl".into(),
                out: "router.toml".into(),
                mode: "tiers".to_owned(),
                min_labels: 5,
            })
        );
    }

    #[test]
    fn parse_onboard_accepts_flags() {
        let command = parse([
            "onboard",
            "prompts.jsonl",
            "--arms",
            "local,cloud",
            "--log",
            "labels.jsonl",
            "--calibrate",
            "--mode",
            "tiers",
        ])
        .expect("onboard should parse");

        assert_eq!(
            command,
            CliCommand::Onboard(OnboardOptions {
                prompts: "prompts.jsonl".into(),
                arms: Some("local,cloud".to_owned()),
                log: "labels.jsonl".into(),
                calibrate: true,
                mode: "tiers".to_owned(),
            })
        );
    }

    #[test]
    fn parse_judge_accepts_flags() {
        let command = parse([
            "judge",
            "prompts.jsonl",
            "--arms",
            "local,cloud",
            "--gold",
            "gold.jsonl",
            "--log",
            "labels.jsonl",
            "--mode",
            "classifier",
            "--kappa-floor",
            "0.75",
            "--folds",
            "3",
            "--limit",
            "4",
            "--save-comparisons",
            "comparisons.jsonl",
        ])
        .expect("judge should parse");

        assert_eq!(
            command,
            CliCommand::Judge(JudgeOptions {
                prompts: "prompts.jsonl".into(),
                arms: Some("local,cloud".to_owned()),
                gold: Some("gold.jsonl".into()),
                log: "labels.jsonl".into(),
                mode: "classifier".to_owned(),
                kappa_floor: 0.75,
                folds: 3,
                limit: Some(4),
                save_comparisons: Some("comparisons.jsonl".into()),
            })
        );
    }

    #[test]
    fn init_prints_exact_preset_config_without_writing() {
        let dir = unique_temp_dir("cli-init-print");
        for preset in ["hybrid", "openai", "gemini"] {
            let path = dir.join(format!("{preset}.toml"));
            let output = run_output(
                [
                    "init",
                    "--preset",
                    preset,
                    "--path",
                    path.to_str().expect("path is utf-8"),
                    "--print",
                ],
                None,
            )
            .expect("init --print should succeed");

            assert_eq!(
                output.stdout,
                wayfinder_internal_gateway::bootstrap::render_config(
                    &wayfinder_internal_gateway::bootstrap::PRESETS[preset],
                )
            );
            assert!(output.stderr.is_empty());
            assert!(!path.exists());
            assert!(!dir.join(".env.example").exists());
        }
    }

    #[test]
    fn init_interactive_is_explicitly_unsupported() {
        let dir = unique_temp_dir("cli-init-interactive");
        let path = dir.join("wayfinder-router.toml");
        let err = run_output(
            [
                "init",
                "--interactive",
                "--print",
                "--path",
                path.to_str().expect("path is utf-8"),
            ],
            None,
        )
        .expect_err("interactive init should not be ignored");

        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("interactive init"));
        assert!(!path.exists());
    }

    #[test]
    fn init_writes_config_and_env_example_next_to_path() {
        let dir = unique_temp_dir("cli-init-write");
        let path = dir.join("nested").join("router.toml");
        fs::create_dir_all(path.parent().expect("path has a parent")).expect("dir write");

        let output = run_output(
            [
                "init",
                "--preset",
                "openai",
                "--path",
                path.to_str().expect("path is utf-8"),
            ],
            None,
        )
        .expect("init should write files");

        assert!(output.stdout.contains("wrote"));
        assert!(output.stdout.contains("OPENAI_API_KEY"));
        assert!(path.is_file());
        assert!(fs::read_to_string(&path)
            .expect("config should read")
            .contains("gpt-4o-mini"));
        assert!(
            fs::read_to_string(path.parent().unwrap().join(".env.example"))
                .expect("env example should read")
                .contains("OPENAI_API_KEY=")
        );
    }

    #[test]
    fn init_refuses_to_clobber_without_force_and_force_overwrites() {
        let dir = unique_temp_dir("cli-init-force");
        let path = dir.join("wayfinder-router.toml");
        fs::write(&path, "# mine\n").expect("config write");

        let err = run_output(
            ["init", "--path", path.to_str().expect("path is utf-8")],
            None,
        )
        .expect_err("init should refuse existing file");
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("already exists"));
        assert_eq!(
            fs::read_to_string(&path).expect("config should read"),
            "# mine\n"
        );

        run_output(
            [
                "init",
                "--path",
                path.to_str().expect("path is utf-8"),
                "--force",
            ],
            None,
        )
        .expect("force should overwrite");
        assert!(fs::read_to_string(&path)
            .expect("config should read")
            .contains("[gateway.models.cloud]"));
    }

    #[test]
    fn doctor_reports_missing_and_ready_key_status() {
        let dir = unique_temp_dir("cli-doctor");
        let config = dir.join("wayfinder-router.toml");
        let key = format!("WAYFINDER_CLI_DOCTOR_TEST_{}", std::process::id());
        fs::write(
            &config,
            format!(
                r#"[routing]
threshold = 0.08

[gateway.models.local]
base_url = "http://localhost:11434/v1"
model = "llama3.1"

[gateway.models.cloud]
base_url = "https://api.example.test/v1"
model = "hosted"
api_key_env = "{key}"
"#
            ),
        )
        .expect("config write");

        std::env::remove_var(&key);
        let err = run_output(
            ["doctor", "--dir", dir.to_str().expect("path is utf-8")],
            None,
        )
        .expect_err("missing key should return config error");
        assert_eq!(err.exit_code(), 1);
        assert!(err.to_string().contains("config:"));
        assert!(err.to_string().contains("local"));
        assert!(err.to_string().contains("keyless"));
        assert!(err.to_string().contains("cloud"));
        assert!(err.to_string().contains("not ready"));
        assert!(err.to_string().contains(&key));

        std::env::set_var(&key, "sk-test");
        let output = run_output(
            ["doctor", "--dir", dir.to_str().expect("path is utf-8")],
            None,
        )
        .expect("set key should be ready");
        std::env::remove_var(&key);
        assert!(output.stdout.contains("ready:"));
        assert!(output.stdout.contains("keyless"));
        assert!(output.stdout.contains("set"));
        assert!(output.stderr.is_empty());
    }

    #[test]
    fn doctor_without_config_is_usage_error() {
        let dir = unique_temp_dir("cli-doctor-missing");
        let err = run_output(
            ["doctor", "--dir", dir.to_str().expect("path is utf-8")],
            None,
        )
        .expect_err("missing config should be a usage error");

        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("no wayfinder-router.toml"));
    }

    #[test]
    fn keys_new_mints_pasteable_block() {
        let output = run_output(["keys", "new", "--id", "team-a", "--tag", "prod"], None)
            .expect("keys new should succeed");
        let key = output
            .stdout
            .lines()
            .find(|line| line.starts_with("wf-"))
            .expect("plaintext key should be printed");
        let hash = output
            .stdout
            .lines()
            .find_map(|line| line.strip_prefix("hash = \""))
            .and_then(|line| line.strip_suffix('"'))
            .expect("hash line should be printed");

        assert!(output.stdout.contains("[gateway.keys.team-a]"));
        assert!(output.stdout.contains("tags = [\"prod\"]"));
        assert!(wayfinder_internal_core::vkeys::verify(key, hash));
        assert!(output.stderr.is_empty());
    }

    #[test]
    fn keys_new_escapes_toml_special_id_and_tags() {
        let output = run_output(
            [
                "keys",
                "new",
                "--id",
                "we\"ird.id",
                "--tag",
                "a\"b",
                "--tag",
                "ok",
            ],
            None,
        )
        .expect("keys new should succeed");
        let block = output
            .stdout
            .split("# Give this key")
            .next()
            .expect("config block should be present");
        wayfinder_internal_gateway::validate_gateway_toml(block, "keys-new")
            .expect("generated key block should be valid gateway TOML");

        assert!(block.contains("[gateway.keys.\"we\\\"ird.id\"]"));
        assert!(block.contains("tags = [\"a\\\"b\", \"ok\"]"));
    }

    #[test]
    fn onboard_with_stub_invoker_records_labels_and_calibrates() {
        let dir = unique_temp_dir("cli-onboard");
        let prompts = dir.join("prompts.jsonl");
        let log = dir.join("labels.jsonl");
        fs::write(
            &prompts,
            "{\"text\":\"What is DNS?\"}\nExplain an impossible distributed systems proof.\n",
        )
        .expect("prompts should write");

        let output = super::execute_onboard_with_invoker(
            OnboardOptions {
                prompts,
                arms: Some("local,cloud".to_owned()),
                log: log.clone(),
                calibrate: true,
                mode: "threshold".to_owned(),
            },
            ["local".to_owned(), "cloud".to_owned()],
            |arm, prompt| {
                if arm == "local" && prompt.contains("impossible") {
                    "I cannot help with that.".to_owned()
                } else {
                    format!("{arm} answered {prompt}")
                }
            },
            |_, outputs| {
                if outputs["local"].contains("cannot help") {
                    Some("cloud".to_owned())
                } else {
                    Some("local".to_owned())
                }
            },
        )
        .expect("onboard should run");

        let labels = fs::read_to_string(&log).expect("labels should be written");
        assert!(labels.contains("\"label\": \"local\""));
        assert!(labels.contains("\"label\": \"cloud\""));
        assert!(output.stdout.contains("[[routing.tiers]]"));
        assert!(output.stderr.contains("wayfinder-router: judged 2 prompts"));
    }

    #[test]
    fn judge_refuses_failed_gold_gate_and_saves_comparisons_only_when_requested() {
        let dir = unique_temp_dir("cli-judge-refuse");
        let prompts = dir.join("prompts.txt");
        let gold = dir.join("gold.jsonl");
        let log = dir.join("labels.jsonl");
        let comparisons = dir.join("comparisons.jsonl");
        fs::write(&prompts, "short prompt\nanother short prompt\n").expect("prompts write");
        fs::write(
            &gold,
            "{\"text\":\"gold one\",\"label\":\"cloud\"}\n{\"text\":\"gold two\",\"label\":\"cloud\"}\n",
        )
        .expect("gold write");

        let err = super::execute_judge_with_invoker(
            JudgeOptions {
                prompts,
                arms: Some("local,cloud".to_owned()),
                gold: Some(gold),
                log,
                mode: "threshold".to_owned(),
                kappa_floor: 0.6,
                folds: 2,
                limit: Some(1),
                save_comparisons: Some(comparisons.clone()),
            },
            ["local".to_owned(), "cloud".to_owned()],
            |_, _| "same sufficient answer with enough detail".to_owned(),
        )
        .expect_err("failed gate should refuse config");

        assert_eq!(err.exit_code(), 1);
        let message = err.to_string();
        assert!(message.contains("confusion (rows=judge, cols=gold):"));
        assert!(message.contains("trust gates: REFUSED"));
        assert!(message.contains("refusing to emit a config"));
        let labels = fs::read_to_string(dir.join("labels.jsonl")).expect("labels written");
        assert_eq!(labels.lines().count(), 1);
        assert!(comparisons.is_file());
        assert_eq!(
            fs::read_to_string(comparisons)
                .expect("comparisons written")
                .lines()
                .count(),
            3
        );
    }

    #[test]
    fn judge_emits_config_when_gates_pass() {
        let dir = unique_temp_dir("cli-judge-pass");
        let prompts = dir.join("prompts.txt");
        let gold = dir.join("gold.jsonl");
        let log = dir.join("labels.jsonl");
        fs::write(
            &prompts,
            "Hi.\nThanks.\nAnalyze the distributed systems proof with constraints, theorem details, and tradeoffs.\nCompare the architecture options with reasoning, constraints, and a migration plan.\n",
        )
        .expect("prompts write");
        fs::write(
            &gold,
            "{\"text\":\"gold cheap\",\"label\":\"local\"}\n{\"text\":\"gold expensive\",\"label\":\"cloud\"}\n",
        )
        .expect("gold write");

        let output = super::execute_judge_with_invoker(
            JudgeOptions {
                prompts,
                arms: Some("local,cloud".to_owned()),
                gold: Some(gold),
                log,
                mode: "threshold".to_owned(),
                kappa_floor: 0.6,
                folds: 2,
                limit: None,
                save_comparisons: None,
            },
            ["local".to_owned(), "cloud".to_owned()],
            |arm, prompt| {
                if arm == "local"
                    && (prompt.contains("Analyze")
                        || prompt.contains("Compare")
                        || prompt.contains("expensive"))
                {
                    "I cannot help with that.".to_owned()
                } else {
                    "complete sufficient answer with enough detail".to_owned()
                }
            },
        )
        .expect("passing gates should emit config");

        assert!(output
            .stdout
            .contains("# wayfinder-router judge: trusted config"));
        assert!(output.stdout.contains("[[routing.tiers]]"));
        assert!(output.stderr.contains("trust gates: PASS"));
        assert!(!dir.join("comparisons.jsonl").exists());
    }

    #[test]
    fn onboard_stops_when_model_invoker_fails_without_recording_labels() {
        let dir = unique_temp_dir("cli-onboard-fail");
        let prompts = dir.join("prompts.txt");
        let log = dir.join("labels.jsonl");
        fs::write(&prompts, "What is DNS?\n").expect("prompts write");

        let err = super::execute_onboard_with_invoker_result(
            OnboardOptions {
                prompts,
                arms: Some("local,cloud".to_owned()),
                log: log.clone(),
                calibrate: true,
                mode: "threshold".to_owned(),
            },
            ["local".to_owned(), "cloud".to_owned()],
            |arm, _| {
                if arm == "local" {
                    Err("local upstream unavailable".to_owned())
                } else {
                    Ok("cloud answer".to_owned())
                }
            },
            |_, _| Some("local".to_owned()),
        )
        .expect_err("model failure should stop onboarding");

        assert_eq!(err.exit_code(), 1);
        assert!(err.to_string().contains("local upstream unavailable"));
        assert!(!log.exists());
    }

    #[test]
    fn judge_returns_comparison_write_error_without_recording_labels() {
        let dir = unique_temp_dir("cli-judge-comparison-fail");
        let prompts = dir.join("prompts.txt");
        let gold = dir.join("gold.jsonl");
        let log = dir.join("labels.jsonl");
        let comparisons = dir.join("missing").join("comparisons.jsonl");
        fs::write(&prompts, "short prompt\n").expect("prompts write");
        fs::write(&gold, "{\"text\":\"gold cheap\",\"label\":\"local\"}\n").expect("gold write");

        let err = super::execute_judge_with_invoker(
            JudgeOptions {
                prompts,
                arms: Some("local,cloud".to_owned()),
                gold: Some(gold),
                log: log.clone(),
                mode: "threshold".to_owned(),
                kappa_floor: 0.0,
                folds: 2,
                limit: Some(1),
                save_comparisons: Some(comparisons.clone()),
            },
            ["local".to_owned(), "cloud".to_owned()],
            |_, _| "same sufficient answer with enough detail".to_owned(),
        )
        .expect_err("comparison write failure should stop judging");

        assert_eq!(err.exit_code(), 2);
        assert!(err
            .to_string()
            .contains(&format!("cannot write {}", comparisons.display())));
        assert!(!log.exists());
    }

    #[test]
    fn parse_calibrate_rejects_unknown_mode_as_usage_error() {
        let err = parse(["calibrate", "data.jsonl", "--mode", "bogus"])
            .expect_err("unknown calibrate mode should be a usage error");

        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("--mode"));
    }

    #[test]
    fn parse_calibrate_rejects_unknown_objective_as_usage_error() {
        let err = parse(["calibrate", "data.jsonl", "--objective", "bogus"])
            .expect_err("unknown calibrate objective should be a usage error");

        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("--objective"));
    }

    #[test]
    fn parse_recalibrate_rejects_unknown_mode_as_usage_error() {
        let err = parse(["recalibrate", "--mode", "bogus"])
            .expect_err("unknown recalibrate mode should be a usage error");

        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("--mode"));
    }

    fn unique_temp_dir(prefix: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("{prefix}-{nanos}"));
        fs::create_dir_all(&dir).expect("temp dir should be created");
        dir
    }
}
