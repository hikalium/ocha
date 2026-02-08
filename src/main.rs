use clap::Parser;
use serde::{Deserialize, Serialize};

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

    /// The prompt to send to the model
    prompt: String,
}

#[derive(Serialize)]
struct GenerateRequest<'a> {
    model: &'a str,
    prompt: &'a str,
    stream: bool,
}

#[derive(Deserialize)]
struct GenerateResponse {
    response: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    let url = format!("http://{}:{}/api/generate", args.server, args.port);

    let client = reqwest::Client::new();
    let request_body = GenerateRequest {
        model: &args.model,
        prompt: &args.prompt,
        stream: false,
    };

    let res = client
        .post(&url)
        .json(&request_body)
        .send()
        .await?
        .error_for_status()?;

    let response_data: GenerateResponse = res.json().await?;

    println!("{}", response_data.response);

    Ok(())
}