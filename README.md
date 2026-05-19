# ocha

A simple Rust CLI tool to talk to LLM backends behind one provider-neutral
interface. Supported backends:

- **`ollama`** (default) — a local/network Ollama server (native `/api/chat`).
- **`claude`** — the Anthropic Messages API (`/v1/messages`).
- **`claude-cli`** — the locally installed, already-authenticated
  `claude` (Claude Code) CLI in print mode. No `ANTHROPIC_API_KEY`
  needed. Claude Code's own built-in tools are removed (`--tools ""`)
  and a system preamble makes it speak ocha's plain-text protocol, so it
  behaves like any other backend (plain chat *and* agentic) while ocha
  stays the single approval point. Trades the design note's "build on
  the primitive API" stance for zero-setup auth — see that note's §4.

ocha owns its own agent loop, so the agentic command protocol is plain
text and works the same on **every** backend regardless of native tool
support — including `claude-cli`, where Claude Code's own built-in tools
are removed (`--tools ""`) and a system preamble makes it speak ocha's
plain-text protocol just like a tool-less local model.

The design rationale for this architecture — why ocha owns its loop and
approval point rather than using a vendor agent SDK, and the
lowest-common-denominator interface across the supported backends — is in
[`docs/llm-interface-layering-and-common-abstraction.md`](docs/llm-interface-layering-and-common-abstraction.md).

## Prerequisites

