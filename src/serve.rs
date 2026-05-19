//! `ocha serve` — local HTTP server for remote conversation control.
//!
//! **M3 scope:** lifecycle (M2) **+ conversation over HTTP/SSE**. A
//! `POST …/messages` drives ocha's own `run_turn` in a background task
//! with an [`SseObserver`]; clients watch progress on
//! `GET …/events` (Server-Sent Events). Approval is still
//! [`AutoApprover`] — commands auto-execute exactly like the CLI; the
//! remote approve/deny gate is M4. The server is a *client* of ocha's
//! owned loop, never a replacement for it.
//!
//! Binds `127.0.0.1` only (localhost-only, no auth). Built on
//! `hyper`/`hyper-util` + a hand-rolled SSE stream (`futures_util`),
//! per the §1.1 minimal-dependency policy — no framework.

use crate::turn::{AutoApprover, CommandApprover, Decision, TurnObserver};
use crate::{
    BackendConfig, CommandRequest, Reminder, Role, RunTurnConfig, Session, build_backend, run_turn,
};
use async_trait::async_trait;
use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::convert::Infallible;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, oneshot};

/// Unified response body: JSON replies and the SSE stream both box into
/// this so one handler signature serves both.
type Body = BoxBody<Bytes, Infallible>;

/// One Server-Sent Event. `broadcast` requires `Clone`.
#[derive(Clone)]
struct SseMsg {
    event: &'static str,
    data: serde_json::Value,
}

#[derive(Clone, Copy, PartialEq)]
enum SessionState {
    Idle,
    Generating,
    AwaitingApproval,
}

impl SessionState {
    fn as_str(self) -> &'static str {
        match self {
            SessionState::Idle => "idle",
            SessionState::Generating => "generating",
            SessionState::AwaitingApproval => "awaiting_approval",
        }
    }
}

/// A command parked on the remote approval gate.
struct Pending {
    cmd_id: String,
    request: CommandRequest,
    responder: oneshot::Sender<Decision>,
}

/// CLI-derived defaults for new sessions (decision: top-level args =
/// session defaults; `POST /api/sessions` overrides per session).
pub struct ServeDefaults {
    pub backend_cfg: BackendConfig,
    pub system: Option<String>,
    pub command_per_response: usize,
    pub reminders: Vec<Reminder>,
}

/// Per-session config we echo back. A superset is in design §3.1; the
/// minimal representative subset is kept.
#[derive(Clone, Serialize)]
struct SessionConfigOut {
    backend: String,
    model: Option<String>,
    system: Option<String>,
    command_per_response: usize,
    approval_mode: String,
}

struct SessionRecord {
    id: String,
    created_at: String,
    config: SessionConfigOut,
    messages: Vec<crate::Message>,
    state: SessionState,
    /// Live event fan-out: every connected SSE client subscribes here.
    tx: broadcast::Sender<SseMsg>,
    /// Set while `state == AwaitingApproval`; the gate awaits its responder.
    pending: Option<Pending>,
    // Materials to run a turn (resolved from defaults + create request).
    backend_cfg: BackendConfig,
    system: Option<String>,
    command_per_response: usize,
    approval_mode: String,
}

#[derive(Deserialize, Default)]
struct CreateSessionReq {
    backend: Option<String>,
    model: Option<String>,
    system: Option<String>,
    command_per_response: Option<usize>,
    approval_mode: Option<String>,
}

#[derive(Deserialize)]
struct SendMessageReq {
    content: String,
}

#[derive(Deserialize, Default)]
struct DenyReq {
    reason: Option<String>,
}

struct AppState {
    sessions: Mutex<HashMap<String, SessionRecord>>,
    next_id: AtomicU64,
    next_cmd: AtomicU64,
    defaults: ServeDefaults,
}

/// `CommandApprover` that parks the turn until a remote approve/deny (or
/// a timeout) resolves it — the §4 design realized as a single `await`
/// inside ocha's own loop. Best-effort SSE broadcast; ocha stays the
/// single execution point.
struct RemoteApprover {
    state: Arc<AppState>,
    id: String,
    tx: broadcast::Sender<SseMsg>,
    timeout: Duration,
}

