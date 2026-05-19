//! Seams that let the single owned turn loop (`run_turn`) be driven by
//! something other than a terminal — without the loop knowing or caring.
//!
//! - [`TurnObserver`] replaces the loop's hard-coded `print!`/`println!`
//!   side-channel. The CLI uses [`StdoutObserver`], which reproduces the
//!   previous output **byte for byte**; the future `serve` mode will
//!   supply an observer that emits SSE events instead.
//! - [`CommandApprover`] is the single decision point between detecting a
//!   `!!!OCHA_RUN_CMD` and executing it. The CLI uses [`AutoApprover`]
//!   (always approve — exactly today's behavior); `serve` will supply a
//!   remote approve/deny gate.
//!
//! Both seams are `Send + Sync` so the loop stays usable from async
//! server tasks (and so the streaming closure remains `Send`).

use crate::CommandRequest;
use async_trait::async_trait;
use std::io::{self, Write};

/// Outcome of the approval gate for one command.
pub enum Decision {
    Approve,
    // Constructed by the remote serve gate (M4); dormant under the CLI's
    // AutoApprover. The reason is fed back to the model on denial.
    #[allow(dead_code)]
    Deny {
        reason: String,
    },
}

/// Decides whether a parsed [`CommandRequest`] may run. ocha remains the
/// single execution point; this only gates it.
#[async_trait]
pub trait CommandApprover: Send + Sync {
    async fn decide(&self, req: &CommandRequest) -> Decision;
}

/// CLI policy: auto-approve every command (unchanged historical
/// behavior). The `Deny` path therefore never fires for the CLI.
pub struct AutoApprover;

#[async_trait]
impl CommandApprover for AutoApprover {
    async fn decide(&self, _req: &CommandRequest) -> Decision {
        Decision::Approve
    }
}

/// Observes everything the turn loop would otherwise have printed. Each
/// method corresponds 1:1 to a former `print!`/`println!` site in
/// `run_turn`, so an implementation can render it however it likes.
pub trait TurnObserver: Send + Sync {
    /// A streamed assistant text fragment (was `print!("{}", frag)` + flush).
    fn token(&self, frag: &str);
    /// An activated reminder; `text` is already trimmed by the caller.
    fn reminder(&self, text: &str);
    /// End of the streamed assistant response (was the trailing `println!()`).
    fn response_end(&self);
    /// The raw `!!!OCHA_RUN_CMD` JSON payload about to be parsed/run.
    fn command_payload(&self, json: &str);
    /// The command that is about to execute.
    fn command_executing(&self, binary: &str, args: &[String]);
    /// Non-empty captured stdout from a finished command.
    fn command_stdout(&self, s: &str);
    /// Non-empty captured stderr from a finished command.
    fn command_stderr(&self, s: &str);
    /// The serialized `CommandResult` fed back to the model.
    fn command_result(&self, json: &str);
}

/// CLI observer: writes to stdout exactly as `run_turn` did before the
/// seam refactor. The format strings here are the byte-for-byte
/// contract guarded by the M1 golden test.
pub struct StdoutObserver;

impl TurnObserver for StdoutObserver {
    fn token(&self, frag: &str) {
        print!("{}", frag);
        let _ = io::stdout().flush();
    }
    fn reminder(&self, text: &str) {
        println!("[Reminder: {}]", text);
    }
    fn response_end(&self) {
        println!();
    }
    fn command_payload(&self, json: &str) {
        println!("[Payload: {}]", json);
    }
    fn command_executing(&self, binary: &str, args: &[String]) {
        println!("[Executing: {} {}]", binary, args.join(" "));
    }
    fn command_stdout(&self, s: &str) {
        println!("STDOUT:\n{}", s);
    }
    fn command_stderr(&self, s: &str) {
        println!("STDERR:\n{}", s);
    }
    fn command_result(&self, json: &str) {
        println!("[Result: {}]", json);
    }
}
