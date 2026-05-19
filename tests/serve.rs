//! M2 acceptance: hermetic integration test for `ocha serve`.
//!
//! Spawns the real binary with the in-process mock backend
//! (`OCHA_MOCK_BACKEND=1`) on an OS-assigned port — no network, no real
//! model, deterministic. Asserts the lifecycle endpoints and the
//! loopback-only bind.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};

/// Start `ocha serve --port 0` (mock backend) and return the child plus
/// the base URL it printed once bound.
#[allow(deprecated)]
// assert_cmd::cargo::cargo_bin: fine without a custom build-dir
// The child is returned to the caller, which kill()s + wait()s it; the
// lint can't see across the helper boundary.
#[allow(clippy::zombie_processes)]
fn spawn_server() -> (Child, String) {
    let bin = assert_cmd::cargo::cargo_bin("ocha");
    let mut child = Command::new(bin)
        .arg("serve")
        .arg("--port")
        .arg("0")
        .env("OCHA_MOCK_BACKEND", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ocha serve");

    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).expect("read serve stdout") == 0 {
            let _ = child.kill();
            let _ = child.wait();
            panic!("ocha serve exited before announcing its address");
        }
        if let Some(idx) = line.find("http://") {
            return (child, line[idx..].trim().to_string());
        }
    }
}

#[test]
fn serve_lifecycle_hermetic() {
    let (mut child, base) = spawn_server();
    let c = reqwest::blocking::Client::new();

    // Loopback-only bind (design §7).
    assert!(
        base.starts_with("http://127.0.0.1:"),
        "server not bound to loopback: {base}"
    );

    // Health.
    let h: serde_json::Value = c
        .get(format!("{base}/api/health"))
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(h["ok"], true);
    assert!(h["version"].is_string());

    // Models come from the mock (hermetic).
    let m: serde_json::Value = c
        .get(format!("{base}/api/models"))
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(m["models"][0]["name"], "mock-model");

    // Create.
    let r = c
        .post(format!("{base}/api/sessions"))
        .body(r#"{"system":"hermetic"}"#)
        .send()
        .unwrap();
    assert_eq!(r.status().as_u16(), 201);
    let v: serde_json::Value = r.json().unwrap();
    let id = v["id"].as_str().unwrap().to_string();
    assert_eq!(v["state"], "idle");

    // List reflects it.
    let list: serde_json::Value = c
        .get(format!("{base}/api/sessions"))
        .send()
        .unwrap()
        .json()
        .unwrap();
    assert!(
        list.as_array().unwrap().iter().any(|s| s["id"] == id),
        "created session missing from list"
    );

    // Get returns the snapshot; config echoes the request, history empty.
    let g = c.get(format!("{base}/api/sessions/{id}")).send().unwrap();
    assert_eq!(g.status().as_u16(), 200);
    let gv: serde_json::Value = g.json().unwrap();
    assert_eq!(gv["config"]["system"], "hermetic");
    assert_eq!(gv["config"]["approval_mode"], "gated");
    assert!(gv["messages"].as_array().unwrap().is_empty());
    assert!(gv["pending_command"].is_null());

    // Conversation is not implemented until M3.
    let pm = c
        .post(format!("{base}/api/sessions/{id}/messages"))
        .send()
        .unwrap();
    assert_eq!(pm.status().as_u16(), 501);

    // Delete -> gone.
    assert_eq!(
        c.delete(format!("{base}/api/sessions/{id}"))
            .send()
            .unwrap()
            .status()
            .as_u16(),
        200
    );
    assert_eq!(
        c.get(format!("{base}/api/sessions/{id}"))
            .send()
            .unwrap()
            .status()
            .as_u16(),
        404
    );

    // Unknown route -> 404.
    assert_eq!(
        c.get(format!("{base}/nope"))
            .send()
            .unwrap()
            .status()
            .as_u16(),
        404
    );

    let _ = child.kill();
    let _ = child.wait();
}
