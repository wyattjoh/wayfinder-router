use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::fs;
use std::path::PathBuf;

use serde_json::Value as JsonValue;
use wayfinder_internal_core::calibrate::{
    calibrate, load_dataset, CalibrationOptions, CalibrationResult,
};
use wayfinder_internal_core::complexity::{
    binary_tiers, explain_score, score_complexity, ComplexityScore, FeatureWeights, RoutingConfig,
    DEFAULT_WEIGHTS, FEATURE_ORDER,
};
use wayfinder_internal_core::config::load_routing_config;
use wayfinder_internal_gateway::recalibrate::{recalibrate, DEFAULT_MIN_LABELS};
use wayfinder_internal_gateway::{serve_summary, ServeOptions};
use wayfinder_internal_tui::{run_chat, ChatOptions, HELP};

const EXIT_CONFIG: i32 = 1;
const EXIT_USAGE: i32 = 2;

#[derive(Debug, PartialEq, Eq)]
pub struct CliError {
    message: String,
    exit_code: i32,
}

impl CliError {
    fn new(message: impl Into<String>) -> Self {
        Self::usage(message)
    }

    fn usage(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            exit_code: EXIT_USAGE,
        }
    }

    fn config(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            exit_code: EXIT_CONFIG,
        }
    }

    pub fn exit_code(&self) -> i32 {
        self.exit_code
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
    Chat(ChatOptions),
    Route(RouteOptions),
    Calibrate(CalibrateOptions),
    Recalibrate(RecalibrateOptions),
    Help(String),
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
        Some("serve") => Ok(CliCommand::Serve(parse_serve(args)?)),
        Some("chat") => match parse_chat(args)? {
            None => Ok(CliCommand::Help(chat_help())),
            Some(mut options) => {
                if options.input.is_none() {
                    options.input = stdin.and_then(non_empty);
                }
                Ok(CliCommand::Chat(options))
            }
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
        Some(command) => Err(CliError::new(format!(
            "unknown command '{command}' (expected 'serve', 'chat', 'route', 'calibrate', or 'recalibrate')"
        ))),
        None => Err(CliError::new(
            "expected command: serve, chat, route, calibrate, or recalibrate",
        )),
    }
}

pub fn execute(command: CliCommand) -> Result<CommandOutput, CliError> {
    match command {
        CliCommand::Serve(options) => Ok(CommandOutput {
            stdout: serve_summary(&options),
            stderr: String::new(),
        }),
        CliCommand::Chat(options) => Ok(CommandOutput {
            stdout: run_chat(&options).map_err(|err| CliError::config(err.to_string()))?,
            stderr: String::new(),
        }),
        CliCommand::Route(options) => execute_route(options),
        CliCommand::Calibrate(options) => execute_calibrate(options),
        CliCommand::Recalibrate(options) => execute_recalibrate(options),
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
            other => return Err(CliError::new(format!("unknown serve option '{other}'"))),
        }
    }
    Ok(options)
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
            "--mode" => options.mode = next_value(&mut args, "--mode")?,
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
            "--objective" => options.objective = next_value(&mut args, "--objective")?,
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
            "--mode" => options.mode = next_value(&mut args, "--mode")?,
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
    use super::{parse, run, run_output, CalibrateOptions, CliCommand, RecalibrateOptions};

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
}
