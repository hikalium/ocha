use clap::{Parser, Subcommand, ValueEnum};
use rand::RngExt;
use serde::{Deserialize, Serialize};
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::AsyncReadExt;
use tokio::time::{Duration, timeout};

mod backend;
mod serve;
mod turn;
use backend::claude::ClaudeBackend;
use backend::claude_cli::ClaudeCliBackend;
use backend::mock::MockBackend;
use backend::ollama::OllamaBackend;
use backend::{Backend, Message, Role, Session};
use turn::{AutoApprover, CommandApprover, Decision, StdoutObserver, TurnObserver};

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "kebab-case")]
enum BackendKind {
    Ollama,
    Claude,
    /// Shell out to the locally installed, already-authenticated `claude`
    /// (Claude Code) CLI — no ANTHROPIC_API_KEY needed.
    ClaudeCli,
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Which LLM backend to talk to
    #[arg(long, value_enum, default_value_t = BackendKind::Ollama)]
    backend: BackendKind,

    /// IP address of the Ollama server (ollama backend only)
    #[arg(short = 's', long, default_value = "127.0.0.1")]
    server: String,

    /// Port of the Ollama server (ollama backend only)
    #[arg(short = 'p', long, default_value = "11434")]
    port: u16,

    /// Override the API base URL (defaults: ollama http://<server>:<port>,
    /// claude https://api.anthropic.com)
    #[arg(long)]
    api_base: Option<String>,

    /// Model to use (defaults: ollama gemma3:27b, claude claude-sonnet-4-6)
    #[arg(short = 'm', long)]
    model: Option<String>,

    /// Max tokens to generate (claude backend only)
    #[arg(long, default_value = "4096")]
    max_tokens: u32,

    /// Optional system prompt sent out of band
    #[arg(long)]
    system: Option<String>,

    /// Path to a session file for persistent context
    #[arg(short = 'S', long)]
    session: Option<PathBuf>,

    /// Path to a reminders JSON file
    #[arg(short = 'r', long)]
    reminders: Option<PathBuf>,

    /// Max number of command executions per user response
    #[arg(long, default_value = "5")]
    command_per_response: usize,

    /// Path to a log file to record interactions
    #[arg(long)]
    log: Option<PathBuf>,

    /// List available models on the selected backend
    #[arg(long)]
    list_models: bool,

    /// The prompt to send to the model. If omitted, starts interactive mode.
    prompt: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Run the local web-UI / remote-control server (localhost only).
    /// Top-level options (--backend, -m, --system, …) become the
    /// defaults for new sessions; the API can override per session.
    Serve {
        /// Port to bind on 127.0.0.1 (0 = OS-assigned).
        #[arg(long, default_value = "8765")]
        port: u16,
    },
}

#[derive(Serialize, Deserialize)]
struct LogEntry {
    timestamp: String,
    entity: String,
    content: serde_json::Value,
}

fn append_to_log(path: &std::path::Path, entity: &str, content: impl Serialize) {
    let entry = LogEntry {
        timestamp: chrono::Local::now().to_rfc3339(),
        entity: entity.to_string(),
        content: serde_json::to_value(content).unwrap_or(serde_json::Value::Null),
    };

    let mut logs: Vec<LogEntry> = if path.exists() {
        let file = std::fs::File::open(path).ok();
        file.and_then(|f| serde_json::from_reader(f).ok())
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    logs.push(entry);

    if let Ok(file) = std::fs::File::create(path) {
        let _ = serde_json::to_writer_pretty(file, &logs);
    }
}

#[derive(Deserialize, Debug, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum Timing {
    Pre,
    Post,
}

#[derive(Deserialize, Debug)]
struct Reminder {
    probability: f32,
    prompt: String,
    timing: Timing,
    #[serde(default)]
    init: bool,
}

#[derive(Deserialize, Debug)]
struct CommandRequest {
    timeout: u64,
    binary: String,
    args: Vec<String>,
    #[allow(dead_code)]
    description: String,
}

