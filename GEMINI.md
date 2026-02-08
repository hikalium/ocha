# Gemini Development Guide: ocha

This document provides context for AI agents (like Gemini) to assist in the ongoing development of the `ocha` CLI tool.

## Project Context
`ocha` is a minimalist Rust-based CLI client for Ollama. It currently supports basic single-turn generation using the Ollama `/api/generate` endpoint.

- **Tech Stack:** Rust (Edition 2021)
- **Key Libraries:**
  - `clap`: Command-line argument parsing (Derive API).
  - `reqwest`: HTTP client for API requests.
  - `tokio`: Async runtime.
  - `serde`/`serde_json`: Serialization/Deserialization.
- **Default Model:** `gemma3:27b` (Adjusted to match local environment availability).

## Architecture Decisions
- **Non-Streaming by Default:** To keep the initial implementation simple, `stream: false` is used in the API request.
- **Direct Output:** The tool prints the final response string directly to `stdout`.

## Roadmap & Future Tasks
When continuing development, consider the following enhancements:

### 1. Streaming Support
- Modify `GenerateRequest` to set `stream: true`.
- Use `reqwest` to handle the response stream.
- Parse the NDJSON (Newline Delimited JSON) chunks and print them to `stdout` in real-time.

### 2. Enhanced API Interaction
- Implement `/api/tags` to list available models.
- Implement `/api/chat` for multi-turn conversations (maintaining message history).
- Add support for system prompts.

### 3. CLI Polish
- Add a `--json` flag to output the raw response.
- Improve error messages (e.g., check if the server is up before sending the payload).
- Add colorized output for better readability.

## Guidelines for Gemini
- **Git Commits:** Always run `git commit` after completing a coherent set of changes (e.g., after adding a feature or fixing a bug).
- **Keep it Simple:** Avoid over-engineering. `ocha` is intended to be a lightweight tool.
- **Idiomatic Rust:** Follow standard Rust conventions. Use `Result` for error handling.
- **Documentation:** Always update `README.md` when adding new CLI flags or features.
- **Verification:** Before finalizing changes, test against the local Ollama server (defaulting to `localhost:11434`).
