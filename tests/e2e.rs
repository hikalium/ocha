use assert_cmd::Command;
use tempfile::NamedTempFile;

#[allow(deprecated)]
#[test]
fn test_session_persistence() {
    // Ollama host is overridable so the E2E test can target a remote
    // server (e.g. OCHA_TEST_OLLAMA_HOST=eevee). Defaults to localhost.
    let host = std::env::var("OCHA_TEST_OLLAMA_HOST").unwrap_or_else(|_| "localhost".to_string());

    // Check if Ollama is running first. If not, skip the test.
    if reqwest::blocking::get(format!("http://{host}:11434")).is_err() {
        eprintln!("Ollama server not found at {host}:11434, skipping E2E test.");
        return;
    }

    let session_file = NamedTempFile::new().unwrap();
    let session_path = session_file.path().to_str().unwrap();

    // Step 1: Tell the model a specific fact
    let mut cmd = Command::cargo_bin("ocha").unwrap();
    cmd.arg("-s")
        .arg(&host)
        .arg("-S")
        .arg(session_path)
        .arg("My secret word is 'XEBRA'. Remember it.")
        .assert()
        .success();

    // Step 2: Ask the model for the fact using the session
    let mut cmd = Command::cargo_bin("ocha").unwrap();
    let assert = cmd
        .arg("-s")
        .arg(&host)
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
