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
- **Streaming by Default:** The tool now uses `stream: true` to provide a real-time "typing" experience.
- **Persistent Sessions:** Context is managed via a `Session` struct and can be saved to a JSON file using the `--session` flag.
- **Interactive REPL:** If no prompt is provided, the tool enters a loop reading from `stdin`.

## Roadmap & Future Tasks
- [x] Streaming Support
- [x] Persistent Sessions
- [x] Interactive Mode
- [x] Probability-based Reminders
- [x] Agentic Command Execution
- [ ] Enhanced API Interaction (e.g., `/api/tags` to list models)
- [ ] Multi-turn chat using `/api/chat` (currently uses `/api/generate` with context)
- [ ] CLI Polish (colorized output, better error handling)

## Guidelines for Gemini
- **Git Commits:** Always run `git commit` after completing a coherent set of changes. Before committing, ensure you run `cargo fmt` and `cargo clippy --all-targets --all-features -- -D warnings` and fix any issues.
- **Keep it Simple:** Avoid over-engineering. `ocha` is intended to be a lightweight tool.
- **Idiomatic Rust:** Follow standard Rust conventions. Use `Result` for error handling.
- **Documentation:** Always update `README.md` when adding new CLI flags or features.
- **Verification:** Before finalizing changes, test against the local Ollama server (defaulting to `localhost:11434`).
