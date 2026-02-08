use clap::Parser;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::io::{self, Write};
use std::path::PathBuf;

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

async fn generate(
    client: &reqwest::Client,
    url: &str,
    model: &str,
    prompt: &str,
    session: &mut Session,
) -> Result<(), Box<dyn std::error::Error>> {
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

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        // NDJSON: each chunk might contain one or more JSON objects separated by newlines
        let lines = std::str::from_utf8(&chunk)?;
        for line in lines.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let resp_part: GenerateResponse = serde_json::from_str(line)?;
            print!("{}", resp_part.response);
            io::stdout().flush()?;
            full_response.push_str(&resp_part.response);

            if resp_part.done {
                if let Some(ctx) = resp_part.context {
                    session.context = ctx;
                }
            }
        }
    }
    println!();

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

    if let Some(prompt) = args.prompt {
        generate(&client, &url, &args.model, &prompt, &mut session).await?;
        if let Some(ref path) = args.session {
            let content = serde_json::to_string_pretty(&session)?;
            std::fs::write(path, content)?;
        }
    } else {
        println!("Entering interactive mode. Type 'exit' or use Ctrl+C to quit.");
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

            generate(&client, &url, &args.model, input, &mut session).await?;

            if let Some(ref path) = args.session {
                let content = serde_json::to_string_pretty(&session)?;
                std::fs::write(path, content)?;
            }
        }
    }

    Ok(())
}