#[derive(Serialize, Debug)]
struct CommandResult {
    status: Option<i32>,
    stdout: String,
    stderr: String,
    remaining_commands: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn apply_reminders(
    prompt: &str,
    reminders: &[Reminder],
    is_new_session: bool,
) -> (String, Vec<String>) {
    let mut rng = rand::rng();
    let mut pre = String::new();
    let mut post = String::new();
    let mut activated = Vec::new();

    for reminder in reminders {
        let should_apply = if reminder.init {
            is_new_session
        } else {
            rng.random_range(0.0..1.0) < reminder.probability
        };

        if should_apply {
            activated.push(reminder.prompt.clone());
            match reminder.timing {
                Timing::Pre => pre.push_str(&reminder.prompt),
                Timing::Post => post.push_str(&reminder.prompt),
            }
        }
    }

    (format!("{}{}{}", pre, prompt, post), activated)
}

async fn execute_command(req: CommandRequest) -> (Option<i32>, String, String, Option<String>) {
    let mut cmd = tokio::process::Command::new(&req.binary);
    cmd.args(&req.args);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    match cmd.spawn() {
        Ok(mut child) => match timeout(Duration::from_secs(req.timeout), child.wait()).await {
            Ok(Ok(status)) => {
                let mut stdout = String::new();
                let mut stderr = String::new();
                if let Some(mut out) = child.stdout.take() {
                    let _ = out.read_to_string(&mut stdout).await;
                }
                if let Some(mut err) = child.stderr.take() {
                    let _ = err.read_to_string(&mut stderr).await;
                }
                (status.code(), stdout, stderr, None)
            }
            Ok(Err(e)) => (
                None,
                String::new(),
                String::new(),
                Some(format!("Execution failed: {}", e)),
            ),
            Err(_) => {
                let _ = child.kill().await;
                (
                    None,
                    String::new(),
                    String::new(),
                    Some("Execution timed out".to_string()),
                )
            }
        },
        Err(e) => (
            None,
            String::new(),
            String::new(),
            Some(format!("Failed to spawn: {}", e)),
        ),
    }
}

fn is_command_line(line: &str) -> bool {
    line.trim_start().starts_with("!!!OCHA_RUN_CMD")
}

fn extract_command(response: &str) -> Option<&str> {
    response.lines().find(|l| is_command_line(l)).map(|l| {
        let payload = l.trim_start().strip_prefix("!!!OCHA_RUN_CMD").unwrap();
        if let Some(last_brace) = payload.rfind('}') {
            &payload[..=last_brace]
        } else {
            payload
        }
    })
}

struct RunTurnConfig<'a> {
    backend: &'a dyn Backend,
    system: Option<&'a str>,
    initial_prompt: &'a str,
    session: &'a mut Session,
    reminders: &'a [Reminder],
    command_per_response: usize,
    is_new_session: bool,
    log_path: Option<&'a std::path::Path>,
    observer: &'a dyn TurnObserver,
    approver: &'a dyn CommandApprover,
}

