# Web UI for remote conversation control + remote command-approval gate

**Date:** 2026-05-19
**Status:** **Implemented (v1).** Milestones M1–M5 landed on `main`
(see §11); this doc remains the authoritative spec + rationale + the §12
dependency-audit gate for future serve work.
**Related:** [`docs/llm-interface-layering-and-common-abstraction.md`](llm-interface-layering-and-common-abstraction.md)
(why ocha owns its loop and the human approval gate); `GEMINI.md`
("Architecture Decisions").

---

## 1. Goal & scope

Add an `ocha serve` mode that exposes ocha's existing turn loop over a
small local HTTP API, plus a single-page web UI, so a conversation can be
driven **remotely**: send prompts, watch tokens stream, manage multiple
sessions, switch backend/model — and crucially **approve or deny each
`!!!OCHA_RUN_CMD` command before it executes**.

This is the natural realization of the project thesis. The companion
design note (§2, §4) records *why* a vendor agent SDK's remote-approval
path is unreliable (it hangs / silently bypasses the gate) and concludes
"the app should own the loop and the human gate, built on the most
primitive stable API." ocha already owns that loop in `run_turn`. The
remote gate here is therefore **a single `await` point inside ocha's own
loop** — not a vendor control protocol — which is exactly the robust
design §4 advocates.

### In scope
- New `ocha serve` subcommand (HTTP + streaming), localhost-only.
- Multi-session conversation management over the API.
- Token streaming to the browser.
- **Per-command remote approval gate** (approve / deny-with-reason).
- Reuse of the existing `Backend` trait, `Session`/`Message` model,
  reminders, logging, and the `!!!OCHA_RUN_CMD` protocol unchanged.

