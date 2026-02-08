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

    /// The prompt to send to the model. If omitted, starts interactive mode.
    prompt: Option<String>,
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

fn apply_reminders(prompt: &str, reminders: &[Reminder], is_new_session: bool) -> String {
    let mut rng = rand::rng();
    let mut pre = String::new();
    let mut post = String::new();

    for reminder in reminders {
        if reminder.init && !is_new_session {
            continue;
        }
        if rng.random_range(0.0..1.0) < reminder.probability {
            match reminder.timing {
                Timing::Pre => pre.push_str(&reminder.prompt),
                Timing::Post => post.push_str(&reminder.prompt),
            }
        }
    }

    format!("{}{}{}", pre, prompt, post)
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

async fn generate_internal(
    client: &reqwest::Client,
    url: &str,
    model: &str,
    prompt: &str,
    session: &mut Session,
    stream_output: bool,
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

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        let lines = std::str::from_utf8(&chunk)?;
        for line in lines.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let resp_part: GenerateResponse = serde_json::from_str(line)?;

            full_response.push_str(&resp_part.response);

            // Check for command marker only at the beginning
            if !is_command_mode && full_response.trim_start().starts_with("!!!OCHA_RUN_CMD") {
                is_command_mode = true;
            }

            if stream_output && !is_command_mode {
                print!("{}", resp_part.response);
                io::stdout().flush()?;
            }

            #[allow(clippy::collapsible_if)]
            if resp_part.done {
                if let Some(ctx) = resp_part.context {
                    session.context = ctx;
                }
            }
        }
    }

    if stream_output && !is_command_mode {
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
}

async fn run_turn(config: RunTurnConfig<'_>) -> Result<(), Box<dyn std::error::Error>> {
    let mut current_prompt = apply_reminders(
        config.initial_prompt,
        config.reminders,
        config.is_new_session,
    );

    let mut remaining_commands = config.command_per_response;

    loop {
        let response = generate_internal(
            config.client,
            config.url,
            config.model,
            &current_prompt,
            config.session,
            true,
        )
        .await?;

        if let Some(cmd_json_str) = response.trim().strip_prefix("!!!OCHA_RUN_CMD") {
            if remaining_commands == 0 {
                let error_result = CommandResult {
                    status: None,

                    stdout: String::new(),

                    stderr: String::new(),

                    remaining_commands: 0,

                    error: Some("Command execution limit exceeded per response.".to_string()),
                };

                current_prompt = serde_json::to_string(&error_result)?;

                // Continue loop to report error to model
            } else {
                remaining_commands -= 1;

                // Parse JSON

                match serde_json::from_str::<CommandRequest>(cmd_json_str) {
                    Ok(req) => {
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
                        current_prompt = serde_json::to_string(&result)?;
                    }

                    Err(e) => {
                        let error_result = CommandResult {
                            status: None,

                            stdout: String::new(),

                            stderr: String::new(),

                            remaining_commands,

                            error: Some(format!("Failed to parse command request: {}", e)),
                        };

                        current_prompt = serde_json::to_string(&error_result)?;
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