- **Rust toolchain**: Installed via [rustup](https://rustup.rs/).
- **Ollama** (for the `ollama` backend): a server running and reachable.
- **`ANTHROPIC_API_KEY`** (for the `claude` backend): exported in the env.
- **Claude Code CLI** (for the `claude-cli` backend): `claude` installed,
  on `PATH`, and logged in (`claude` once interactively, or `claude
  login`). No API key required.

## Installation

To build and install the binary locally:

```bash
cd ocha
cargo install --path .
```

## Usage

### One-off Prompt
Basic usage with default settings (localhost:11434, model: gemma3:27b):

```bash
ocha "Hello, how are you?"
```

### Interactive Mode
If no prompt is provided, `ocha` enters an interactive REPL mode:

```bash
ocha
```
Type `exit` or use `Ctrl+C` to quit.

### Persistent Sessions
Use the `-S` or `--session` flag to specify a JSON file for saving conversation context. This allows the model to remember previous interactions even across different command invocations.

```bash
ocha -S my_chat.json "My name is Alice."
ocha -S my_chat.json "What is my name?"
```

### Options

| Flag | Long Flag | Description | Default |
|------|-----------|-------------|---------|
|      | `--backend`| Backend: `ollama`, `claude`, or `claude-cli` | `ollama` |
| `-s` | `--server`| IP address of the Ollama server (ollama only) | `127.0.0.1` |
| `-p` | `--port`  | Port of the Ollama server (ollama only) | `11434` |
|      | `--api-base`| Override API base URL | per-backend |
| `-m` | `--model` | Model name to use | ollama `gemma3:27b` / claude `claude-sonnet-4-6` |
|      | `--max-tokens`| Max tokens to generate (claude only) | `4096` |
|      | `--system`| System prompt sent out of band | (None) |
| `-S` | `--session`| Path to session JSON file | (None) |
| `-r` | `--reminders`| Path to reminders JSON file | (None) |

> **Session format change.** Sessions are now stored as a neutral message
> history (`{"messages":[...]}`) so they work across backends. Old
> Ollama-only `{"context":[...]}` files are ignored and start fresh.

#### Backend examples

```bash
# Ollama (default)
ocha -m llama3 "What is Rust?"

# Claude (Anthropic Messages API)
export ANTHROPIC_API_KEY=sk-ant-...
ocha --backend claude "Explain ownership in one sentence."
ocha --backend claude -m claude-sonnet-4-6 --system "Be terse." "Hi"

# Claude via the installed Claude Code CLI (no API key; uses its login)
ocha --backend claude-cli "Explain ownership in one sentence."
ocha --backend claude-cli -m haiku --system "Be terse." "Hi"
# OCHA_CLAUDE_CLI overrides the binary path if `claude` is not on PATH.
```

### Reminders
You can inject hidden prompts (e.g., system instructions) based on probability using a reminders file.

**reminders.json example:**
```json
[
  {
    "probability": 1.0,
    "prompt": " (Keep your response very brief)",
    "timing": "post",
    "init": false
  }
]
```
The `init` field (default: `false`) determines if the reminder is only applied at the start of a new session.

### Command Execution (Agentic Capabilities)
`ocha` can execute commands on behalf of the model if enabled. The model can request execution by outputting a specific JSON structure prefixed with `!!!OCHA_RUN_CMD`.

**Protocol:**
1. Model outputs: `!!!OCHA_RUN_CMD{"binary": "ls", "args": ["-la"], "timeout": 5, "description": "Listing files"}`
2. `ocha` executes the command and feeds the result (exit code, stdout, stderr) back to the model as a JSON payload.
3. The model continues generation based on the command result.

**Safety Limits:**
- `--command-per-response`: Limits the number of chained commands per user turn (default: 5).

> **Backend note.** Verified on all three backends (ocha intercepts
> `!!!OCHA_RUN_CMD`, executes, and feeds the result back). For
> **`claude-cli`** this is achieved by removing Claude Code's built-in
> tools (`--tools ""`) and prepending a system preamble that directs it
> to ocha's plain-text protocol — so it behaves like a tool-less local
> model and can never execute anything itself; ocha remains the single
> approval point.

### Examples

**Specify a different server and port:**
```bash
ocha -s 192.168.1.50 -p 11434 "Tell me a joke."
```

**Use a different model:**
```bash
ocha -m llama3 "What is Rust?"
```

## Remote Web UI (`ocha serve`)

`ocha serve` starts a small local HTTP server with a single-page web UI
for driving conversations remotely — **with a per-command approve/deny
gate**. The full design rationale and API are in
[`docs/web-ui-remote-control-design.md`](docs/web-ui-remote-control-design.md).

```bash
# Top-level flags become the default config for new sessions:
ocha --backend claude-cli -m haiku serve            # http://127.0.0.1:8765
ocha --backend ollama -s eevee serve --port 9000
```

Open the printed `http://127.0.0.1:<port>` in a browser: create a
session, send prompts, watch tokens stream, and **approve or deny each
`!!!OCHA_RUN_CMD` before it runs**. The UI is plain HTML/JS embedded in
the binary — no build step, no external assets.

- **Loopback only, no auth — by design.** The server binds `127.0.0.1`;
  it is *not* reachable off-box. Anyone with local access can drive the
  session and approve commands that execute with your privileges — treat
  the machine as the trust boundary. For remote use, tunnel it
  (`ssh -L 8765:127.0.0.1:8765 host`); ocha intentionally does not add
  TLS/auth (see design §7).
- **Approval gate.** Sessions default to `gated` (remote approve/deny;
  unanswered commands auto-deny after `OCHA_APPROVAL_TIMEOUT_SECS`,
  default 600s, and the turn continues — never an indefinite hang). Set
  `"approval_mode":"auto"` on a session to auto-execute like the CLI.
- No `ANTHROPIC_API_KEY` is needed if you use the `claude-cli` backend.

**Manual UI smoke checklist** (the API paths are covered by
`tests/serve.rs`):

1. `ocha --backend claude-cli -m haiku serve`, open the URL.
2. **New** (optionally a system prompt) → a session appears, selected.
3. Send "list files here with ls" → tokens stream into the transcript.
4. An approval banner shows the `ls` command → **Approve** → result
   appears, the turn continues to completion.
5. Send another command prompt → **Deny** with a reason → the model is
   told it was denied and continues.
6. While a turn/approval is pending, **Cancel** unwinds it.

## Testing

```bash
cargo test
```

Unit tests and the `ocha serve` integration tests (`tests/serve.rs`,
hermetic — spawn the binary with an in-process mock backend, no network)
run anywhere. The end-to-end tests (`tests/e2e.rs`) run the same
provider-neutral session-recall flow (persist a fact in one process,
recall it in a second) against each backend, and **each one skips
itself** when its backend is unavailable — so `cargo test` is always
green offline.

| Test | Runs when | Configure with |
|------|-----------|----------------|
| `test_session_persistence_ollama` | an Ollama server is reachable | `OCHA_TEST_OLLAMA_HOST` (default `localhost`; passed to `ocha` via `-s`) |
| `test_session_persistence_claude` | `ANTHROPIC_API_KEY` is set | `OCHA_TEST_CLAUDE_MODEL` (default `claude-haiku-4-5-20251001`, kept cheap) |
| `test_session_persistence_claude_cli` | `OCHA_TEST_CLAUDE_CLI=1` **and** `claude` on `PATH` (opt-in: spends real subscription budget) | `OCHA_TEST_CLAUDE_MODEL` (default `haiku`) |

```bash
# Ollama backend against a remote server
OCHA_TEST_OLLAMA_HOST=eevee cargo test --test e2e -- --nocapture

# Claude backend (live Anthropic API — a small paid round trip)
ANTHROPIC_API_KEY=sk-ant-... cargo test --test e2e -- --nocapture

# claude-cli backend (opt-in; uses the CLI's login, no API key)
OCHA_TEST_CLAUDE_CLI=1 cargo test --test e2e -- --nocapture

# All backends in one run
ANTHROPIC_API_KEY=sk-ant-... OCHA_TEST_OLLAMA_HOST=eevee \
  OCHA_TEST_CLAUDE_CLI=1 cargo test --test e2e -- --nocapture
```
