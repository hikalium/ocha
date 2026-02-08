use clap::Parser;
use futures_util::StreamExt;
use rand::RngExt;
use serde::{Deserialize, Serialize};
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::AsyncReadExt;
use tokio::time::{Duration, timeout};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// IP address of the Ollama server
    #[arg(short = 's', long, default_value = "127.0.0.1")]
    server: String,

    /// Port of the Ollama server
    #[arg(short = 'p', long, default_value = "11434")]
    port: u16,

    /// Model to use
    #[arg(short = 'm', long, default_value = "gemma3:27b")]
    model: String,

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

    /// List available models on the Ollama server
    #[arg(long)]
    list_models: bool,

    /// The prompt to send to the model. If omitted, starts interactive mode.
    prompt: Option<String>,
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

#[derive(Deserialize)]
struct ModelInfo {
    name: String,
    size: u64,
    modified_at: String,
}

#[derive(Deserialize)]
struct ModelsResponse {
    models: Vec<ModelInfo>,
}

#[derive(Serialize)]
struct GenerateRequest<'a> {
    model: &'a str,
    prompt: &'a str,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    context: Option<&'a [i32]>,
}

#[derive(Deserialize, Serialize, Default)]
struct Session {
    context: Vec<i32>,
}

#[derive(Deserialize)]
struct GenerateResponse {
    response: String,
    done: bool,
    context: Option<Vec<i32>>,
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

fn apply_reminders(prompt: &str, reminders: &[Reminder], is_new_session: bool) -> (String, Vec<String>) {
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

async fn list_models(
    client: &reqwest::Client,
    url: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let res = client.get(url).send().await?;
    if !res.status().is_success() {
        let err_text = res.text().await?;
        return Err(format!("Ollama API error: {}", err_text).into());
    }

    let resp: ModelsResponse = res.json().await?;
    println!("{:<40} {:<10} {:<20}", "NAME", "SIZE", "MODIFIED");
    println!("{}", "-".repeat(70));
    for model in resp.models {
        let size_gb = model.size as f64 / 1_073_741_824.0;
        println!(
            "{:<40} {:<10.2} GB {:<20}",
            model.name, size_gb, model.modified_at
        );
    }
    Ok(())
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

async fn generate_internal(
    client: &reqwest::Client,
    url: &str,
    model: &str,
    prompt: &str,
    session: &mut Session,
    stream_output: bool,
    log_path: Option<&std::path::Path>,
) -> Result<String, Box<dyn std::error::Error>> {
    let request_body = GenerateRequest {
        model,
        prompt,
        stream: true,
        context: if session.context.is_empty() {
            None
        } else {
            Some(&session.context)
        },
    };

    let res = client.post(url).json(&request_body).send().await?;

    if !res.status().is_success() {
        let err_text = res.text().await?;
        return Err(format!("Ollama API error: {}", err_text).into());
    }

    let mut stream = res.bytes_stream();
    let mut full_response = String::new();
    let mut is_command_mode = false;
    let mut last_line_start = 0;
    let mut line_buffer = Vec::new();

    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                if let (Some(path), false) = (log_path, full_response.trim().is_empty()) {
                    append_to_log(path, "llm", full_response.trim());
                }
                return Err(Box::new(e));
            }
        };

        line_buffer.extend_from_slice(&chunk);

        while let Some(newline_pos) = line_buffer.iter().position(|&b| b == b'\n') {
            let line_bytes = line_buffer.drain(..=newline_pos).collect::<Vec<u8>>();
            let line_str = match std::str::from_utf8(&line_bytes) {
                Ok(s) => s,
                Err(e) => {
                    if let (Some(path), false) = (log_path, full_response.trim().is_empty()) {
                        append_to_log(path, "llm", full_response.trim());
                    }
                    return Err(Box::new(e));
                }
            };

            if line_str.trim().is_empty() {
                continue;
            }

            let resp_part: GenerateResponse = match serde_json::from_str(line_str) {
                Ok(rp) => rp,
                Err(e) => {
                    if let (Some(path), false) = (log_path, full_response.trim().is_empty()) {
                        append_to_log(path, "llm", full_response.trim());
                    }
                    return Err(Box::new(e));
                }
            };

            for c in resp_part.response.chars() {
                full_response.push(c);

                if !is_command_mode {
                    let current_line = &full_response[last_line_start..];
                    if is_command_line(current_line) {
                        is_command_mode = true;
                    }
                }

                if stream_output {
                    print!("{}", c);
                    io::stdout().flush()?;
                }

                if c == '\n' {
                    last_line_start = full_response.len();
                }
            }

            if let (true, Some(ctx)) = (resp_part.done, resp_part.context) {
                session.context = ctx;
            }
        }
    }

    if stream_output {
        println!();
    }

    Ok(full_response)
}

