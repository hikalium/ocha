# ocha

A simple Rust CLI tool to interact with an Ollama server on your local network.

## Prerequisites

- **Rust toolchain**: Installed via [rustup](https://rustup.rs/).
- **Ollama**: An Ollama server running and reachable on your network.

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
| `-s` | `--server`| IP address of the Ollama server | `127.0.0.1` |
| `-p` | `--port`  | Port of the Ollama server | `11434` |
| `-m` | `--model` | Model name to use | `gemma3:27b` |
| `-S` | `--session`| Path to session JSON file | (None) |
| `-r` | `--reminders`| Path to reminders JSON file | (None) |

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