#[async_trait]
impl CommandApprover for RemoteApprover {
    async fn decide(&self, req: &CommandRequest) -> Decision {
        let n = self.state.next_cmd.fetch_add(1, Ordering::SeqCst);
        let cmd_id = format!("c_{n:x}");
        let (otx, orx) = oneshot::channel::<Decision>();

        {
            let mut map = self.state.sessions.lock().unwrap();
            let Some(r) = map.get_mut(&self.id) else {
                return Decision::Deny {
                    reason: "session gone".into(),
                };
            };
            r.state = SessionState::AwaitingApproval;
            r.pending = Some(Pending {
                cmd_id: cmd_id.clone(),
                request: req.clone(),
                responder: otx,
            });
        }
        let _ = self.tx.send(SseMsg {
            event: "command_pending",
            data: serde_json::json!({ "cmd_id": cmd_id, "request": req }),
        });
        let _ = self.tx.send(state_msg("awaiting_approval"));

        let decision = match tokio::time::timeout(self.timeout, orx).await {
            Ok(Ok(d)) => d,
            Ok(Err(_)) => Decision::Deny {
                reason: "approval channel closed".into(),
            },
            Err(_) => Decision::Deny {
                reason: "approval timed out".into(),
            },
        };

        // Resume: clear our pending entry (if still ours) and continue.
        {
            let mut map = self.state.sessions.lock().unwrap();
            if let Some(r) = map.get_mut(&self.id) {
                if r.pending.as_ref().map(|p| p.cmd_id == cmd_id) == Some(true) {
                    r.pending = None;
                }
                r.state = SessionState::Generating;
            }
        }
        let _ = self.tx.send(state_msg("generating"));
        decision
    }
}

/// `TurnObserver` that fans each callback out as an SSE event. Sends are
/// best-effort: `broadcast::Sender::send` erroring just means no client
/// is currently connected, which is fine.
struct SseObserver {
    tx: broadcast::Sender<SseMsg>,
}

impl SseObserver {
    fn emit(&self, event: &'static str, data: serde_json::Value) {
        let _ = self.tx.send(SseMsg { event, data });
    }
}

impl TurnObserver for SseObserver {
    fn token(&self, frag: &str) {
        self.emit("token", serde_json::json!({ "text": frag }));
    }
    fn reminder(&self, text: &str) {
        self.emit("reminder", serde_json::json!({ "text": text }));
    }
    fn response_end(&self) {
        // The assistant `message` + `turn_complete` are emitted by the
        // orchestrator once the loop returns.
    }
    fn command_payload(&self, _json: &str) {}
    fn command_executing(&self, binary: &str, args: &[String]) {
        // M3 auto-executes (no gate yet); surface what is running.
        self.emit(
            "command_executing",
            serde_json::json!({ "binary": binary, "args": args }),
        );
    }
    fn command_stdout(&self, _s: &str) {}
    fn command_stderr(&self, _s: &str) {}
    fn command_result(&self, json: &str) {
        let data = serde_json::from_str(json).unwrap_or(serde_json::Value::Null);
        self.emit("command_result", data);
    }
}

fn json(status: StatusCode, body: &serde_json::Value) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body.to_string())).boxed())
        .unwrap()
}

fn err(status: StatusCode, msg: &str) -> Response<Body> {
    json(status, &serde_json::json!({ "error": msg }))
}

fn snapshot(r: &SessionRecord) -> serde_json::Value {
    let pending = r
        .pending
        .as_ref()
        .map(|p| serde_json::json!({ "cmd_id": p.cmd_id, "request": p.request }));
    serde_json::json!({
        "id": r.id,
        "state": r.state.as_str(),
        "created_at": r.created_at,
        "config": r.config,
        "messages": r.messages,
        "pending_command": pending,
    })
}

fn backend_name(k: crate::BackendKind) -> &'static str {
    match k {
        crate::BackendKind::Ollama => "ollama",
        crate::BackendKind::Claude => "claude",
        crate::BackendKind::ClaudeCli => "claude-cli",
    }
}

/// Resolve a requested backend name to a concrete `BackendConfig`,
/// falling back to the CLI defaults when unset/unknown.
fn resolve_backend(state: &AppState, name: Option<&str>) -> BackendConfig {
    use crate::BackendKind::*;
    let mut cfg = state.defaults.backend_cfg.clone();
    if let Some(n) = name {
        cfg.backend = match n {
            "ollama" => Ollama,
            "claude" => Claude,
            "claude-cli" => ClaudeCli,
            _ => cfg.backend,
        };
    }
    cfg
}