struct RunTurnConfig<'a> {
    client: &'a reqwest::Client,

    url: &'a str,

    model: &'a str,

    initial_prompt: &'a str,

    session: &'a mut Session,

    reminders: &'a [Reminder],

    command_per_response: usize,

    is_new_session: bool,

    log_path: Option<&'a std::path::Path>,
}

async fn run_turn(config: RunTurnConfig<'_>) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(path) = config.log_path {
        append_to_log(path, "user", config.initial_prompt);
    }

    let mut current_turn_prompt = config.initial_prompt.to_string();
    let mut remaining_commands = config.command_per_response;
    let mut is_first_turn = config.is_new_session;

    loop {
        let (prompted_input, activated_reminders) = apply_reminders(
            &current_turn_prompt,
            config.reminders,
            is_first_turn,
        );

        is_first_turn = false;

        for reminder in activated_reminders {
            println!("[Reminder: {}]", reminder.trim());
        }

        let response = generate_internal(
            config.client,
            config.url,
            config.model,
            &prompted_input,
            config.session,
            true,
            config.log_path,
        )
        .await?;

        // Log the LLM response before potentially executing a command
        if let (Some(path), false) = (config.log_path, response.trim().is_empty()) {
            append_to_log(path, "llm", response.trim());
        }

        if let Some(cmd_json_str) = extract_command(&response) {
            if remaining_commands == 0 {
                let error_result = CommandResult {
                    status: None,
                    stdout: String::new(),
                    stderr: String::new(),
                    remaining_commands: 0,
                    error: Some("Command execution limit exceeded per response.".to_string()),
                };

                current_turn_prompt = serde_json::to_string(&error_result).unwrap_or_else(|_| {
                    "{\"error\": \"Failed to serialize limit error\"}".to_string()
                });

                if let Some(path) = config.log_path {
                    append_to_log(path, "tool", &error_result);
                }
                // Continue loop to report error to model
            } else {
                remaining_commands -= 1;

                // Parse JSON
                match serde_json::from_str::<CommandRequest>(cmd_json_str) {
                    Ok(req) => {
                        println!("[Payload: {}]", cmd_json_str);
                        println!("[Executing: {} {}]", req.binary, req.args.join(" "));
                        let (status, stdout, stderr, error) = execute_command(req).await;
                        if !stdout.is_empty() {
                            println!("STDOUT:\n{}", stdout);
                        }
                        if !stderr.is_empty() {
                            println!("STDERR:\n{}", stderr);
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
                        println!("[Result: {}]", result_json);

                        if let Some(path) = config.log_path {
                            append_to_log(path, "tool", &result);
                        }

                        current_turn_prompt = result_json;
                    }

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

                        current_turn_prompt =
                            serde_json::to_string(&error_result).unwrap_or_else(|_| {
                                "{\"error\": \"Failed to serialize parse error\"}".to_string()
                            });
                    }
                }
            }
        } else {
            // Normal text response, we are done with this turn

            break;
        }
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let url = format!("http://{}:{}/api/generate", args.server, args.port);
    let client = reqwest::Client::new();

    if args.list_models {
        let tags_url = format!("http://{}:{}/api/tags", args.server, args.port);
        list_models(&client, &tags_url).await?;
        return Ok(());
    }

    let mut session = if let Some(ref path) = args.session {
        if path.exists() {
            let content = std::fs::read_to_string(path)?;
            serde_json::from_str(&content).unwrap_or_default()
        } else {
            Session::default()
        }
    } else {
        Session::default()
    };

    // Simple check: if context is empty, it's a new session
    let is_new_session_initial = session.context.is_empty();

    let reminders: Vec<Reminder> = if let Some(ref path) = args.reminders {
        let content = std::fs::read_to_string(path)?;
        serde_json::from_str(&content)?
    } else {
        Vec::new()
    };

    if let Some(prompt) = args.prompt {
        run_turn(RunTurnConfig {
            client: &client,
            url: &url,
            model: &args.model,
            initial_prompt: &prompt,
            session: &mut session,
            reminders: &reminders,
            command_per_response: args.command_per_response,
            is_new_session: is_new_session_initial,
            log_path: args.log.as_deref(),
        })
        .await?;
        if let Some(ref path) = args.session {
            let content = serde_json::to_string_pretty(&session)?;
            std::fs::write(path, content)?;
        }
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
                client: &client,
                url: &url,
                model: &args.model,
                initial_prompt: input,
                session: &mut session,
                reminders: &reminders,
                command_per_response: args.command_per_response,
                is_new_session: is_new,
                log_path: args.log.as_deref(),
            })
            .await?;

            first_turn = false;

            if let Some(ref path) = args.session {
                let content = serde_json::to_string_pretty(&session)?;
                std::fs::write(path, content)?;
            }
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

        // Test init only reminder
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

        // Test init: true with 0 probability always fires on new session
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