async fn run_turn(config: RunTurnConfig<'_>) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(path) = config.log_path {
        append_to_log(path, "user", config.initial_prompt);
    }

    config
        .session
        .messages
        .push(Message::new(Role::User, config.initial_prompt));

    let mut current_input = config.initial_prompt.to_string();
    let mut remaining_commands = config.command_per_response;
    let mut is_first_turn = config.is_new_session;

    loop {
        let (prompted_input, activated_reminders) =
            apply_reminders(&current_input, config.reminders, is_first_turn);
        is_first_turn = false;

        for reminder in activated_reminders {
            config.observer.reminder(reminder.trim());
        }

        // Reminders are ephemeral nudges: send them to the model but keep
        // the persisted history clean by swapping only the outgoing copy
        // of the latest turn.
        let mut outgoing = config.session.messages.clone();
        if let Some(last) = outgoing.last_mut() {
            last.content = prompted_input;
        }

        let mut sink = |frag: &str| config.observer.token(frag);
        let response = config
            .backend
            .chat(config.system, &outgoing, &mut sink)
            .await?;
        config.observer.response_end();

        if let (Some(path), false) = (config.log_path, response.trim().is_empty()) {
            append_to_log(path, "llm", response.trim());
        }

        config
            .session
            .messages
            .push(Message::new(Role::Assistant, response.clone()));

        let Some(cmd_json_str) = extract_command(&response) else {
            break; // plain text response: turn complete
        };

        if remaining_commands == 0 {
            let error_result = CommandResult {
                status: None,
                stdout: String::new(),
                stderr: String::new(),
                remaining_commands: 0,
                error: Some("Command execution limit exceeded per response.".to_string()),
            };
            let result_json = serde_json::to_string(&error_result)
                .unwrap_or_else(|_| "{\"error\": \"Failed to serialize limit error\"}".to_string());
            if let Some(path) = config.log_path {
                append_to_log(path, "tool", &error_result);
            }
            config
                .session
                .messages
                .push(Message::new(Role::Tool, result_json.clone()));
            current_input = result_json;
            continue;
        }

        remaining_commands -= 1;

        let result_json = match serde_json::from_str::<CommandRequest>(cmd_json_str) {
            Ok(req) => match config.approver.decide(&req).await {
                Decision::Approve => {
                    config.observer.command_payload(cmd_json_str);
                    config.observer.command_executing(&req.binary, &req.args);
                    let (status, stdout, stderr, error) = execute_command(req).await;
                    if !stdout.is_empty() {
                        config.observer.command_stdout(&stdout);
                    }
                    if !stderr.is_empty() {
                        config.observer.command_stderr(&stderr);
                    }
                    let result = CommandResult {
                        status,
                        stdout,
                        stderr,
                        remaining_commands,
                        error,
                    };
                    let result_json = serde_json::to_string(&result).unwrap_or_else(|_| {
                        "{\"error\": \"Failed to serialize command result\"}".to_string()
                    });
                    config.observer.command_result(&result_json);
                    if let Some(path) = config.log_path {
                        append_to_log(path, "tool", &result);
                    }
                    result_json
                }
                // Dormant for the CLI (AutoApprover never denies); the
                // remote serve gate uses this path. Denied commands are
                // fed back to the model exactly like other tool errors.
                Decision::Deny { reason } => {
                    let denied = CommandResult {
                        status: None,
                        stdout: String::new(),
                        stderr: String::new(),
                        remaining_commands,
                        error: Some(format!("Command denied by operator: {}", reason)),
                    };
                    if let Some(path) = config.log_path {
                        append_to_log(path, "tool", &denied);
                    }
                    serde_json::to_string(&denied).unwrap_or_else(|_| {
                        "{\"error\": \"Failed to serialize deny result\"}".to_string()
                    })
                }
            },
            Err(e) => {
                let error_result = CommandResult {
                    status: None,
                    stdout: String::new(),
                    stderr: String::new(),
                    remaining_commands,
                    error: Some(format!("Failed to parse command request: {}", e)),
                };
                if let Some(path) = config.log_path {
                    append_to_log(path, "tool", &error_result);
                }
                serde_json::to_string(&error_result).unwrap_or_else(|_| {
                    "{\"error\": \"Failed to serialize parse error\"}".to_string()
                })
            }
        };

        config
            .session
            .messages
            .push(Message::new(Role::Tool, result_json.clone()));
        current_input = result_json;
    }

    Ok(())
}

/// Everything needed to construct a backend, decoupled from CLI parsing
/// so the future `serve` mode can build one per session from an API
/// request rather than from `Args`.
#[derive(Clone)]
struct BackendConfig {
    backend: BackendKind,
    server: String,
    port: u16,
    api_base: Option<String>,
    model: Option<String>,
    max_tokens: u32,
}

impl BackendConfig {
    fn from_args(args: &Args) -> Self {
        Self {
            backend: args.backend,
            server: args.server.clone(),
            port: args.port,
            api_base: args.api_base.clone(),
            model: args.model.clone(),
            max_tokens: args.max_tokens,
        }
    }
}

