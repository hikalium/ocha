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

Basic usage with default settings (localhost:11434, model: gemma3:27b):

```bash
ocha "Hello, how are you?"
```

### Options

| Flag | Long Flag | Description | Default |
|------|-----------|-------------|---------|
| `-s` | `--server`| IP address of the Ollama server | `127.0.0.1` |
| `-p` | `--port`  | Port of the Ollama server | `11434` |
| `-m` | `--model` | Model name to use | `gemma3:27b` |

### Examples

**Specify a different server and port:**
```bash
ocha -s 192.168.1.50 -p 11434 "Tell me a joke."
```

**Use a different model:**
```bash
ocha -m llama3 "What is Rust?"
```
