# LLM interface layering, primitive APIs, and a provider-agnostic
# approval-gated agent harness

**Date:** 2026-05-18
**Status:** Design rationale for ocha's multi-backend architecture

This is ocha's core design note. It records **why** ocha owns its own
provider-agnostic agent loop and single approval/execution point instead
of delegating to a vendor agent SDK, and what the lowest-common-denominator
interface across Anthropic, Google Gemini, OpenAI and Ollama actually is.
It is the rationale behind the `Backend` trait, the neutral
`Vec<Message>` conversation model, and the plain-text `!!!OCHA_RUN_CMD`
protocol (see `GEMINI.md` → "Architecture Decisions").

> **Origin (external context).** This investigation started from a Claude
> Code Remote Control tool-approval hang observed in a container harness
> that is *not* part of ocha. The conclusion that shaped ocha — "the app
> should own the loop and the human gate, built on the most primitive and
> most stable API rather than on an unstable upper-layer SDK" — is
> captured in §4 below. The original Remote-Control-hang write-up lives in
> separate infrastructure tooling and is **not** needed to develop ocha;
> this note is self-contained.

---

## 1. Interface layering (most primitive → highest level)

```
[most primitive]
1. Anthropic Messages API   POST /v1/messages   (HTTP + JSON)
     └ stream:true → SSE (text/event-stream): message_start /
       content_block_delta / ... / message_stop
     ↑ official SDKs (anthropic-py / -ts) are thin wrappers.
       Nothing lower is exposed (model weights are not public).
2. Agent loop = application responsibility
     The Messages API emits tool_use blocks but NEVER executes tools.
     The execute-tool → feed tool_result → re-call loop is yours to write.
3. Claude Agent SDK / Claude Code
     loop + permission/control protocol + stream-json transport +
     built-in tools + context compaction + MCP + skills + system prompt.
```

Key consequence: **the most primitive, most stable, best-documented
interface is the Messages API** (`/v1/messages`); `stream: true` (SSE) is
its streaming *mode*, not a separate lower API. Batch / token-counting /
Files APIs are siblings, not lower layers. The stream-json transport and
`control_request`/`control_response` permission channel used by Claude Code
live **above** the Messages API; their instability does not affect the
Messages API itself.

## 2. Agent SDK: current known-bad areas (upper layer only)

Relevant when choosing SDK vs. raw Messages API for a custom
remote-driven, approval-gated client. Confirmed from issue trackers:

- **claude-code #27203** — background subagent tool calls bypass
  `canUseTool` in `default` mode and silently fail; denial corrupts the
  parent transport ("Stream closed"). Directly breaks remote-approval
  routing for background tasks.
- **claude-agent-sdk-python #469** — CLI v2.1.6+ does not emit the
  `can_use_tool` `control_request` even with `--permission-prompt-tool
  stdio`; Python permission callbacks effectively non-functional via CLI
  transport. Must be verified on real hardware before relying on it.
- **claude-agent-sdk-python #926** — bundled `claude` binary hangs when
  stdout is not a TTY (Docker/pipes/cron) — structurally the same class as
  the original Remote Control hang.
- **ts-sdk #289 / #287** — long-session resume silently drops
  `tool_use`/`tool_result` history / `parentUuid` chain corruption →
  duplicated side-effecting actions.
- Docs-level constraints: `canUseTool` fires only on `"ask"` decisions;
  Python `can_use_tool` needs a dummy `PreToolUse` hook + streaming;
  `allowed_tools` does not constrain `bypassPermissions`; hooks run
  concurrently (no ordering).

Verdict: `canUseTool`-based remote approval is *partially viable* with
workarounds (no background tasks, short sessions, version pinning, TS SDK
preferred). For a bulletproof approval gate, see §4.

## 3. Primitive-API comparison: Anthropic vs Gemini vs OpenAI

OpenAI's `codex` CLI uses the **Responses API** (`POST /v1/responses`);
Chat Completions support in Codex was deprecated (removal early Feb 2026).

