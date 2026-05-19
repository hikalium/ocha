use assert_cmd::Command;
use tempfile::NamedTempFile;

/// Drive the neutral-session secret-word flow against whatever backend
/// `extra_args` selects: invoke `ocha` once to state a fact into a fresh
/// session file, then a *second, separate* process that only reads that
/// session back and must recall the fact. This exercises the
/// provider-neutral `{"messages":[...]}` session end to end (persisted by
/// one process, replayed by another) and is identical across backends —
/// only `extra_args` differs.
#[allow(deprecated)] // assert_cmd::Command::cargo_bin: fine without a custom build-dir
fn assert_session_recall(extra_args: &[&str]) {
    let session_file = NamedTempFile::new().unwrap();
    let session_path = session_file.path().to_str().unwrap();

    // Step 1: state the fact into a new session.
    let mut cmd = Command::cargo_bin("ocha").unwrap();
    cmd.args(extra_args)
        .arg("-S")
        .arg(session_path)
        .arg("My secret word is 'XEBRA'. Remember it.")
        .assert()
        .success();

    // Step 2: a fresh process reloads the session and must recall it.
    let mut cmd = Command::cargo_bin("ocha").unwrap();
    let assert = cmd
        .args(extra_args)
        .arg("-S")
        .arg(session_path)
        .arg("What is my secret word?")
        .assert()
        .success();

    let output = String::from_utf8_lossy(&assert.get_output().stdout);
    assert!(
        output.to_uppercase().contains("XEBRA"),
        "Output did not contain the secret word: {}",
        output
    );
}

#[test]
fn test_session_persistence_ollama() {
    // Ollama host is overridable so the E2E test can target a remote
    // server (e.g. OCHA_TEST_OLLAMA_HOST=eevee). Defaults to localhost.
    let host = std::env::var("OCHA_TEST_OLLAMA_HOST").unwrap_or_else(|_| "localhost".to_string());

    // Check if Ollama is running first. If not, skip the test.
    if reqwest::blocking::get(format!("http://{host}:11434")).is_err() {
        eprintln!("Ollama server not found at {host}:11434, skipping Ollama E2E test.");
        return;
    }

    assert_session_recall(&["-s", &host]);
}

#[test]
fn test_session_persistence_claude() {
    // Key-gated: skip cleanly when no Anthropic credentials are present
    // (CI without secrets, offline dev) — same self-skip philosophy as
    // the Ollama reachability check above. ANTHROPIC_API_KEY is inherited
    // by the spawned `ocha` process from this test process's environment.
    if std::env::var("ANTHROPIC_API_KEY").is_err() {
        eprintln!("ANTHROPIC_API_KEY not set, skipping Claude E2E test.");
        return;
    }

    // Model is overridable; default to a cheap, fully-capable model so the
    // paid e2e round trip stays inexpensive.
    let model = std::env::var("OCHA_TEST_CLAUDE_MODEL")
        .unwrap_or_else(|_| "claude-haiku-4-5-20251001".to_string());

    assert_session_recall(&["--backend", "claude", "-m", &model]);
}
