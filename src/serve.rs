//! `ocha serve` — local HTTP server for remote conversation control.
//!
//! **M2 scope: lifecycle bookkeeping only.** Health, model listing,
//! session CRUD. No turn execution, no SSE, no approval gate yet
//! (`POST …/messages` returns `501`); those are M3/M4. The server is a
//! client of ocha's owned loop, never a replacement for it.
//!
//! Binds `127.0.0.1` only (design decision: localhost-only, no auth).
//! Built on `hyper`/`hyper-util` (already in the dep graph) — no
//! framework, per the §1.1 minimal-dependency policy.

use crate::{BackendConfig, Reminder, build_backend};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::net::TcpListener;

/// CLI-derived defaults for new sessions (decision: top-level args =
/// session defaults; `POST /api/sessions` overrides per session).
pub struct ServeDefaults {
    pub backend_cfg: BackendConfig,
    pub system: Option<String>,
    pub command_per_response: usize,
    #[allow(dead_code)] // applied to turns in M3
    pub reminders: Vec<Reminder>,
}

/// Per-session config we echo back. A superset is in design §3.1; M2
/// keeps the minimal representative subset.
#[derive(Clone, Serialize)]
struct SessionConfigOut {
    backend: String,
    model: Option<String>,
    system: Option<String>,
    command_per_response: usize,
    approval_mode: String,
}

#[derive(Serialize)]
struct SessionRecord {
    id: String,
    state: &'static str,
    created_at: String,
    config: SessionConfigOut,
    // Empty until M3 (no turns run yet).
    messages: Vec<crate::Message>,
}

#[derive(Deserialize, Default)]
struct CreateSessionReq {
    backend: Option<String>,
    model: Option<String>,
    system: Option<String>,
    command_per_response: Option<usize>,
    approval_mode: Option<String>,
}

struct AppState {
    sessions: Mutex<HashMap<String, SessionRecord>>,
    next_id: AtomicU64,
    defaults: ServeDefaults,
}

fn json(status: StatusCode, body: &serde_json::Value) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body.to_string())))
        .unwrap()
}

fn err(status: StatusCode, msg: &str) -> Response<Full<Bytes>> {
    json(status, &serde_json::json!({ "error": msg }))
}

fn snapshot(r: &SessionRecord) -> serde_json::Value {
    serde_json::json!({
        "id": r.id,
        "state": r.state,
        "created_at": r.created_at,
        "config": r.config,
        "messages": r.messages,
        "pending_command": serde_json::Value::Null,
    })
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

async fn handle(req: Request<Incoming>, state: Arc<AppState>) -> Response<Full<Bytes>> {
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
            let d = &state.defaults;
            let config = SessionConfigOut {
                backend: req.backend.unwrap_or_else(|| match d.backend_cfg.backend {
                    crate::BackendKind::Ollama => "ollama".into(),
                    crate::BackendKind::Claude => "claude".into(),
                    crate::BackendKind::ClaudeCli => "claude-cli".into(),
                }),
                model: req.model.or_else(|| d.backend_cfg.model.clone()),
                system: req.system.or_else(|| d.system.clone()),
                command_per_response: req.command_per_response.unwrap_or(d.command_per_response),
                // Serve default is the remote gate (design §1).
                approval_mode: req.approval_mode.unwrap_or_else(|| "gated".into()),
            };
            let n = state.next_id.fetch_add(1, Ordering::SeqCst);
            let id = format!("s_{n:x}");
            let record = SessionRecord {
                id: id.clone(),
                state: "idle",
                created_at: chrono::Local::now().to_rfc3339(),
                config,
                messages: Vec::new(),
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
                        "id": r.id, "state": r.state,
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

        (&Method::POST, ["api", "sessions", id, "messages"]) => {
            if !state.sessions.lock().unwrap().contains_key(*id) {
                return err(StatusCode::NOT_FOUND, "no such session");
            }
            err(
                StatusCode::NOT_IMPLEMENTED,
                "conversation not implemented until M3",
            )
        }

        _ => err(StatusCode::NOT_FOUND, "not found"),
    }
}

/// Run the server until the process is killed. Binds loopback only.
pub async fn run(defaults: ServeDefaults, port: u16) -> Result<(), Box<dyn std::error::Error>> {
    let state = Arc::new(AppState {
        sessions: Mutex::new(HashMap::new()),
        next_id: AtomicU64::new(1),
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
                async move { Ok::<_, std::convert::Infallible>(handle(req, state).await) }
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