fn build_backend(
    cfg: &BackendConfig,
    client: reqwest::Client,
) -> Result<Box<dyn Backend>, Box<dyn std::error::Error>> {
    // Test-only hermetic backend, reachable solely via the env switch
    // (never the CLI surface). Lets `ocha serve` integration tests run
    // without a real model or network.
    if std::env::var("OCHA_MOCK_BACKEND").as_deref() == Ok("1") {
        return Ok(Box::new(MockBackend::default()));
    }
    match cfg.backend {
        BackendKind::Ollama => {
            let base = cfg
                .api_base
                .clone()
                .unwrap_or_else(|| format!("http://{}:{}", cfg.server, cfg.port));
            let model = cfg
                .model
                .clone()
                .unwrap_or_else(|| "gemma3:27b".to_string());
            Ok(Box::new(OllamaBackend::new(client, base, model)))
        }
        BackendKind::Claude => {
            let base = cfg
                .api_base
                .clone()
                .unwrap_or_else(|| "https://api.anthropic.com".to_string());
            let api_key = std::env::var("ANTHROPIC_API_KEY").map_err(
                |_| "ANTHROPIC_API_KEY environment variable is required for the claude backend",
            )?;
            let model = cfg
                .model
                .clone()
                .unwrap_or_else(|| "claude-sonnet-4-6".to_string());
            Ok(Box::new(ClaudeBackend::new(
                client,
                base,
                api_key,
                model,
                cfg.max_tokens,
            )))
        }
        BackendKind::ClaudeCli => {
            // Uses the CLI's own login (OAuth/subscription); no API key.
            // Binary is overridable for non-standard installs / tests.
            let binary = std::env::var("OCHA_CLAUDE_CLI").unwrap_or_else(|_| "claude".to_string());
            Ok(Box::new(ClaudeCliBackend::new(binary, cfg.model.clone())))
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // Serve mode is its own thing (mutually exclusive with the REPL):
    // top-level flags become per-session defaults, then we hand off to
    // the HTTP server and never return.
    if let Some(Command::Serve { port }) = args.command {
        let reminders: Vec<Reminder> = if let Some(ref path) = args.reminders {
            serde_json::from_str(&std::fs::read_to_string(path)?)?
        } else {
            Vec::new()
        };
        let defaults = serve::ServeDefaults {
            backend_cfg: BackendConfig::from_args(&args),
            system: args.system.clone(),
            command_per_response: args.command_per_response,
            reminders,
        };
        return serve::run(defaults, port).await;
    }

    let client = reqwest::Client::new();
    let backend = build_backend(&BackendConfig::from_args(&args), client)?;

    if args.list_models {
        let models = backend.list_models().await?;
        println!("{:<40} {:<30}", "NAME", "DETAIL");
        println!("{}", "-".repeat(70));
        for m in models {
            println!("{:<40} {:<30}", m.name, m.detail);
        }
        return Ok(());
    }

    let mut session: Session = if let Some(ref path) = args.session {
        if path.exists() {
            let content = std::fs::read_to_string(path)?;
            serde_json::from_str(&content).unwrap_or_default()
        } else {
            Session::default()
        }
    } else {
        Session::default()
    };

    let is_new_session_initial = session.messages.is_empty();
    let system = args.system.as_deref();

    let reminders: Vec<Reminder> = if let Some(ref path) = args.reminders {
        let content = std::fs::read_to_string(path)?;
        serde_json::from_str(&content)?
    } else {
        Vec::new()
    };

    let save_session = |session: &Session| -> Result<(), Box<dyn std::error::Error>> {
        if let Some(ref path) = args.session {
            std::fs::write(path, serde_json::to_string_pretty(session)?)?;
        }
        Ok(())
    };

    // CLI seam policies: stdout output, auto-approve every command
    // (unchanged historical behavior). `serve` will swap these.
    let observer = StdoutObserver;
    let approver = AutoApprover;

    if let Some(prompt) = args.prompt.clone() {
        run_turn(RunTurnConfig {
            backend: backend.as_ref(),
            system,
            initial_prompt: &prompt,
            session: &mut session,
            reminders: &reminders,
            command_per_response: args.command_per_response,
            is_new_session: is_new_session_initial,
            log_path: args.log.as_deref(),
            observer: &observer,
            approver: &approver,
        })
        .await?;
        save_session(&session)?;
    } else {
        println!("Entering interactive mode. Type 'exit' or use Ctrl+C to quit.");
        let mut first_turn = true;
        loop {
            print!(">>> ");
            io::stdout().flush()?;

            let mut input = String::new();
            if io::stdin().read_line(&mut input)? == 0 {
                break;
            }

            let input = input.trim();
            if input.is_empty() {
                continue;
            }
            if input == "exit" || input == "quit" {
                break;
            }

            let is_new = if first_turn {
                is_new_session_initial
            } else {
                false
            };

            run_turn(RunTurnConfig {
                backend: backend.as_ref(),
                system,
                initial_prompt: input,
                session: &mut session,
                reminders: &reminders,
                command_per_response: args.command_per_response,
                is_new_session: is_new,
                log_path: args.log.as_deref(),
                observer: &observer,
                approver: &approver,
            })
            .await?;

            first_turn = false;
            save_session(&session)?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_command_line() {
        assert!(is_command_line("!!!OCHA_RUN_CMD{}"));
        assert!(is_command_line("  !!!OCHA_RUN_CMD{}"));
        assert!(is_command_line("\t!!!OCHA_RUN_CMD{}"));
        assert!(!is_command_line("Not a command"));
        assert!(!is_command_line("Check this: !!!OCHA_RUN_CMD{}"));
    }

    #[test]
    fn test_extract_command() {
        let response = "Some text\n!!!OCHA_RUN_CMD{\"binary\": \"ls\"}\nMore text";
        assert_eq!(extract_command(response), Some("{\"binary\": \"ls\"}"));

        let response = "!!!OCHA_RUN_CMD{\"binary\": \"whoami\"}";
        assert_eq!(extract_command(response), Some("{\"binary\": \"whoami\"}"));

        let response = "  !!!OCHA_RUN_CMD{\"binary\": \"pwd\"}";
        assert_eq!(extract_command(response), Some("{\"binary\": \"pwd\"}"));

        let response = "No command here";
        assert_eq!(extract_command(response), None);

        let response = "Text\n  !!!OCHA_RUN_CMD{\"a\": 1}\nEnding";
        assert_eq!(extract_command(response), Some("{\"a\": 1}"));

        let response = "!!!OCHA_RUN_CMD{\"binary\": \"ls\"} some trailing text";
        assert_eq!(extract_command(response), Some("{\"binary\": \"ls\"}"));
    }

    #[test]
    fn test_apply_reminders() {
        let reminders = vec![
            Reminder {
                probability: 1.0,
                prompt: "[PRE]".to_string(),
                timing: Timing::Pre,
                init: false,
            },
            Reminder {
                probability: 1.0,
                prompt: "[POST]".to_string(),
                timing: Timing::Post,
                init: false,
            },
        ];

        let (result, activated) = apply_reminders("PROMPT", &reminders, false);
        assert_eq!(result, "[PRE]PROMPT[POST]");
        assert_eq!(activated.len(), 2);
        assert!(activated.contains(&"[PRE]".to_string()));
        assert!(activated.contains(&"[POST]".to_string()));

        let reminders_init = vec![Reminder {
            probability: 1.0,
            prompt: "[INIT]".to_string(),
            timing: Timing::Pre,
            init: true,
        }];
        let (res_init, act_init) = apply_reminders("P", &reminders_init, true);
        assert_eq!(res_init, "[INIT]P");
        assert_eq!(act_init.len(), 1);

        let (res_no_init, act_no_init) = apply_reminders("P", &reminders_init, false);
        assert_eq!(res_no_init, "P");
        assert_eq!(act_no_init.len(), 0);

        let reminders_init_zero = vec![Reminder {
            probability: 0.0,
            prompt: "[INIT_ZERO]".to_string(),
            timing: Timing::Pre,
            init: true,
        }];
        let (res_zero, act_zero) = apply_reminders("P", &reminders_init_zero, true);
        assert_eq!(res_zero, "[INIT_ZERO]P");
        assert_eq!(act_zero.len(), 1);

        let (res_zero_next, act_zero_next) = apply_reminders("P", &reminders_init_zero, false);
        assert_eq!(res_zero_next, "P");
        assert_eq!(act_zero_next.len(), 0);
    }

    #[test]
    fn test_append_to_log() {
        let temp_dir = tempfile::tempdir().unwrap();
        let log_path = temp_dir.path().join("test.log");

        append_to_log(&log_path, "user", "Hello");
        append_to_log(&log_path, "llm", "Hi there");

        let content = std::fs::read_to_string(&log_path).unwrap();
        let logs: Vec<LogEntry> = serde_json::from_str(&content).unwrap();

        assert_eq!(logs.len(), 2);
        assert_eq!(logs[0].entity, "user");
        assert_eq!(
            logs[0].content,
            serde_json::Value::String("Hello".to_string())
        );
        assert_eq!(logs[1].entity, "llm");
        assert_eq!(
            logs[1].content,
            serde_json::Value::String("Hi there".to_string())
        );
        assert!(!logs[0].timestamp.is_empty());
    }
}