| Dimension | Anthropic Messages | Gemini generateContent | OpenAI Responses |
|---|---|---|---|
| Endpoint | `POST /v1/messages` | `POST .../{model}:generateContent` | `POST /v1/responses` |
| Turn container | `messages[]` | `contents[]` | `input[]` or string |
| Roles | `user`,`assistant` | `user`,`model` | `user`,`assistant`,`developer`,`system` |
| System prompt | top-level `system` | top-level `systemInstruction` | top-level `instructions`/`developer` |
| Content units | typed blocks | typed `parts` | typed items + content parts |
| Tool decl. | JSON Schema (`input_schema`) | OpenAPI-subset JSON Schema (UPPERCASE) | JSON Schema (`parameters`,`strict`) |
| Model call signal | `tool_use` block | `functionCall` part | `function_call` item |
| Result fed back | `tool_result` in **user** msg | `functionResponse` part | `function_call_output` item |
| Args encoding | object | object | **JSON string** |
| API executes user tools? | No | No | No |
| Hosted self-executing tools | code_exec, web_search/fetch | code execution, Google Search | web_search, code_interpreter, file_search, MCP |
| Conversation state | Stateless | Stateless | Stateless **or** server-side (`previous_response_id`+`store`) |
| Streaming transport | SSE | chunked JSON (opt. SSE) | SSE (semantic typed events) |
| Partial tool-args stream | yes (`input_json_delta`) | **no** (whole `functionCall`) | yes (`function_call_arguments.delta`) |
| Stop/finish | `stop_reason` | `finishReason` | `status`+`incomplete_details` |
| Usage | `input_tokens`/`output_tokens` | `promptTokenCount`/`candidatesTokenCount` | `input_tokens`/`output_tokens` |
| Auth | `x-api-key` | `x-goog-api-key`/Bearer | `Authorization: Bearer` |

### Extracted common abstraction

All three reduce to: **conversation = ordered role-tagged turns;
system prompt = one out-of-band slot; turn content = ordered typed parts
(`text`/`media`/`tool_call`/`tool_result`/`thinking?`); tools = `{name,
description, JSON Schema}`; agent loop = model-emits-call → harness
intercepts → (approval gate) → harness executes → feeds result back →
repeat until non-tool finish; streaming = normalized
text/tool-args/finish deltas; result = `{finish_reason, usage}`.**

Crucially, **none of the three executes user-defined tools** — they only
emit requests. So an "approve before execute" gate is *identical logic*
across providers.

### Divergences that force provider-specific code

1. **Hosted/server-executed tools bypass the approval gate** (Gemini
   codeExecution/Search, OpenAI web_search/code_interpreter/file_search/MCP,
   Anthropic code_execution/web_search). They run remotely before the
   harness sees a call. → the harness must **not declare hosted tools**.
2. **OpenAI server-side state** (`previous_response_id`+`store`): force
   `store:false` + resend full input to keep one stateless transcript.
3. Tool-arg encoding: OpenAI `arguments` is a JSON string (decode it).
4. Streaming tool-arg granularity: Gemini delivers whole `functionCall`.
5. Role/system-prompt shapes differ (per-provider serializers).
6. `thinking`/`reasoning` round-trip rules are non-portable.
7. Parallel tool calls toggled differently; Anthropic needs all
   `tool_result`s in the next message.

## 4. Verdict: the portable, approval-gated design

The "**app owns the loop + human gate**" design is viable across Anthropic,
Gemini and OpenAI with a thin per-provider adapter (role renaming, OpenAI
JSON-string decode, system-prompt placement, streaming-transport adapter),
**provided**:

- **hosted/server-executed tools are never declared** (non-negotiable —
  otherwise the gate is silently bypassed), and
- **OpenAI is pinned to stateless** (`store:false`).

This sidesteps the entire Claude Code / Agent SDK upper-layer instability
(§2): with the raw Messages API and your own loop, the approval gate is a
single `if` in your code — you simply do not execute a `tool_use` block
until your remote UI approves. Trade-off: you reimplement built-in tools,
file-edit heuristics, context compaction, MCP, skills, system prompt.

| Approach | Approval-gate robustness | Build cost | Exposure to SDK bugs |
|---|---|---|---|
| Messages API + own loop | Highest (your code is the only gate) | High | None |
| Agent SDK + `canUseTool` | Medium (SDK firing rules / bugs) | Low–Med | #469/#27203/… |
| `claude --remote-control` | Low (local TTY dialog, hangs) | Lowest | Structural hang |

**How ocha applies this:** ocha is the "Messages API + own loop" row.
`main.rs` owns the backend-agnostic turn loop and is the single
approval/execution point; the `!!!OCHA_RUN_CMD` plain-text protocol is the
gate (works even on tool-less local models); no backend declares hosted
tools, so the gate can never be silently bypassed.

### The `claude-cli` backend: a deliberate, contained exception

