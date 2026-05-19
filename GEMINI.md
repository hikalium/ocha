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

> **Design rationale:** the *why* behind everything below — the
> provider-agnostic agent loop, the single approval/execution point, and
> the lowest-common-denominator interface across Anthropic / Gemini /
> OpenAI / Ollama — is recorded in
> [`docs/llm-interface-layering-and-common-abstraction.md`](docs/llm-interface-layering-and-common-abstraction.md).
> Read it before changing backends, the turn loop, or the
> `!!!OCHA_RUN_CMD` protocol.

- **Provider-neutral backend trait:** `src/backend/` defines a `Backend`
  trait (`chat` + `list_models`); `ollama`, `claude` and `claude_cli`
  implement it. `main.rs` owns the backend-agnostic turn loop, reminders,
  logging and the agentic command protocol. Add a new provider by adding
  one module.
- **`claude-cli` is a contained upper-layer exception:** it shells out to
  the installed Claude Code CLI so no API key is needed, but only as a
  tools-disabled text engine. The full rationale and the invariant that
  keeps ocha's approval gate intact are in the design note's §4
  ("The `claude-cli` backend: a deliberate, contained exception"). Read
  it before touching that backend.
- **Neutral conversation model:** sessions are a `Vec<Message>` of
  role-tagged text (resent each turn — the lowest common denominator that
  works across Ollama, Anthropic and Gemini). The Ollama-only opaque
  `context` token array was removed.
- **Agentic protocol stays plain text:** the `!!!OCHA_RUN_CMD` mechanism
  is backend-independent and works even on models with no native tool
  calling. ocha's loop is the single approval/execution point.
- **Streaming by Default:** `stream: true`; each backend owns its own
  framing (Ollama NDJSON vs Anthropic SSE) behind the trait.
- **Interactive REPL:** If no prompt is provided, read from `stdin`.

## Roadmap & Future Tasks
- [x] Streaming Support
- [x] Persistent Sessions
- [x] Interactive Mode
- [x] Probability-based Reminders
- [x] Agentic Command Execution
- [x] Enhanced API Interaction (`list-models` for every backend)
- [x] Multi-turn chat using `/api/chat` (neutral message history)
- [x] Provider-neutral backend trait (Ollama + Claude)
- [x] `claude-cli` backend (installed Claude Code CLI; no API key)
- [ ] **Gemini backend** (see plan below)
- [ ] CLI Polish (colorized output, better error handling)

## Gemini backend — implementation plan

Add `src/backend/gemini.rs` implementing `Backend`, wired in as
`BackendKind::Gemini` (`--backend gemini`). No changes to the turn loop.

1. **Endpoint:** `POST {base}/v1beta/models/{model}:streamGenerateContent?alt=sse`
   (base default `https://generativelanguage.googleapis.com`). Auth via
   `x-goog-api-key: $GEMINI_API_KEY` (error if unset, like the claude key).
2. **Request mapping:** body `{ contents:[...], systemInstruction:{parts:[{text}]} }`.
   Map roles: `Assistant -> "model"`, `User`/`Tool -> "user"`; `System`
   turns + `--system` merged into `systemInstruction` (same split the
   `claude` backend already does). Each message → `{role, parts:[{text}]}`.
3. **Streaming:** with `alt=sse` Gemini emits SSE `data:` lines carrying
   `GenerateContentResponse`; extract
   `candidates[0].content.parts[].text`, push to the token sink, accumulate
   full text. Reuse the line/`data:` parser shape from `claude.rs`.
4. **Finish/usage:** read `candidates[0].finishReason`; `usageMetadata`
   optional (kept out of the base contract).
5. **list_models:** `GET {base}/v1beta/models` → map `name`/`displayName`
   to `ModelInfo`. Default model e.g. `gemini-2.5-flash`.
6. **Tools:** none declared — the text `!!!OCHA_RUN_CMD` protocol already
   covers agentic use; declaring Gemini hosted tools would bypass ocha's
   approval point, so they are intentionally omitted.
7. **Tests/docs:** key-gated e2e like claude; update README options table
   and the backend examples; `cargo fmt` + `clippy -D warnings`.

Estimated surface: ~1 new file (~150 lines) + a `BackendKind` arm + a
`build_backend` arm. The neutral loop, sessions, reminders and command
protocol need no changes — this is the payoff of the trait refactor.

## Guidelines for Gemini
- **Git Commits:** Always run `git commit` after completing a coherent set of changes. Before committing, ensure you run `cargo fmt` and `cargo clippy --all-targets --all-features -- -D warnings` and fix any issues.
- **Keep it Simple:** Avoid over-engineering. `ocha` is intended to be a lightweight tool.
- **Idiomatic Rust:** Follow standard Rust conventions. Use `Result` for error handling.
- **Documentation:** Always update `README.md` when adding new CLI flags or features.
- **Verification:** Before finalizing changes, test against the local Ollama server (defaulting to `localhost:11434`).
