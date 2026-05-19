# ocha

A simple Rust CLI tool to talk to LLM backends behind one provider-neutral
interface. Supported backends:

- **`ollama`** (default) — a local/network Ollama server (native `/api/chat`).
- **`claude`** — the Anthropic Messages API (`/v1/messages`).

ocha owns its own agent loop, so the agentic command protocol is plain text
and works identically on every backend regardless of native tool support.

The design rationale for this architecture — why ocha owns its loop and
approval point rather than using a vendor agent SDK, and the
lowest-common-denominator interface across the supported backends — is in
[`docs/llm-interface-layering-and-common-abstraction.md`](docs/llm-interface-layering-and-common-abstraction.md).

## Prerequisites

- **Rust toolchain**: Installed via [rustup](https://rustup.rs/).
- **Ollama** (for the `ollama` backend): a server running and reachable.
- **`ANTHROPIC_API_KEY`** (for the `claude` backend): exported in the env.

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
|      | `--backend`| Backend: `ollama` or `claude` | `ollama` |
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

### Examples

**Specify a different server and port:**
```bash
ocha -s 192.168.1.50 -p 11434 "Tell me a joke."
```

**Use a different model:**
```bash
ocha -m llama3 "What is Rust?"
```

## Testing

```bash
cargo test
```

Unit tests run anywhere. The end-to-end session-persistence test
(`tests/e2e.rs`) needs a reachable Ollama server; it **skips itself**
when none is found. By default it looks at `localhost:11434`. Point it
at a remote server with `OCHA_TEST_OLLAMA_HOST` (the test passes this to
`ocha` via `-s`):

```bash
OCHA_TEST_OLLAMA_HOST=eevee cargo test --test e2e -- --nocapture
```