The `claude-cli` backend talks to the locally installed Claude Code CLI
(`claude -p --output-format stream-json`) — structurally the upper layer
this note argues *against*. It exists for exactly one payoff: the CLI is
already authenticated (OAuth / subscription), so the backend needs no
`ANTHROPIC_API_KEY` and no billing setup. The contradiction is contained
by stripping the upper layer back down to a tool-less model that ocha
drives with its own plain-text loop — exactly the Ollama shape:

- **Built-in tools removed** (`--tools ""`, *not* `--allowed-tools ""`)
  — this drops the tools from the declared set entirely, not just at
  permission time, so Claude Code cannot execute anything and the model
  has no Bash to fall back on. This is the §4 "hosted/self-executing
  tools must never be declared" rule applied to a CLI instead of an API.
  (`--allowed-tools ""` was insufficient: the model still saw a Bash
  tool and used it instead of ocha's protocol.)
- **`--system-prompt` always set** to an `AGENTIC_SYSTEM` preamble (plus
  any caller system prompt). It replaces Claude Code's default agent
  persona and tells the now-tool-less model that the only way to act is
  ocha's plain-text `!!!OCHA_RUN_CMD` protocol. Result: claude-cli does
  both plain chat *and* the agentic loop identically to a tool-less
  Ollama model, with ocha still the single approval/execution point.
- **`--no-session-persistence`** — state stays ocha's neutral
  resent-history `Vec<Message>`, not Claude Code's session store.

Residual exposure is accepted knowingly: the structural non-TTY/pipe
hang class (§2, claude-agent-sdk-python #926) applies to *this backend
only*; the API-based `claude`/`ollama` backends remain on the primitive,
stable layer. Cost note: each call still carries Claude Code's cached
~24k-token system prompt, so `claude-cli` is not as cheap as the raw
`claude` API backend at the same model — it buys auth convenience, not
efficiency.

---

## 5. Ollama, and the minimal universal streaming interface

### Ollama specifics (confirmed from primary docs unless marked *inferred*)

**Native API** (`github.com/ollama/ollama/blob/main/docs/api.md`)

- **Endpoints:** `POST /api/chat` (`messages[]`) and `POST /api/generate`
  (single `prompt`); `POST /api/embed`; `/api/tags|show|ps` for model mgmt.
- **Roles / system prompt:** `system`,`user`,`assistant`,`tool`. System
  prompt via a `system`-role message, the top-level `system` field on
  `/api/generate`, or the Modelfile `SYSTEM` directive (lowest precedence).
- **Streaming transport: NDJSON, not SSE** — newline-delimited JSON
  objects, no `data:` prefix, no `[DONE]` sentinel. `/api/generate` chunks
  carry `response`; `/api/chat` chunks carry `message:{role,content}`.
- **Terminal:** every chunk has `done` (bool); final chunk has
  `done:true` + `done_reason` (`stop`/`load`/`unload`; *inferred:*
  `length` on truncation).
- **Usage:** final chunk only — `prompt_eval_count`, `eval_count` (+
  nanosecond durations). `/api/generate` also returns an opaque `context`
  token array reusable as cheap multi-turn state.
- **Tools:** `/api/chat` accepts OpenAI-style `tools` (JSON Schema);
  returns `message.tool_calls[].function.{name,arguments}` with
  **`arguments` a parsed object** (not a string). Results fed back as
  `{role:"tool",content,tool_name}`. Tool support is **model-dependent**;
  a base model without a tool template **silently ignores `tools`, no
  error** (*inferred from behavior*). Streamed tool calls supported.
- **Multimodal:** per-message `images:["<base64>"]` for vision models.
- **Auth: none by design** (bare HTTP to `localhost:11434`); any bearer
  token is a reverse-proxy concern, not Ollama's.

**OpenAI-compat** (`docs.ollama.com/api/openai-compatibility`)

- `/v1/chat/completions`, `/v1/completions`, `/v1/embeddings`,
  `/v1/models`, and (v0.13.3+, **experimental, non-stateful**)
  `/v1/responses` (no `previous_response_id` → not the stateful Responses
  model; older versions 404).
- **Streaming here is SSE** (`data:` + `[DONE]`) — *different transport
  from the native NDJSON path*. `arguments` here is a JSON **string**
  (OpenAI shape). `api_key` required but **ignored**. Treat as
  *largely-complete-but-partial* (`n` ignored, partial field coverage,
  no stateful conversations).

### Comparison rows (extends the §3 table)

| Dimension | Ollama native | Ollama OpenAI-compat |
|---|---|---|
| Endpoint | `POST /api/chat` (`/api/generate`) | `POST /v1/chat/completions` (`/v1/responses` non-stateful, exp.) |
| Turn container | `messages[]` (or `prompt`) | `messages[]` |
| Roles | `system`,`user`,`assistant`,`tool` | `system`,`user`,`assistant`,`tool` |
| System prompt | `system` field / system msg / Modelfile `SYSTEM` | `system` role message |
| Content units | `content` string + `images[]` + `tool_calls` | `content` string (+ multimodal parts) |
| Tool decl. schema | OpenAI-style `function` / JSON Schema | OpenAI `function` |
| Model call signal | `message.tool_calls[]` | `tool_calls[]` (`finish_reason:"tool_calls"`) |
| Result fed back | `{role:"tool",content,tool_name}` | `{role:"tool",tool_call_id,content}` |
| Args encoding | JSON **object** | JSON **string** |
| API executes user tools? | No | No |
| Hosted self-exec tools | None | None |
| Conversation state | client resends (`/api/generate` `context` token) | client resends (no `previous_response_id`) |
| Streaming transport | **NDJSON** | **SSE** (`[DONE]`) |
| Partial tool-args stream | yes (chunked tool_calls) | yes (OpenAI-style deltas) |
| Stop/finish | `done`+`done_reason` | `finish_reason` |
| Usage | `prompt_eval_count`/`eval_count` (final chunk) | `usage` (model-dependent) |
| Auth | **none** (localhost) | Bearer **ignored** (placeholder) |

### Minimal universal interface (text-chunk-in → text-chunk-out)

Provable lowest common denominator across all five — including a bare
base model with no tool template, no vision, no usage guarantees:

**Required contract (works everywhere):**
- **Input:** one user text string (+ optional *abstract* system text the
  adapter routes per backend).
- **Output:** a stream of append-only **text deltas** (no structured
  blocks).
- **Terminal:** exactly one end-of-stream event with a finish reason
  normalized to `{stop, length, error}` (Anthropic `stop_reason`, Gemini
  `finishReason`, OpenAI `status`, Ollama `done_reason` all suffice).
- **Multi-turn:** by *resending* accumulated text only (the one strategy
  all five support; server-side state / Ollama `context` are
  optimizations, not part of the contract).

**Optional / capability-negotiated (never assumed):** token usage
(nullable — Ollama native: final chunk only; some compat configs omit);
tools/function calling (**not universal** — fails open on tool-less local
models); multimodal images; partial tool-arg streaming; server-side
conversation state; thinking/reasoning traces.

### Feasibility verdict

A single lowest-common-denominator **streaming-text** interface is
realistically implementable. Thinnest viable contract:

```
generate(systemText?, userText) -> stream<TextDelta>
                                  + Final{finishReason, usage?}
```

Every backend satisfies this with a thin adapter. Required per-backend
shims: **transport normalization** (the biggest divergence — SSE typed
events vs Gemini chunked-JSON vs **Ollama NDJSON** vs Ollama-compat
SSE-`[DONE]`; do not share framing code), system-prompt placement,
delta-field extraction, finish-reason mapping, nullable usage.

Failure modes to design around: (1) tool-less local models silently
ignore `tools` — capability-detect, never infer tool support from a model
name; (2) NDJSON vs SSE need different parsers; (3) args encoding object
vs string — normalize at the adapter; (4) auth: none vs ignored vs real;
(5) per-model context windows differ wildly — surface `length`, never
assume a window; (6) Ollama OpenAI-compat is partial — prefer **native
`/api/chat`** for Ollama, compat as fallback only.

**Recommendation.** Universal contract = *single user text (+ optional
abstract system text) in → ordered text deltas + one terminal
`{finishReason, usage?}` out*, multi-turn by client resend. Everything
richer (tools, multimodal, partial tool-arg streaming, server-side state,
reasoning, usage) is **capability-detected per (backend, model)** behind
optional extension interfaces — never in the base path. This base path is
genuinely universal; it does **not** carry the approval gate, because the
gate lives one layer up in the agent loop (§4) and only applies to
backends/models that actually support tools.

### Primary sources

- Ollama native API — https://github.com/ollama/ollama/blob/main/docs/api.md
- Ollama OpenAI compatibility — https://docs.ollama.com/api/openai-compatibility ,
  https://ollama.com/blog/openai-compatibility
- `/v1/responses` tracking — https://github.com/ollama/ollama/issues/10309 ,
  https://github.com/ollama/ollama/issues/13595
- Go API types — https://pkg.go.dev/github.com/ollama/ollama/api
