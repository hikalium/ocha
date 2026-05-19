//! Hermetic integration tests for `ocha serve`.
//!
//! Spawns the real binary with the in-process mock backend
//! (`OCHA_MOCK_BACKEND=1`) on an OS-assigned port — no network, no real
//! model, deterministic. M2: lifecycle + loopback bind. M3: conversation
//! over HTTP/SSE.

use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// Start `ocha serve --port 0` (mock backend, plus any extra env) and
/// return the child plus the base URL it printed once bound.
#[allow(deprecated)]
// assert_cmd::cargo::cargo_bin: fine without a custom build-dir
// The child is returned to the caller, which kill()s + wait()s it; the
// lint can't see across the helper boundary.
#[allow(clippy::zombie_processes)]
fn spawn_server_env(extra: &[(&str, &str)]) -> (Child, String) {
    let bin = assert_cmd::cargo::cargo_bin("ocha");
    let mut cmd = Command::new(bin);
    cmd.arg("serve")
        .arg("--port")
        .arg("0")
        .env("OCHA_MOCK_BACKEND", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in extra {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("spawn ocha serve");

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
            let base = line[idx..].trim().to_string();
            // Readiness probe: don't return until the server actually
            // answers, so tests are deterministic under parallel load.
            let c = reqwest::blocking::Client::new();
            for _ in 0..100 {
                if c.get(format!("{base}/api/health")).send().is_ok() {
                    return (child, base);
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = child.kill();
            let _ = child.wait();
            panic!("ocha serve never became ready at {base}");
        }
    }
}

fn spawn_server() -> (Child, String) {
    spawn_server_env(&[])
}

/// Parse an `event:`/`data:` SSE frame into `(event, data)`.
fn parse_frame(s: &str) -> (String, String) {
    let mut ev = String::new();
    let mut data = String::new();
    for line in s.lines() {
        if let Some(x) = line.strip_prefix("event: ") {
            ev = x.trim().to_string();
        }
        if let Some(x) = line.strip_prefix("data: ") {
            data = x.trim().to_string();
        }
    }
    (ev, data)
}

fn find2(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\n\n")
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

    // Messages to a missing session -> 404 (conversation flow itself
    // is covered by serve_conversation_sse_hermetic).
    let pm = c
        .post(format!("{base}/api/sessions/s_doesnotexist/messages"))
        .body(r#"{"content":"x"}"#)
        .send()
        .unwrap();
    assert_eq!(pm.status().as_u16(), 404);

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

/// M3 acceptance: a full conversation over HTTP + SSE, hermetic.
#[test]
fn serve_conversation_sse_hermetic() {
    let script = "alpha beta gamma delta";
    let (mut child, base) = spawn_server_env(&[("OCHA_MOCK_REPLY", script)]);
    let c = reqwest::blocking::Client::new();

    let id = {
        let v: serde_json::Value = c
            .post(format!("{base}/api/sessions"))
            .body("{}")
            .send()
            .unwrap()
            .json()
            .unwrap();
        v["id"].as_str().unwrap().to_string()
    };

    // Subscribe to SSE in a thread; signal `ready` after the snapshot
    // frame (subscription is then active), collect until `state:idle`
    // following `turn_complete`.
    let evurl = format!("{base}/api/sessions/{id}/events");
    let (ready_tx, ready_rx) = mpsc::channel::<()>();
    let (done_tx, done_rx) = mpsc::channel::<Vec<(String, String)>>();
    let t = std::thread::spawn(move || {
        let cc = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .unwrap();
        let mut resp = cc.get(&evurl).send().unwrap();
        let mut buf: Vec<u8> = Vec::new();
        let mut tmp = [0u8; 1024];
        let mut events: Vec<(String, String)> = Vec::new();
        let mut ready = false;
        let mut saw_complete = false;
        loop {
            let n = match resp.read(&mut tmp) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            buf.extend_from_slice(&tmp[..n]);
            while let Some(pos) = find2(&buf) {
                let frame: Vec<u8> = buf.drain(..pos + 2).collect();
                let (ev, data) = parse_frame(&String::from_utf8_lossy(&frame));
                if ev.is_empty() {
                    continue;
                }
                if ev == "snapshot" && !ready {
                    ready = true;
                    let _ = ready_tx.send(());
                }
                if ev == "turn_complete" {
                    saw_complete = true;
                }
                let is_idle = ev == "state" && data.contains("idle");
                events.push((ev, data));
                if saw_complete && is_idle {
                    let _ = done_tx.send(events.clone());
                    return;
                }
            }
        }
        let _ = done_tx.send(events);
    });

    ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("SSE never connected/snapshot");

    let post = c
        .post(format!("{base}/api/sessions/{id}/messages"))
        .body(r#"{"content":"hi"}"#)
        .send()
        .unwrap();
    assert_eq!(post.status().as_u16(), 202);

    let events = done_rx
        .recv_timeout(Duration::from_secs(15))
        .expect("turn did not complete");
    t.join().ok();

    let kinds: Vec<&str> = events.iter().map(|(e, _)| e.as_str()).collect();
    assert_eq!(
        kinds.first(),
        Some(&"snapshot"),
        "first event not snapshot: {kinds:?}"
    );
    let pos = |k: &str| kinds.iter().position(|x| *x == k);
    let gen_pos = pos("state").expect("no state event");
    let tok = pos("token").expect("no token event");
    let msg = pos("message").expect("no message event");
    let tc = pos("turn_complete").expect("no turn_complete");
    assert!(
        0 < gen_pos && gen_pos < tok,
        "state:generating not before tokens: {kinds:?}"
    );
    assert!(
        tok < msg && msg < tc,
        "ordering token<message<turn_complete broken: {kinds:?}"
    );
    assert!(
        kinds.last() == Some(&"state"),
        "stream didn't end on state:idle: {kinds:?}"
    );

    // Assembled token text equals the mock script exactly.
    let text: String = events
        .iter()
        .filter(|(e, _)| e == "token")
        .map(|(_, d)| {
            serde_json::from_str::<serde_json::Value>(d).unwrap()["text"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(text, script);

    // History persisted: user prompt + assistant reply.
    let g: serde_json::Value = c
        .get(format!("{base}/api/sessions/{id}"))
        .send()
        .unwrap()
        .json()
        .unwrap();
    let m = g["messages"].as_array().unwrap();
    assert_eq!(m.len(), 2);
    assert_eq!(m[0]["role"], "user");
    assert_eq!(m[0]["content"], "hi");
    assert_eq!(m[1]["role"], "assistant");
    assert_eq!(m[1]["content"], script);
    assert_eq!(g["state"], "idle");

    // Reconnect mid-idle: the snapshot carries the full history.
    let mut r2 = c
        .get(format!("{base}/api/sessions/{id}/events"))
        .send()
        .unwrap();
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 1024];
    let snap = loop {
        let n = r2.read(&mut tmp).unwrap();
        assert!(n > 0, "no snapshot on reconnect");
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find2(&buf) {
            break parse_frame(&String::from_utf8_lossy(&buf[..pos + 2]));
        }
    };
    assert_eq!(snap.0, "snapshot");
    let sv: serde_json::Value = serde_json::from_str(&snap.1).unwrap();
    assert_eq!(sv["messages"].as_array().unwrap().len(), 2);
    assert_eq!(sv["state"], "idle");
    drop(r2);

    let _ = child.kill();
    let _ = child.wait();
}

// ---- M4: remote approval gate ----

const M4_SCRIPT: &str = "running\n!!!OCHA_RUN_CMD{\"binary\":\"echo\",\"args\":[\"m4ok\"],\"timeout\":5,\"description\":\"e\"}<<<NEXT>>>all finished";

fn create_session(c: &reqwest::blocking::Client, base: &str) -> String {
    let v: serde_json::Value = c
        .post(format!("{base}/api/sessions"))
        .body("{}")
        .send()
        .unwrap()
        .json()
        .unwrap();
    v["id"].as_str().unwrap().to_string()
}

/// Poll GET until `pending_command` appears; return its cmd_id.
fn poll_pending(
    c: &reqwest::blocking::Client,
    base: &str,
    id: &str,
) -> (String, serde_json::Value) {
    for _ in 0..100 {
        let g: serde_json::Value = c
            .get(format!("{base}/api/sessions/{id}"))
            .send()
            .unwrap()
            .json()
            .unwrap();
        if !g["pending_command"].is_null() {
            assert_eq!(g["state"], "awaiting_approval");
            return (
                g["pending_command"]["cmd_id"].as_str().unwrap().to_string(),
                g,
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("command_pending never appeared");
}

/// Poll GET until the turn returns to idle; return the final snapshot.
fn poll_idle(c: &reqwest::blocking::Client, base: &str, id: &str) -> serde_json::Value {
    for _ in 0..200 {
        let g: serde_json::Value = c
            .get(format!("{base}/api/sessions/{id}"))
            .send()
            .unwrap()
            .json()
            .unwrap();
        if g["state"] == "idle" && g["messages"].as_array().unwrap().len() >= 3 {
            return g;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("turn never returned to idle");
}

fn tool_msg(g: &serde_json::Value) -> String {
    g["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .expect("no tool message")["content"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn serve_approval_approve_hermetic() {
    let (mut child, base) = spawn_server_env(&[
        ("OCHA_MOCK_REPLY", M4_SCRIPT),
        ("OCHA_APPROVAL_TIMEOUT_SECS", "30"),
    ]);
    let c = reqwest::blocking::Client::new();
    let id = create_session(&c, &base);

    let post = c
        .post(format!("{base}/api/sessions/{id}/messages"))
        .body(r#"{"content":"go"}"#)
        .send()
        .unwrap();
    assert_eq!(post.status().as_u16(), 202);

    let (cmd_id, snap) = poll_pending(&c, &base, &id);
    assert_eq!(snap["pending_command"]["request"]["binary"], "echo");

    // Stale / unknown cmd_id -> 404 while a real one is pending.
    let bogus = c
        .post(format!(
            "{base}/api/sessions/{id}/commands/c_deadbeef/approve"
        ))
        .send()
        .unwrap();
    assert_eq!(bogus.status().as_u16(), 404);

    // Approve the real one.
    let ok = c
        .post(format!(
            "{base}/api/sessions/{id}/commands/{cmd_id}/approve"
        ))
        .send()
        .unwrap();
    assert_eq!(ok.status().as_u16(), 200);

    let g = poll_idle(&c, &base, &id);
    let tool = tool_msg(&g);
    assert!(
        tool.contains("\"status\":0"),
        "tool result not status 0: {tool}"
    );
    assert!(tool.contains("m4ok"), "command stdout missing: {tool}");
    // Re-approving the now-consumed cmd_id -> 404.
    let again = c
        .post(format!(
            "{base}/api/sessions/{id}/commands/{cmd_id}/approve"
        ))
        .send()
        .unwrap();
    assert_eq!(again.status().as_u16(), 404);

    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn serve_approval_deny_hermetic() {
    let (mut child, base) = spawn_server_env(&[
        ("OCHA_MOCK_REPLY", M4_SCRIPT),
        ("OCHA_APPROVAL_TIMEOUT_SECS", "30"),
    ]);
    let c = reqwest::blocking::Client::new();
    let id = create_session(&c, &base);

    c.post(format!("{base}/api/sessions/{id}/messages"))
        .body(r#"{"content":"go"}"#)
        .send()
        .unwrap();
    let (cmd_id, _) = poll_pending(&c, &base, &id);

    let d = c
        .post(format!("{base}/api/sessions/{id}/commands/{cmd_id}/deny"))
        .body(r#"{"reason":"no network access"}"#)
        .send()
        .unwrap();
    assert_eq!(d.status().as_u16(), 200);

    let g = poll_idle(&c, &base, &id);
    let tool = tool_msg(&g);
    assert!(
        tool.contains("denied by operator") && tool.contains("no network access"),
        "deny reason not fed back: {tool}"
    );

    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn serve_approval_timeout_hermetic() {
    let (mut child, base) = spawn_server_env(&[
        ("OCHA_MOCK_REPLY", M4_SCRIPT),
        ("OCHA_APPROVAL_TIMEOUT_SECS", "1"),
    ]);
    let c = reqwest::blocking::Client::new();
    let id = create_session(&c, &base);

    c.post(format!("{base}/api/sessions/{id}/messages"))
        .body(r#"{"content":"go"}"#)
        .send()
        .unwrap();
    poll_pending(&c, &base, &id);

    // Do nothing; the 1s gate auto-denies and the turn still completes.
    let g = poll_idle(&c, &base, &id);
    let tool = tool_msg(&g);
    assert!(
        tool.contains("approval timed out"),
        "auto-deny reason missing: {tool}"
    );
    assert_eq!(g["state"], "idle");

    let _ = child.kill();
    let _ = child.wait();
}

// ---- M5: embedded web UI ----

#[test]
fn serve_index_ui_hermetic() {
    let (mut child, base) = spawn_server();
    let c = reqwest::blocking::Client::new();

    let r = c.get(format!("{base}/")).send().unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert!(
        r.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/html"),
        "GET / is not text/html"
    );
    let body = r.text().unwrap();

    // App is actually there and wired to the API.
    assert!(body.contains("<title>ocha"), "missing title");
    assert!(body.contains("EventSource"), "UI doesn't use SSE");
    assert!(body.contains("/api/sessions"), "UI not wired to the API");

    // §1.1: zero third-party origin — no remote scripts/styles/fonts/CDN.
    for needle in [
        "http://",
        "https://",
        "//cdn",
        "unpkg",
        "googleapis",
        "jsdelivr",
    ] {
        assert!(
            !body.contains(needle),
            "embedded UI references an external origin: {needle}"
        );
    }

    let _ = child.kill();
    let _ = child.wait();
}