/// Run one turn in a detached task, streaming progress over SSE. ocha's
/// `run_turn` stays the single owned loop; this only feeds it.
fn spawn_turn(state: Arc<AppState>, id: String, content: String) {
    tokio::spawn(async move {
        // Snapshot the per-session run materials under the lock.
        let (backend_cfg, system, cpr, tx, history, approval_mode) = {
            let map = state.sessions.lock().unwrap();
            let Some(r) = map.get(&id) else { return };
            (
                r.backend_cfg.clone(),
                r.system.clone(),
                r.command_per_response,
                r.tx.clone(),
                r.messages.clone(),
                r.approval_mode.clone(),
            )
        };
        let is_new = history.is_empty();
        let prev_len = history.len();

        let backend = match build_backend(&backend_cfg, reqwest::Client::new()) {
            Ok(b) => b,
            Err(e) => {
                set_idle(&state, &id);
                let _ = tx.send(SseMsg {
                    event: "error",
                    data: serde_json::json!({ "message": e.to_string() }),
                });
                let _ = tx.send(state_msg("idle"));
                return;
            }
        };

        let mut session = Session { messages: history };
        let observer = SseObserver { tx: tx.clone() };
        // `auto` = behave exactly like the CLI; `gated` (serve default)
        // = the remote approve/deny gate. Timeout configurable for tests.
        let approver: Box<dyn CommandApprover> = if approval_mode == "auto" {
            Box::new(AutoApprover)
        } else {
            let secs = std::env::var("OCHA_APPROVAL_TIMEOUT_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(600);
            Box::new(RemoteApprover {
                state: state.clone(),
                id: id.clone(),
                tx: tx.clone(),
                timeout: Duration::from_secs(secs),
            })
        };
        let cfg = RunTurnConfig {
            backend: &*backend,
            system: system.as_deref(),
            initial_prompt: &content,
            session: &mut session,
            reminders: &state.defaults.reminders,
            command_per_response: cpr,
            is_new_session: is_new,
            log_path: None,
            observer: &observer,
            approver: &*approver,
        };

        let result = run_turn(cfg).await;
        let final_msgs = session.messages;

        {
            let mut map = state.sessions.lock().unwrap();
            if let Some(r) = map.get_mut(&id) {
                r.messages = final_msgs.clone();
                r.state = SessionState::Idle;
            }
        }

        match result {
            Ok(()) => {
                for m in final_msgs.iter().skip(prev_len) {
                    if m.role != Role::User {
                        let _ = tx.send(SseMsg {
                            event: "message",
                            data: serde_json::to_value(m).unwrap_or(serde_json::Value::Null),
                        });
                    }
                }
                let _ = tx.send(SseMsg {
                    event: "turn_complete",
                    data: serde_json::json!({}),
                });
            }
            Err(e) => {
                let _ = tx.send(SseMsg {
                    event: "error",
                    data: serde_json::json!({ "message": e.to_string() }),
                });
            }
        }
        let _ = tx.send(state_msg("idle"));
    });
}

fn state_msg(s: &'static str) -> SseMsg {
    SseMsg {
        event: "state",
        data: serde_json::json!({ "state": s }),
    }
}

fn set_idle(state: &AppState, id: &str) {
    if let Some(r) = state.sessions.lock().unwrap().get_mut(id) {
        r.state = SessionState::Idle;
    }
}

fn sse_frame(msg: &SseMsg) -> Bytes {
    Bytes::from(format!("event: {}\ndata: {}\n\n", msg.event, msg.data))
}

/// Build the `text/event-stream` body: a synthetic `snapshot` frame
/// first, then live broadcast events, with a ~15s `ping` keep-alive.
/// Hand-rolled via `futures_util::stream::unfold` — no extra deps.
fn sse_response(
    snapshot_json: serde_json::Value,
    rx: broadcast::Receiver<SseMsg>,
) -> Response<Body> {
    let first = format!("event: snapshot\ndata: {snapshot_json}\n\n");
    let stream = futures_util::stream::unfold((Some(first), rx), |(pending, mut rx)| async move {
        if let Some(s) = pending {
            let f: Result<Frame<Bytes>, Infallible> = Ok(Frame::data(Bytes::from(s)));
            return Some((f, (None, rx)));
        }
        loop {
            tokio::select! {
                r = rx.recv() => match r {
                    Ok(msg) => {
                        let f = Ok(Frame::data(sse_frame(&msg)));
                        return Some((f, (None, rx)));
                    }
                    // Slow consumer dropped events: keep the stream
                    // alive (snapshot already reconciled state).
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return None,
                },
                _ = tokio::time::sleep(Duration::from_secs(15)) => {
                    let f = Ok(Frame::data(Bytes::from(
                        "event: ping\ndata: {}\n\n".to_string(),
                    )));
                    return Some((f, (None, rx)));
                }
            }
        }
    });
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(StreamBody::new(stream).boxed())
        .unwrap()
}

async fn handle(req: Request<Incoming>, state: Arc<AppState>) -> Response<Body> {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or("").to_string();
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();

    match (&method, segments.as_slice()) {
        (&Method::GET, ["api", "health"]) => json(
            StatusCode::OK,
            &serde_json::json!({ "ok": true, "version": env!("CARGO_PKG_VERSION") }),
        ),

        (&Method::GET, ["api", "models"]) => {
            let backend = query
                .split('&')
                .find_map(|kv| kv.strip_prefix("backend="))
                .map(|s| s.to_string());
            let cfg = resolve_backend(&state, backend.as_deref());
            // Resolve the (non-Send) build Result *before* any await so
            // the connection future stays Send.
            let backend = match build_backend(&cfg, reqwest::Client::new()) {
                Ok(b) => b,
                Err(e) => return err(StatusCode::BAD_REQUEST, &e.to_string()),
            };
            match backend.list_models().await {
                Ok(models) => {
                    let list: Vec<_> = models
                        .into_iter()
                        .map(|m| serde_json::json!({ "name": m.name, "detail": m.detail }))
                        .collect();
                    json(StatusCode::OK, &serde_json::json!({ "models": list }))
                }
                Err(e) => err(StatusCode::BAD_GATEWAY, &e.to_string()),
            }
        }

        (&Method::POST, ["api", "sessions"]) => {
            let body = req.into_body().collect().await.map(|b| b.to_bytes());
            let req: CreateSessionReq = match body {
                Ok(b) if b.is_empty() => CreateSessionReq::default(),
                Ok(b) => match serde_json::from_slice(&b) {
                    Ok(v) => v,
                    Err(e) => return err(StatusCode::BAD_REQUEST, &format!("invalid JSON: {e}")),
                },
                Err(e) => return err(StatusCode::BAD_REQUEST, &e.to_string()),
            };
            let mut cfg = resolve_backend(&state, req.backend.as_deref());
            cfg.model = req.model.clone().or(cfg.model);
            let d = &state.defaults;
            let config = SessionConfigOut {
                backend: backend_name(cfg.backend).to_string(),
                model: cfg.model.clone(),
                system: req.system.clone().or_else(|| d.system.clone()),
                command_per_response: req.command_per_response.unwrap_or(d.command_per_response),
                approval_mode: req.approval_mode.unwrap_or_else(|| "gated".into()),
            };
            let n = state.next_id.fetch_add(1, Ordering::SeqCst);
            let id = format!("s_{n:x}");
            let (tx, _) = broadcast::channel(1024);
            let record = SessionRecord {
                id: id.clone(),
                created_at: chrono::Local::now().to_rfc3339(),
                messages: Vec::new(),
                state: SessionState::Idle,
                tx,
                pending: None,
                backend_cfg: cfg,
                system: config.system.clone(),
                command_per_response: config.command_per_response,
                approval_mode: config.approval_mode.clone(),
                config,
            };
            let resp = serde_json::json!({
                "id": id, "state": "idle", "created_at": record.created_at,
            });
            state.sessions.lock().unwrap().insert(id, record);
            json(StatusCode::CREATED, &resp)
        }

        (&Method::GET, ["api", "sessions"]) => {
            let map = state.sessions.lock().unwrap();
            let list: Vec<_> = map
                .values()
                .map(|r| {
                    serde_json::json!({
                        "id": r.id, "state": r.state.as_str(),
                        "messages": r.messages.len(), "config": r.config,
                    })
                })
                .collect();
            json(StatusCode::OK, &serde_json::json!(list))
        }

        (&Method::GET, ["api", "sessions", id]) => {
            let map = state.sessions.lock().unwrap();
            match map.get(*id) {
                Some(r) => json(StatusCode::OK, &snapshot(r)),
                None => err(StatusCode::NOT_FOUND, "no such session"),
            }
        }

        (&Method::DELETE, ["api", "sessions", id]) => {
            let removed = state.sessions.lock().unwrap().remove(*id).is_some();
            if removed {
                json(StatusCode::OK, &serde_json::json!({ "deleted": true }))
            } else {
                err(StatusCode::NOT_FOUND, "no such session")
            }
        }

        (&Method::GET, ["api", "sessions", id, "events"]) => {
            let map = state.sessions.lock().unwrap();
            match map.get(*id) {
                Some(r) => {
                    let rx = r.tx.subscribe();
                    let snap = snapshot(r);
                    drop(map);
                    sse_response(snap, rx)
                }
                None => err(StatusCode::NOT_FOUND, "no such session"),
            }
        }

        (&Method::POST, ["api", "sessions", id, "messages"]) => {
            let body = req.into_body().collect().await.map(|b| b.to_bytes());
            let msg: SendMessageReq = match body {
                Ok(b) => match serde_json::from_slice(&b) {
                    Ok(v) => v,
                    Err(e) => return err(StatusCode::BAD_REQUEST, &format!("invalid JSON: {e}")),
                },
                Err(e) => return err(StatusCode::BAD_REQUEST, &e.to_string()),
            };

            let id = id.to_string();
            {
                let mut map = state.sessions.lock().unwrap();
                let Some(r) = map.get_mut(&id) else {
                    return err(StatusCode::NOT_FOUND, "no such session");
                };
                if r.state != SessionState::Idle {
                    return err(StatusCode::CONFLICT, "a turn is already in progress");
                }
                r.state = SessionState::Generating;
                let _ = r.tx.send(state_msg("generating"));
            }
            spawn_turn(state.clone(), id, msg.content);
            json(
                StatusCode::ACCEPTED,
                &serde_json::json!({ "state": "generating" }),
            )
        }

        (&Method::POST, ["api", "sessions", id, "commands", cmd_id, action])
            if *action == "approve" || *action == "deny" =>
        {
            let id = id.to_string();
            let cmd_id = cmd_id.to_string();
            let decision = if *action == "approve" {
                Decision::Approve
            } else {
                let body = req.into_body().collect().await.map(|b| b.to_bytes());
                let reason = match body {
                    Ok(b) if !b.is_empty() => serde_json::from_slice::<DenyReq>(&b)
                        .ok()
                        .and_then(|d| d.reason)
                        .unwrap_or_else(|| "denied by operator".into()),
                    _ => "denied by operator".into(),
                };
                Decision::Deny { reason }
            };
            // cmd_id must match the *currently pending* command, else
            // 404 (stale / double-submit / multi-tab race guard).
            let taken = {
                let mut map = state.sessions.lock().unwrap();
                match map.get_mut(&id) {
                    Some(r) if r.pending.as_ref().map(|p| p.cmd_id == cmd_id) == Some(true) => {
                        r.pending.take()
                    }
                    _ => None,
                }
            };
            match taken {
                Some(p) => {
                    let _ = p.responder.send(decision);
                    json(
                        StatusCode::OK,
                        &serde_json::json!({ "state": "generating" }),
                    )
                }
                None => err(StatusCode::NOT_FOUND, "no such pending command"),
            }
        }

        (&Method::POST, ["api", "sessions", id, "cancel"]) => {
            // M4: cancels a *pending approval* (denies it so the turn
            // unwinds and completes). Full mid-stream stream abort is
            // future work (design §5).
            let taken = {
                let mut map = state.sessions.lock().unwrap();
                match map.get_mut(*id) {
                    Some(r) => r.pending.take(),
                    None => return err(StatusCode::NOT_FOUND, "no such session"),
                }
            };
            if let Some(p) = taken {
                let _ = p.responder.send(Decision::Deny {
                    reason: "canceled by operator".into(),
                });
            }
            json(StatusCode::OK, &serde_json::json!({ "canceled": true }))
        }

        _ => err(StatusCode::NOT_FOUND, "not found"),
    }
}

/// Run the server until the process is killed. Binds loopback only.
pub async fn run(defaults: ServeDefaults, port: u16) -> Result<(), Box<dyn std::error::Error>> {
    let state = Arc::new(AppState {
        sessions: Mutex::new(HashMap::new()),
        next_id: AtomicU64::new(1),
        next_cmd: AtomicU64::new(1),
        defaults,
    });

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await?;
    let addr = listener.local_addr()?;
    println!("ocha serve listening on http://{addr}");
    println!("(localhost-only, no auth — see docs/web-ui-remote-control-design.md §7)");

    loop {
        let (stream, _) = listener.accept().await?;
        let io = TokioIo::new(stream);
        let state = state.clone();
        tokio::spawn(async move {
            let svc = service_fn(move |req| {
                let state = state.clone();
                async move { Ok::<_, Infallible>(handle(req, state).await) }
            });
            if let Err(e) = auto::Builder::new(TokioExecutor::new())
                .serve_connection(io, svc)
                .await
            {
                eprintln!("serve connection error: {e}");
            }
        });
    }
}