### Out of scope (non-goals)
- Authentication / multi-user / TLS (localhost-only by decision; remote
  access is the operator's problem via SSH tunnel or reverse proxy).
- Changing CLI behavior: the existing CLI keeps **auto-executing**
  commands; the approval gate is a `serve`-mode policy, injected through
  the same seam.
- Editing/branching past messages, RAG, attachments (future work).
- Live backend/model switch mid-session and "regenerate last turn"
  (post-v1; v1 config is fixed at session creation).
- Persisting server state beyond the existing optional session JSON;
  no restore-on-restart in v1.

### Decisions taken

Resolved 2026-05-19:
- **Remote approval gate: yes.** `serve` mode pauses every command and
  waits for an explicit human decision.
- **Exposure: localhost-only, no auth.** Binds `127.0.0.1` by default.

Resolved 2026-05-20:
- **HTTP layer: `hyper`/`hyper-util`.** Already in the graph via
  `reqwest` → no new external dep (per §1.1). Not `axum`; hand-rolled
  only as fallback.
- **Persistence: opt-in, no restore.** In-memory sessions; if created
  with `persist_path`, history is written like CLI `-S`. Server restart
  loses live state (documented limitation, not solved in v1).
- **v1 control scope: minimal.** Send prompt, approve/deny, cancel,
  session create/list/get/delete. **No** history mutation, **no** live
  backend/model switch, **no** regenerate — explicitly deferred.
- **Reconnect: message-level snapshot.** No token-level replay buffer.
- **Approval timeout: auto-deny after 600 s (configurable), continue.**
  Never an indefinite hang; the model receives the denied result and the
  turn proceeds.

Resolved 2026-05-20 (final, pre-implementation):
- **Default port `8765`, print URL.** Fixed `127.0.0.1:8765`, override
  with `--port`; print `http://127.0.0.1:8765` on startup.
- **Multi-tab: broadcast to all, `cmd_id`-guarded.** Every SSE
  subscriber sees the same stream; any tab may approve/deny; a stale or
  double decision (wrong/!pending `cmd_id`) → `404`. No controller role.
- **CLI args = session defaults.** `ocha serve --backend … -m … --system
  … -r …` set the default config for new sessions; `POST /api/sessions`
  overrides per session. (`approval_mode` still defaults to `gated` in
  serve regardless of CLI.)
- **Test backend: a test-only mock `Backend`.** Serve milestones M2–M4
  are verified with a small in-crate scriptable mock (hermetic, no
  network) — not eevee-gated. Real-backend coverage stays the existing
  e2e suite.
- **Process: doc committed now; push per milestone.** This doc lands on
  `main` as its own commit; M1–M5 are then implemented as minimal-unit
  commits, pushed to `origin/main` after each milestone's acceptance
  check passes (main stays green).

---

## 1.1 Dependency policy (hard constraint)

ocha is deliberately lightweight. This feature must not change that. The
policy below is binding for this work and for the project generally:

- **No new runtime external dependencies** unless **explicitly
  approved**, by name, in advance. Prefer the standard library and
  crates already in the dependency graph. Notably, `tokio` is already a
  direct dependency and `hyper`/`hyper-util`/`tower`/`tower-http` are
  already present **transitively via `reqwest`** — a server built on
  those adds no new *external* surface to vet.
- **Minimal build-time / dev dependencies**, same approval rule. Today's
  dev deps are only `assert_cmd` + `tempfile`; keep it that way.
- **Web UI: vanilla JS, hard requirement.** No framework, no npm, no
  bundler, no transpiler, **no build step**, and **zero runtime
  third-party JS** (no CDN scripts, no fonts, no web components libs).
  Plain HTML + CSS + `EventSource`/`fetch`, embedded in the binary via
  `include_str!`. If a thing cannot be done in readable vanilla JS, it is
  out of scope, not a reason to add a dependency.
- **Approval is explicit and recorded.** Any proposed new dependency
  (runtime or dev) must be raised as its own decision with a one-line
  justification and added to an "approved dependencies" list before use;
  silent additions are not allowed. "It's only transitive" is not an
  exemption for a *direct* dependency.
- **Default answer is "no".** When in doubt, write the code by hand
  against std/tokio rather than pull a crate. Lines of our own simple
  code are cheaper to own than an external supply-chain + API surface.

This is consistent with the companion design note's whole thesis: build
on the most primitive stable layer and own the code, rather than depend
on fragile upper layers.

---

## 2. System architecture

### 2.1 Where this plugs into existing code

Today (`src/main.rs`):

```
run_turn(RunTurnConfig)               // ocha's owned agent loop
  ├─ push user Message into Session
  ├─ apply_reminders()
  ├─ backend.chat(system, msgs, &mut sink)   // sink: print!() to stdout
  ├─ extract_command(response)
  │     └─ Some(cmd) -> execute_command(req) -> feed CommandResult back
  │                     (auto, immediate)            (Role::Tool message)
  └─ loop until plain-text response (or command_per_response hit)
```

The loop already centralizes everything we need. Only **two seams** must
be abstracted so the same loop can be driven by a browser instead of a
terminal — both are behavior-preserving for the CLI:

1. **Output seam — `TurnObserver`.** `run_turn` currently hard-codes
   `print!`/`println!` for tokens and for the `[Reminder] [Payload]
   [Executing] [Result]` side-channel. Replace these with calls on a
   `TurnObserver` trait. CLI supplies a `StdoutObserver` (identical
   output to today); the server supplies an `SseObserver` that turns each
   call into a structured SSE event (§4).

2. **Approval seam — `CommandApprover`.** Between `extract_command()` and
   `execute_command()`, insert an `async` approval call. CLI supplies an
   `AutoApprover` (always approve — today's behavior, unchanged). The
   server supplies a `RemoteApprover` that parks the turn on a
   `oneshot` channel until an HTTP approve/deny call resolves it.

```rust
// New trait sketch (design only)
#[async_trait]
trait CommandApprover {
    async fn decide(&self, sess: &SessionId, cmd: &CommandRequest)
        -> Decision;            // Approve | Deny { reason: String }
}

#[async_trait]
trait TurnObserver {
    fn on_token(&self, sess: &SessionId, frag: &str);
    fn on_reminder(&self, sess: &SessionId, text: &str);
    fn on_command_pending(&self, sess: &SessionId, c: &PendingCommand);
    fn on_command_result(&self, sess: &SessionId, r: &CommandResult);
    fn on_message(&self, sess: &SessionId, m: &Message);
    fn on_state(&self, sess: &SessionId, s: TurnState);
    fn on_error(&self, sess: &SessionId, e: &str);
}
```

`RunTurnConfig` gains `observer: &dyn TurnObserver` and `approver: &dyn
CommandApprover`. **`run_turn` itself stays the single owned loop and the
single execution point** — the gate is just an `.await` it already
naturally has a place for. This preserves the §4 invariant: no backend
declares hosted tools, and now nothing executes until the human (or the
auto policy) decides.

A denied command does **not** error the turn: ocha feeds a
`CommandResult { error: "denied by operator: <reason>", status: None }`
back to the model as the `Role::Tool` message (same shape as the existing
"limit exceeded" path), so the model can react and continue. This reuses
the existing feedback channel verbatim.

### 2.2 Server components

```
ocha serve
 └─ HTTP server (axum/tokio; new dep)              bind 127.0.0.1:<port>
     ├─ SessionManager
     │    HashMap<SessionId, SessionHandle>
     │    SessionHandle = { Session(JSON-compatible),
     │                      TurnState, broadcast::Sender<Event>,
     │                      Mutex (one in-flight turn),
     │                      pending: Option<PendingCommand + oneshot tx>,
     │                      backend config }
     ├─ REST handlers (§3)
     ├─ SSE handler  -> subscribes to the session broadcast channel
     └─ static file handler -> embedded single-page UI (§6)
```

- **One in-flight turn per session.** `POST …/messages` while a turn is
  running returns `409 Conflict`. Concurrency across *different* sessions
  is fine (each has its own `Backend` box / config).
- **Backends reused as-is.** `build_backend` is refactored to take a
  resolved config struct instead of `Args`, so the server can build a
  backend per session from the create-session request.
- **Session persistence.** Optional, reusing the existing
  `{"messages":[…]}` neutral format: a session created with
  `"persist_path"` is written after each turn exactly like CLI `-S`.

### 2.3 Turn state machine (per session)

```
        POST /messages
 idle ───────────────► generating
   ▲                      │  backend streams tokens (event: token)
   │                      │
   │            extract_command? ──no──► append assistant msg ─► idle
   │                      │ yes                 (event: turn_complete)
   │                      ▼
   │              awaiting_approval ──── event: command_pending
   │                      │
   │     approve ─────────┤───────── deny(reason)
   │        ▼             │              ▼
   │   execute_command    │      synthesize denied CommandResult
   │        │             │              │
   │        └──── feed CommandResult as Role::Tool ──┐
   │                      │                          │
   │                      ▼                          │
   │            (command_per_response--)              │
   │             generating  ◄───────────────────────┘
   │                      │
   └───── error ◄─────────┴─ cancel ► canceled ─► idle
```

`TurnState ∈ { idle, generating, awaiting_approval, error, canceled }`.
Every transition emits a `state` SSE event.

---

## 3. HTTP API definition

Base: `http://127.0.0.1:<port>` (default port TBD, e.g. `8765`). All
request/response bodies are JSON unless noted. Errors use
`{ "error": "<message>" }` with a conventional status code.

| Method | Path | Purpose |
|---|---|---|
| `GET`  | `/` | Single-page web UI (HTML) |
| `GET`  | `/api/health` | Liveness `{ "ok": true, "version": "…" }` |
| `GET`  | `/api/models?backend=ollama\|claude\|claude-cli` | Proxy `Backend::list_models` |
| `POST` | `/api/sessions` | Create a session |
| `GET`  | `/api/sessions` | List sessions (id, state, msg count, config) |
| `GET`  | `/api/sessions/{id}` | Session detail incl. full message history |
| `DELETE` | `/api/sessions/{id}` | Drop a session |
| `POST` | `/api/sessions/{id}/messages` | Send a user prompt → starts a turn |
| `GET`  | `/api/sessions/{id}/events` | **SSE** stream of turn events (§4) |
| `POST` | `/api/sessions/{id}/commands/{cmd_id}/approve` | Approve pending command |
| `POST` | `/api/sessions/{id}/commands/{cmd_id}/deny` | Deny pending command (reason) |
| `POST` | `/api/sessions/{id}/cancel` | Cancel the in-flight turn / pending approval |

### 3.1 `POST /api/sessions`

Request:

```json
{
  "backend": "ollama",                 // ollama | claude | claude-cli
  "model": "gemma3:27b",               // optional; backend default if omitted
  "server": "eevee",                   // ollama only, optional
  "port": 11434,                       // ollama only, optional
  "api_base": null,                    // optional override
  "max_tokens": 4096,                  // claude only, optional
  "system": "Be concise.",             // optional out-of-band system prompt
  "reminders": [ /* Reminder objects, same schema as reminders.json */ ],
  "command_per_response": 5,           // optional, default 5
  "approval_mode": "gated",            // "gated" (default in serve) | "auto"
  "persist_path": "/tmp/web.json"      // optional; CLI `-S` equivalent
}
```

Response `201`:

```json
{ "id": "s_3f9c1a", "state": "idle", "created_at": "2026-05-19T…Z" }
```

> `approval_mode` is the per-session realization of the configurable
> seam: `gated` uses `RemoteApprover`, `auto` uses `AutoApprover`. The
> server default is `gated`; the CLI is always `auto`.

### 3.2 `POST /api/sessions/{id}/messages`

Request: `{ "content": "List the files here." }`

Response `202 Accepted`: `{ "turn_id": "t_88", "state": "generating" }`
(the actual output arrives on the SSE stream). `409` if a turn is already
in flight for this session.

### 3.3 Approve / deny

`POST /api/sessions/{id}/commands/{cmd_id}/approve` → `200 { "state":
"generating" }`.

`POST /api/sessions/{id}/commands/{cmd_id}/deny`
body `{ "reason": "not allowed to touch the network" }` → `200`.
The reason is surfaced to the model via the synthesized
`CommandResult.error`. `404` if `cmd_id` is not the currently pending
command (stale/double-submit guard).

### 3.4 `GET /api/sessions/{id}`

```json
{
  "id": "s_3f9c1a",
  "state": "awaiting_approval",
  "config": { "backend": "ollama", "model": "gemma3:27b", "...": "..." },
  "messages": [
    { "role": "user",      "content": "List the files here." },
    { "role": "assistant", "content": "I'll list them.\n!!!OCHA_RUN_CMD{…}" }
  ],
  "pending_command": {
    "cmd_id": "c_12",
    "request": { "binary": "ls", "args": ["-la"], "timeout": 5,
                 "description": "Listing files" },
    "remaining_commands": 4
  }
}
```

`messages` is exactly the existing `Vec<Message>` (`role ∈
system|user|assistant|tool`). The web UI renders `tool` messages as
collapsed command-result blocks.

---

## 4. SSE event stream (`GET /api/sessions/{id}/events`)

`text/event-stream`. Each event: `event: <type>` + `data: <json>`. A
client connecting mid-turn first receives a `snapshot` event with the
full current state so the UI can reconcile (events are not replayed from
history; the snapshot + subsequent live events are authoritative).

| `event:` | `data` payload | Meaning |
|---|---|---|
| `snapshot` | full `GET /sessions/{id}` body | Sent once on connect |
| `state` | `{ "state": "generating" }` | State machine transition |
| `token` | `{ "text": "par" }` | Streamed assistant text fragment |
| `reminder` | `{ "text": "(be brief)" }` | An activated reminder (today's `[Reminder]`) |
| `message` | a `Message` object | A full message was appended to history |
| `command_pending` | `PendingCommand` (see §3.4) | **Approval required** |
| `command_result` | `CommandResult` + `{ "cmd_id": "c_12" }` | Command finished (or was denied) |
| `turn_complete` | `{ "turn_id": "t_88" }` | Plain-text answer reached; back to `idle` |
| `error` | `{ "message": "Anthropic API error: …" }` | Turn failed |
| `ping` | `{}` | Keep-alive (~15 s) |

`token`/`command_*` events map 1:1 onto the `TurnObserver` calls, so the
server adapter is mechanical. `CommandResult` is the **existing struct**
(`status, stdout, stderr, remaining_commands, error`) serialized as-is.

### Example exchange (gated)

```
C: POST /api/sessions/s1/messages { "content": "show disk usage" }
S: 202 { "turn_id": "t1", "state": "generating" }
   (on SSE)  event: state           data: {"state":"generating"}
             event: token           data: {"text":"I'll check.\n"}
             event: message         data: {"role":"assistant","content":"…!!!OCHA_RUN_CMD{…}"}
             event: command_pending data: {"cmd_id":"c1","request":{"binary":"df","args":["-h"],…}}
             event: state           data: {"state":"awaiting_approval"}
C: POST /api/sessions/s1/commands/c1/approve
S: 200 { "state": "generating" }
   (on SSE)  event: command_result  data: {"cmd_id":"c1","status":0,"stdout":"…","remaining_commands":4}
             event: token           data: {"text":"Your root fs is 42% full."}
             event: turn_complete   data: {"turn_id":"t1"}
             event: state           data: {"state":"idle"}
```

---

## 5. Concurrency, lifecycle & edge cases

- **One turn per session** (per-session `Mutex`/state guard); cross-session
  parallelism allowed.
- **Pending-approval timeout.** Configurable (default e.g. 600 s). On
  timeout the command is auto-**denied** with reason
  `"approval timed out"` and the turn continues (model gets the denied
  result) — never a silent hang. Rationale: §2 of the companion doc — a
  hung approval is the exact failure mode ocha exists to avoid.
- **`cancel`** aborts the in-flight backend stream and/or resolves a
  pending approval as a hard stop → `canceled` → `idle`. The partial
  assistant text produced so far is still appended to history (parity
  with the CLI, which keeps streamed output).
- **Client disconnect** from SSE does **not** cancel the turn (it keeps
  running server-side; reconnect replays via `snapshot`). Explicit
  `cancel` is the only stop.
- **Crash/restart.** In-memory sessions are lost unless `persist_path`
  was set; with it, history (not in-flight turn) is recoverable on
  recreate. Documented limitation, not solved here.
- **`command_per_response`** semantics are unchanged: the limit still
  decrements per executed command; a denied command still counts as
  consuming a slot (it took a loop iteration), matching today's
  "limit exceeded" handling.

---

## 6. Web UI (single page, embedded)

Minimal, dependency-free static HTML/JS embedded in the binary
(`include_str!`), served at `/`. Panels:

- **Sidebar:** session list + "New session" form (backend/model/system/
  reminders/approval-mode).
- **Transcript:** streamed messages; `tool` messages collapsed; live
  token rendering from the SSE `token` events.
- **Approval banner:** appears on `command_pending` — shows
  `binary + args`, `description`, `timeout`, remaining-commands; two
  buttons: **Approve** / **Deny** (deny opens a reason input).
- **Composer:** prompt box (disabled unless state is `idle`),
  Cancel button (enabled while `generating`/`awaiting_approval`).
- Connection indicator driven by SSE `ping`.

"Simple" is a hard requirement: vanilla JS + `EventSource`, no build
step, no framework.

---

## 7. Security model

Localhost-only, no auth — **by decision**, and acceptable only because:

- The server binds `127.0.0.1`; it is not reachable off-box without the
  operator deliberately tunneling/proxying it.
- The **whole point** is that command execution is now *gated*: nothing
  runs until a human approves. This is strictly safer than today's CLI,
  which auto-executes.

Explicit warnings to document in README/`--help`:

1. Anyone with loopback access (incl. other local users / a compromised
   local process) can drive the conversation and **approve commands that
   execute with ocha's privileges**. Treat the box as the trust boundary.
2. `approval_mode: "auto"` disables the gate — only for trusted,
   non-destructive flows.
3. For remote use: `ssh -L` tunnel or an authenticating reverse proxy.
   ocha intentionally does not reimplement TLS/auth (kept simple; the
   companion doc's philosophy is to not build fragile upper layers).
4. The bind address may be made configurable later, but defaults to
   loopback and any non-loopback bind must require an explicit,
   loudly-documented flag.

---

## 8. Required code changes (no implementation yet)

Behavior-preserving for the existing CLI; each is a small, isolated unit:

1. **Extract seams in `run_turn`:** introduce `TurnObserver` +
   `CommandApprover`; route all `print!`/`println!` and the
   execute-command call through them. Add both to `RunTurnConfig`.
2. **CLI adapters:** `StdoutObserver` (byte-for-byte identical output)
   and `AutoApprover`. Pure refactor; covered by existing tests + a new
   golden test that CLI output is unchanged.
3. **Config refactor:** split a `BackendConfig` out of `Args` so both
   `main` and the server build backends the same way (`build_backend`
   takes `&BackendConfig`).
4. **New `serve` subcommand + module** (`src/serve/`): HTTP server,
   `SessionManager`, SSE, `RemoteApprover`, embedded UI. Per §1.1,
   **target zero new external runtime deps**: build the server on
   `hyper`/`hyper-util` (already in the graph via `reqwest`) or directly
   on `tokio` TCP + a tiny hand-rolled HTTP/1.1 + SSE layer (the surface
   needed — a handful of JSON routes and one `text/event-stream` — is
   small). Any framework crate (`axum`, …) is an explicit-approval
   decision (see §9.1), not a default.
5. **Docs:** README "Remote Web UI" section; `GEMINI.md` roadmap +
   architecture note; update the companion design doc to point here as
   the concrete remote-approval realization of its §4.

These map onto the 5 verifiable milestones in §11; within each
milestone, commits still follow the project's minimal-unit practice.

---

## 9. Open questions

**All resolved — see §1 "Decisions taken".** For the record: HTTP layer
→ `hyper`/`hyper-util`; reconnect → message-level snapshot; reminders
editing → deferred; default port → `8765` + printed URL; multi-tab →
broadcast, `cmd_id`-guarded; CLI args → session defaults; test backend →
in-crate mock; REPL coexistence → `serve` is its own mode, mutually
exclusive with the stdin REPL in one process; commit/push → doc now,
push per milestone.

No open questions remain blocking v1 implementation.

---

## 10. Why this is consistent with ocha's thesis

The companion note's §4 verdict: *"with the raw Messages API and your own
loop, the approval gate is a single `if` in your code — you simply do not
execute until your remote UI approves."* This document is that sentence
made concrete: `run_turn` stays the single owned loop; the remote gate is
one `await` on a `oneshot` resolved by an HTTP call; no vendor control
protocol, no SDK upper layer, no hosted tools. The web UI is a *client of
ocha's loop*, never a replacement for it.

---

## 11. Implementation milestones (v1)

> **All five shipped on `main`.** M1 seam refactor · M2 serve skeleton ·
> M3 HTTP+SSE conversation · M4 remote approval gate · M5 embedded UI +
> docs. One transitive dep (`httpdate`) was caught by the §12.3 audit in
> M2, surfaced, and explicitly approved before proceeding — the gate
> worked exactly as intended.

Five milestones. Each is **contained** (lands on `main`, breaks nothing,
the CLI keeps working at every step) and **verifiable** (a concrete,
automatable acceptance check — no "looks done"). No new external runtime
dependency is introduced by any milestone (§1.1). Ordered; each builds on
the previous.

### M1 — Seam refactor (no behavior change)

**Build:** Introduce `TurnObserver` + `CommandApprover`; route every
`print!`/`println!` and the execute-command call in `run_turn` through
them. Add CLI adapters `StdoutObserver` + `AutoApprover`. Split
`BackendConfig` out of `Args`; `build_backend(&BackendConfig)`.

**Contained:** Pure refactor. No server, no new deps. CLI is the only
caller.

**Verify (acceptance):**
- Existing 15 unit + 3 e2e tests still green (`OCHA_TEST_OLLAMA_HOST=eevee
  cargo test`).
- New **golden test**: capture `ocha -s eevee -m gemma3:12b "<fixed
  prompt>"` stdout before vs after the refactor → byte-identical
  (incl. the `[Reminder]/[Payload]/[Executing]/[Result]` side-channel).
- `cargo clippy -D warnings` + `cargo fmt --check` clean.

### M2 — `serve` skeleton: lifecycle only

**Build:** `ocha serve` subcommand. `hyper`/`hyper-util` server bound to
`127.0.0.1:8765`. `SessionManager`. Endpoints: `GET /api/health`,
`GET /api/models`, `POST/GET/DELETE /api/sessions[/{id}]`,
`GET /api/sessions/{id}` (history). `POST …/messages` returns `501 Not
Implemented` for now.

**Contained:** No turn execution, no SSE, no approval. Server is inert
beyond bookkeeping; CLI untouched.

**Verify (acceptance):** A `tests/serve.rs` integration test that spawns
`ocha serve` (mock backend) on an ephemeral port, then asserts: health
`ok`; create → `201` + id; list/get reflect it; delete → gone; `models`
returns the mock's list; a bind check that the socket is loopback-only.
Hermetic — no network.

### M3 — Conversation over HTTP + SSE streaming

**Build:** Implement `POST …/messages` driving `run_turn` with an
`SseObserver`; `GET /api/sessions/{id}/events` (SSE: `snapshot`,
`state`, `token`, `message`, `turn_complete`, `error`, `ping`).
**Approval still `AutoApprover`** in serve (commands auto-execute, exactly
like the CLI today) — the gate is M4.

**Contained:** Full remote conversation works; behavior matches the CLI
(auto-exec). No gate yet.

**Verify (acceptance):** Hermetic integration test (mock backend scripted
to stream a fixed multi-token reply): create session → POST a prompt →
consume SSE → assert ordered `state:generating` … `token`* …
`turn_complete` `state:idle`, the assembled text equals the mock script,
and it is appended to `GET /sessions/{id}`. Reconnect mid-idle →
`snapshot` carries full history.

### M4 — Remote approval gate

**Build:** `RemoteApprover` (parks turn on a `oneshot`);
`command_pending` event + `awaiting_approval` state;
`POST …/commands/{cmd_id}/approve|deny`; `cancel`; 600 s configurable
auto-deny-and-continue timeout; denied → synthesized
`CommandResult{error}` fed back as `Role::Tool`.

**Contained:** Builds only on M3 seams; CLI still `AutoApprover`.

**Verify (acceptance):** Hermetic integration test (mock backend scripted
to emit `!!!OCHA_RUN_CMD{"binary":"echo",…}` then a follow-up reply):
assert `command_pending` + `awaiting_approval`; (a) approve →
`command_result status:0` + turn continues to `turn_complete`; (b) deny
w/ reason → `CommandResult.error` contains the reason and it is fed back
as the next `Role::Tool` message; (c) short timeout override → auto-deny
fires and the turn still completes. `cmd_id` stale/!pending approve →
`404`.

### M5 — Embedded vanilla-JS UI + docs

**Build:** Single-page UI (`include_str!`, vanilla JS/CSS, no build step,
zero third-party JS — §1.1) at `GET /`: session list/create, transcript
with live tokens, approval banner (approve / deny+reason), composer,
cancel. Docs: README "Remote Web UI" section + security warnings;
`GEMINI.md` roadmap + architecture pointer; update the companion design
note to cite this as its §4 realization.

**Contained:** UI is static; API frozen by M4. Final milestone.

**Verify (acceptance):** `GET /` returns HTML and references no external
origin (grep the asset for `http://`/`https://`/`cdn` → none). Scripted
browserless smoke via the API already covered by M3/M4; a documented
manual UI smoke checklist (load, send, approve, deny, cancel). Full suite
green; `clippy -D warnings`; docs present. Tag/announce v1.

> **Per-milestone definition of done:** acceptance check passes in CI/
> local, `cargo fmt --check` + `cargo clippy --all-targets -- -D
> warnings` clean, full `OCHA_TEST_OLLAMA_HOST=eevee cargo test` green,
> CLI behavior unchanged, **dependency audit (§12) passes**.

---

## 12. Approved dependency baseline & per-milestone audit gate

This section is the **predefined, reviewed dependency allow-list** for
the whole serve effort. It operationalizes §1.1.

### 12.1 What the plan adds

**Zero new crates enter `Cargo.lock`.** The HTTP/SSE server is built on
crates **already in the dependency graph** (pulled transitively today by
`reqwest`). The only change to `Cargo.toml` is **promoting a few
already-present crates to *direct* dependencies and enabling their
`server` feature** — additional compiled code paths in
*already-vetted, already-locked* crates, not new supply-chain surface.

Caveat made explicit (this is *why* approval is needed): today `hyper
v1.8.1` / `hyper-util v0.1.20` are compiled with **client features only**
(`client, http1, http2`) — **no `server`**. Serve must enable it.

### 12.2 Pre-approved direct-dependency changes (this is the allow-list)

| Crate | In lock today | Change | Why |
|---|---|---|---|
| `hyper` | ✅ 1.8.1 | add as direct dep, `features=["server","http1"]` | HTTP/1.1 server |
| `hyper-util` | ✅ 0.1.20 | add as direct dep, `features=["tokio","server","server-auto"]` | serve connection/IO glue (`service` not needed — `hyper::service::service_fn` used) |
| `http-body-util` | ✅ 0.1.3 | add as direct dep | request/response bodies |
| `http` | ✅ 1.4.0 | direct dep *only if* not re-exported sufficiently by `hyper` | status/headers types |
| `bytes` | ✅ 1.11.1 | add as direct dep | `Full<Bytes>` response bodies |
| `httpdate` | ⚠️ **new** 1.0.3 | **approved 2026-05-20** | pulled transitively by `hyper`'s `server` feature (Date header). Leaf crate, zero sub-deps, hyper-org maintained. Caught by the M2 §12.3 audit and explicitly approved by the user before proceeding; added to `docs/dep-baseline.txt`. |
| `tokio` | ✅ 1.49.0 (direct, `full`) | **no change** | `net`, `sync::{oneshot,broadcast}`, `io` already enabled by `full` |
| `serde`, `serde_json`, `async-trait`, `clap`, `futures-util` | ✅ direct | **no change** | JSON, trait, `serve` subcommand, streams |

**Tests:** the serve integration tests use `reqwest` (already a normal
dep) as the HTTP client and the in-crate **mock backend** (no crate).
**No new dev-dependency** beyond the existing `assert_cmd` + `tempfile`.

Anything **not** in this table — a new crate in `Cargo.lock`, a
different crate for the HTTP/JSON/async layer (`axum`, `warp`,
`hyper`-alternatives), a new dev-dep, or a feature enablement on a crate
not listed here — is **out of scope and requires STOP + explicit user
confirmation** before it is added. "It's small" / "it's only transitive"
is not an exemption (§1.1).

### 12.3 Per-milestone audit procedure (mandatory gate)

Run at the end of **every** milestone M1–M5, before its commit/push:

```sh
# 1. No unexpected lock changes vs the reviewed baseline:
git diff --stat -- Cargo.lock Cargo.toml
cargo tree -e normal --prefix none | awk '{print $1,$2}' | sort -u \
  > /tmp/deps.now
diff <(cat docs/dep-baseline.txt) /tmp/deps.now    # Appendix A snapshot
# 2. Any added crate / feature must appear in §12.2. If the diff shows
#    anything else  ->  STOP, do not commit, ask the user.
```

A milestone **fails its definition of done** if the audit shows any
crate or feature change not pre-approved in §12.2. The reviewed baseline
snapshot lives in **Appendix A** (and a machine-checkable copy at
`docs/dep-baseline.txt`, added in M1).

---

## Appendix A — reviewed transitive baseline (audit reference)

Captured 2026-05-20, **129** normal transitive crates (128 at design
time + `httpdate`, approved during M2 — see §12.2) + **2** dev-only
(`assert_cmd`, `tempfile`). This is the reviewed pre-image; the §12.3
audit diffs against it every milestone. (Versions omitted here for
readability; the exact pinned snapshot is `docs/dep-baseline.txt`,
regenerated only with explicit approval.)

```
anstream anstyle anstyle-parse anstyle-query async-trait atomic-waker
aws-lc-rs aws-lc-sys base64 bitflags bytes cfg-if chacha20 chrono
clap clap_builder clap_derive clap_lex colorchoice cpufeatures
displaydoc encoding_rs equivalent errno fnv form_urlencoded
futures-channel futures-core futures-io futures-macro futures-sink
futures-task futures-util getrandom h2 hashbrown heck http http-body
http-body-util httparse httpdate hyper hyper-rustls hyper-util
iana-time-zone
icu_collections icu_locale_core icu_normalizer icu_normalizer_data
icu_properties icu_properties_data icu_provider idna idna_adapter
indexmap ipnet iri-string is_terminal_polyfill itoa libc litemap
lock_api log memchr mime mio num-traits once_cell openssl-probe
parking_lot parking_lot_core percent-encoding pin-project-lite
pin-utils potential_utf proc-macro2 quote rand rand_core reqwest
rustls rustls-native-certs rustls-pki-types rustls-platform-verifier
rustls-webpki scopeguard serde serde_core serde_derive serde_json
signal-hook-registry slab smallvec socket2 stable_deref_trait strsim
subtle sync_wrapper syn synstructure tinystr tokio tokio-macros
tokio-rustls tokio-util tower tower-http tower-layer tower-service
tracing tracing-core try-lock unicode-ident untrusted url utf8_iter
utf8parse want writeable yoke yoke-derive zerofrom zerofrom-derive
zeroize zerovec zerovec-derive zerotrie zmij
```

> Note: `tower*`, `axum`, `warp` etc. are **not** used by the plan even
> though some `tower*` crates appear above (they arrive via `reqwest`);
> the server uses `hyper`/`hyper-util` directly. Their presence in the
> baseline does **not** authorize adding them as direct deps.
