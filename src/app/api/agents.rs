use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use sha2::{Digest, Sha256};

use crate::api::schema::{
    AgentInstructParams, AgentPromptParams, AgentRenameParams, AgentSendKeysParams,
    AgentStartParams, AgentTarget, InstructionDelivery, InstructionOutcome, LastInstructionInfo,
    PaneReadResult, ResponseResult,
};
use crate::app::App;

use super::responses::{encode_error, encode_error_body, encode_success};

const AGENT_PROMPT_SUBMIT_DELAY: Duration = Duration::from_millis(300);
/// How long a written instruction waits for the OMP integration to confirm it.
#[cfg(not(test))]
pub(super) const INSTRUCTION_ACK_TIMEOUT: Duration = Duration::from_secs(8);
#[cfg(test)]
pub(super) const INSTRUCTION_ACK_TIMEOUT: Duration = Duration::from_millis(200);
/// How long a session change (`tree`, `fork`, `new_session`, `switch_session`) waits for its
/// result: OMP's branch summary is a model call, and other extensions' session handlers may each
/// take up to 30 s.
#[cfg(not(test))]
const SESSION_ACTION_ACK_TIMEOUT: Duration = Duration::from_secs(120);
#[cfg(test)]
const SESSION_ACTION_ACK_TIMEOUT: Duration = Duration::from_millis(600);

fn action_ack_timeout(op: crate::api::schema::AgentActionOp) -> Duration {
    if op.is_session_change() {
        SESSION_ACTION_ACK_TIMEOUT
    } else {
        INSTRUCTION_ACK_TIMEOUT
    }
}
/// The longest an instruction may wait in the server queue before it is written. With the ack
/// wait this stays under the 15 s that callers allow for an answer (125 s for the session
/// changes, whose ack wait is `SESSION_ACTION_ACK_TIMEOUT`).
const INSTRUCTION_QUEUE_LIMIT: Duration = Duration::from_secs(5);
/// The integration discards a block that reaches it later than this after it was written. The
/// rest of the ack timeout leaves time for the take to be acked.
const INSTRUCTION_EXPIRY: Duration =
    Duration::from_millis(INSTRUCTION_ACK_TIMEOUT.as_millis() as u64 / 2);

/// How long Herdr follows a taken delivery whose turn has not started, and keeps the last
/// outcome in `AgentInfo.last_instruction`.
#[cfg(not(test))]
pub(super) const INSTRUCTION_OUTCOME_TTL: Duration = Duration::from_secs(120);
#[cfg(test)]
pub(super) const INSTRUCTION_OUTCOME_TTL: Duration = Duration::from_secs(2);

/// An `agent.instruct` delivery written to the PTY whose outcome is not final yet.
pub(crate) struct PendingInstructionAck {
    pub(super) terminal_id: crate::terminal::TerminalId,
    pub(super) owner: crate::platform::OwnerProcessIncarnation,
    pub(super) tx: std::sync::mpsc::Sender<InstructionOutcome>,
    /// The ack wait until the take is confirmed, then the outcome wait.
    pub(super) deadline: Instant,
    /// Cleared once the listener proves it exists after this delivery was written (it took the
    /// block, or the integration reported it), even if the turn start is never confirmed.
    pub(super) withdraws_listener: bool,
    /// The listener's JS runtime when the block was written. An exec restart keeps the process
    /// incarnation but starts a new runtime, which knows nothing of this delivery.
    pub(super) runtime: Option<String>,
}

/// How many deliveries per agent `AgentInfo.recent_instructions` keeps.
const RECENT_INSTRUCTIONS: usize = 8;

/// One entry of `AgentInfo.recent_instructions`, shown until `until`.
pub(crate) struct RecentInstruction {
    pub(crate) info: crate::api::schema::LastInstructionInfo,
    pub(super) until: Instant,
}

/// Recent deliveries per terminal, newest written first.
pub(crate) type RecentInstructions =
    std::collections::HashMap<crate::terminal::TerminalId, Vec<RecentInstruction>>;

/// Sets the outcome of `instruction_id`, adding it as the newest delivery when it is not listed,
/// and keeps it visible for the outcome TTL from now.
pub(super) fn record_instruction_outcome(
    recent: &mut RecentInstructions,
    terminal_id: &crate::terminal::TerminalId,
    instruction_id: &str,
    outcome: InstructionOutcome,
    now: Instant,
) {
    let entries = recent.entry(terminal_id.clone()).or_default();
    let until = now + INSTRUCTION_OUTCOME_TTL;
    if let Some(entry) = entries
        .iter_mut()
        .find(|entry| entry.info.instruction_id == instruction_id)
    {
        entry.info.outcome = outcome;
        entry.until = until;
        return;
    }
    entries.insert(
        0,
        RecentInstruction {
            info: LastInstructionInfo {
                instruction_id: instruction_id.to_string(),
                outcome,
            },
            until,
        },
    );
    entries.truncate(RECENT_INSTRUCTIONS);
}

/// The marked paste the OMP integration consumes:
/// `v3:<id>:<expires unix ms>:<text bytes>:<listener runtime>`. The runtime id is the
/// integration's own random token, known only to it and to herdr, so a person's paste that
/// imitates a block is not taken as an instruction. A listener with action support (integration
/// v14) gets `v4`, whose runtime field is followed by `:<session tag>`: the integration drops a
/// block for a session other than the one it runs now.
fn instruction_block(
    instruction_id: &str,
    text: &str,
    runtime: &str,
    session: Option<&str>,
) -> String {
    match session {
        Some(session) => listener_block(
            "herdr-instruction:v4",
            instruction_id,
            text,
            &format!("{runtime}:{}", session_tag(session)),
            true,
        ),
        None => listener_block("herdr-instruction:v3", instruction_id, text, runtime, false),
    }
}

/// The session a block was checked against, as the OMP integration compares it with the session
/// it runs: the first 16 bytes of the SHA-256 of the session path or id, in hex.
fn session_tag(session: &str) -> String {
    Sha256::digest(session.as_bytes())[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The `agent.action` block: the instruction framing around `{"op","args"}` JSON.
const ACTION_BLOCK: &str = "herdr-action:v1";

/// `end_line` adds a last line `herdr-end:<id>` after the body (action blocks and v4 instruction
/// blocks), so the integration can cut a block that reached OMP's editor out exactly, although
/// OMP's paste handling changes the body (tabs, NFC). The header's length counts the body only.
fn listener_block(kind: &str, block_id: &str, body: &str, runtime: &str, end_line: bool) -> String {
    let expires_ms = (std::time::SystemTime::now() + INSTRUCTION_EXPIRY)
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis());
    let end = if end_line {
        format!("\nherdr-end:{block_id}")
    } else {
        String::new()
    };
    format!(
        "\x1b[200~{kind}:{block_id}:{expires_ms}:{}:{runtime}\n{body}{end}\x1b[201~",
        body.len()
    )
}

/// The latest action per terminal and until when `AgentInfo.last_action` shows it.
pub(crate) type LastActions = std::collections::HashMap<
    crate::terminal::TerminalId,
    (crate::api::schema::LastActionInfo, Instant),
>;

/// Sets the outcome of `action_id` as the terminal's latest action, shown for the outcome TTL.
pub(super) fn record_action_outcome(
    last: &mut LastActions,
    terminal_id: &crate::terminal::TerminalId,
    action_id: &str,
    op: crate::api::schema::AgentActionOp,
    outcome: crate::api::schema::ActionOutcome,
) {
    last.insert(
        terminal_id.clone(),
        (
            crate::api::schema::LastActionInfo {
                action_id: action_id.to_string(),
                op,
                outcome,
            },
            Instant::now() + INSTRUCTION_OUTCOME_TTL,
        ),
    );
}

/// Whether the process that owned a pending action no longer runs: it is not there, or its pid
/// now belongs to another process. A platform that cannot observe processes never says so.
fn owner_process_gone(owner: &crate::platform::OwnerProcessIncarnation) -> bool {
    match crate::platform::observe_process(owner.pid) {
        Ok(None) => true,
        Ok(Some(live)) => live != *owner,
        Err(_) => false,
    }
}

/// An `agent.action` block written to the PTY whose result has not come back.
pub(crate) struct PendingActionAck {
    pub(super) op: crate::api::schema::AgentActionOp,
    pub(super) terminal_id: crate::terminal::TerminalId,
    pub(super) owner: crate::platform::OwnerProcessIncarnation,
    pub(super) runtime: Option<String>,
    pub(super) tx: std::sync::mpsc::Sender<ActionResult>,
    pub(super) deadline: Instant,
    /// Cleared once the listener proved it exists after the block was written (an ack, or a
    /// session report), as for an instruction.
    pub(super) withdraws_listener: bool,
    /// The answer keys of the first ack were written; a second set is refused.
    pub(super) keys_written: bool,
    /// The session file a pending `switch_session` asked for: no other pane may switch to it
    /// until this one has its result.
    pub(super) switch_target: Option<String>,
}

pub(crate) enum ActionResult {
    Done {
        agent: Box<crate::api::schema::AgentInfo>,
        data: serde_json::Map<String, serde_json::Value>,
    },
    Refused(String),
}

/// The two guarded writes to a pane's OMP listener.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ListenerWrite {
    Instruction,
    Action,
}

impl ListenerWrite {
    fn method(self) -> &'static str {
        match self {
            Self::Instruction => "instruct",
            Self::Action => "action",
        }
    }

    fn noun(self) -> &'static str {
        match self {
            Self::Instruction => "instruction",
            Self::Action => "action",
        }
    }

    fn unsupported_code(self) -> &'static str {
        match self {
            Self::Instruction => "agent_instruction_unsupported",
            Self::Action => "agent_action_unsupported",
        }
    }

    fn accepts_field(self) -> &'static str {
        match self {
            Self::Instruction => "accepts_instructions",
            Self::Action => "accepts_actions",
        }
    }

    fn min_integration(self) -> u32 {
        match self {
            Self::Instruction => 13,
            Self::Action => 14,
        }
    }
}

/// A pane whose live OMP listener passed every check for a guarded write.
struct CheckedListener {
    ws_idx: usize,
    pane_id: crate::layout::PaneId,
    agent: crate::api::schema::AgentInfo,
    terminal_id: crate::terminal::TerminalId,
    owner: crate::platform::OwnerProcessIncarnation,
    listener_runtime: Option<String>,
    block_runtime: String,
}

/// A listener runtime id that fits the block header: what the integration mints.
fn valid_listener_runtime(runtime: &str) -> bool {
    (1..=64).contains(&runtime.len())
        && runtime
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

/// Ends a delivery that got no final outcome: its `last_instruction` becomes `unconfirmed`, and a
/// never-taken delivery withdraws the listener of the process it was written to, which may be
/// gone.
fn settle_unconfirmed(
    terminals: &mut std::collections::HashMap<
        crate::terminal::TerminalId,
        crate::terminal::TerminalState,
    >,
    recent: &mut RecentInstructions,
    instruction_id: &str,
    pending: &PendingInstructionAck,
    now: Instant,
) {
    if let Some(terminal) = terminals.get_mut(&pending.terminal_id).filter(|terminal| {
        pending.withdraws_listener && terminal.instruction_listener.as_ref() == Some(&pending.owner)
    }) {
        terminal.instruction_listener = None;
    }
    // An evicted delivery is not listed again.
    if let Some(entry) = recent.get_mut(&pending.terminal_id).and_then(|entries| {
        entries
            .iter_mut()
            .find(|entry| entry.info.instruction_id == instruction_id)
    }) {
        entry.info.outcome = InstructionOutcome::Unconfirmed;
        entry.until = now + INSTRUCTION_OUTCOME_TTL;
    }
}

fn new_instruction_id(request_id: &str, terminal_id: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let digest = Sha256::digest(format!(
        "{request_id}\0{terminal_id}\0{}\0{nanos}",
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

// Codex's Windows input reader does not surface bracketed paste. It detects the prompt as a
// "paste burst" and, while that burst is buffered, rewrites a following Enter into a newline
// instead of submitting. The burst only flushes after an idle timeout, so any size-based delay is
// a timing guess that fails when ConPTY delivery lags it. Codex flushes a buffered burst
// synchronously when it receives a non-character key, so appending one after the paste gives the
// submission a deterministic paste boundary regardless of prompt size or delivery speed.
#[cfg(windows)]
fn append_codex_paste_boundary(runtime: &crate::terminal::TerminalRuntime, text: &mut Vec<u8>) {
    let keys = match crate::app::api_helpers::encode_api_keys(runtime, &["right".to_string()]) {
        Ok(keys) => keys,
        Err(key) => {
            tracing::warn!(key = %key, "failed to encode Codex paste boundary key");
            return;
        }
    };
    if let Some(key) = keys.into_iter().find(|bytes| !bytes.is_empty()) {
        text.extend_from_slice(&key);
    }
}

impl App {
    pub(super) fn handle_agent_list(&mut self, id: String) -> String {
        self.expire_instruction_acks();
        encode_success(
            id,
            ResponseResult::AgentList {
                agents: self.collect_agent_infos(),
            },
        )
    }

    pub(super) fn handle_agent_get(&mut self, id: String, target: AgentTarget) -> String {
        self.expire_instruction_acks();
        self.reconcile_managed_agent_target(&target.target);
        let agent = match self.agent_info_for_target(&target.target) {
            Ok(agent) => agent,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };

        encode_success(id, ResponseResult::AgentInfo { agent })
    }

    pub(super) fn handle_agent_focus(&mut self, id: String, target: AgentTarget) -> String {
        let agent = match self.focus_agent_target(&target.target) {
            Ok(agent) => agent,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };

        encode_success(id, ResponseResult::AgentInfo { agent })
    }

    pub(super) fn handle_agent_rename(&mut self, id: String, params: AgentRenameParams) -> String {
        let agent = match self.rename_agent_target(&params.target, params.name) {
            Ok(agent) => agent,
            Err(err) => return encode_error_body(id, self.agent_rename_error_body(err)),
        };

        encode_success(id, ResponseResult::AgentInfo { agent })
    }

    pub(super) fn handle_agent_start(&mut self, id: String, params: AgentStartParams) -> String {
        let (agent, argv) = match self.start_agent(params) {
            Ok(started) => started,
            Err(err) => return encode_error_body(id, self.agent_start_error_body(err)),
        };

        encode_success(id, ResponseResult::AgentStarted { agent, argv })
    }

    pub(crate) fn handle_deferred_agent_api_request(
        &mut self,
        request: crate::api::schema::Request,
        respond_to: std::sync::mpsc::Sender<String>,
    ) -> bool {
        let params = match request.method {
            crate::api::schema::Method::AgentPrompt(params) => params,
            crate::api::schema::Method::AgentInstruct(params) => {
                if let Err(response) = self.instruct_agent(request.id, params, &respond_to) {
                    let _ = respond_to.send(response);
                }
                return true;
            }
            crate::api::schema::Method::AgentAction(params) => {
                if let Err(response) = self.action_agent(request.id, params, &respond_to) {
                    let _ = respond_to.send(response);
                }
                return true;
            }
            _ => return false,
        };
        match self.queue_agent_prompt(request.id, params) {
            Ok((id, agent, completion)) => {
                std::thread::spawn(move || {
                    let response = match completion.recv() {
                        Ok(Ok(())) => encode_success(id, ResponseResult::AgentPrompted { agent }),
                        Ok(Err(err)) if err.kind() == std::io::ErrorKind::TimedOut => {
                            encode_error(id, "timeout", err.to_string())
                        }
                        Ok(Err(err)) => encode_error(id, "agent_prompt_failed", err.to_string()),
                        Err(_) => encode_error(id, "agent_prompt_failed", "pty actor closed"),
                    };
                    let _ = respond_to.send(response);
                });
            }
            Err(response) => {
                let _ = respond_to.send(response);
            }
        }
        true
    }

    /// Checks identity and writes the marked paste in one app-loop step, so no other request
    /// can retarget the pane in between. The OMP integration consumes the paste before the
    /// editor or any dialog sees it and acks over the socket; a waiter thread answers then.
    fn instruct_agent(
        &mut self,
        id: String,
        params: AgentInstructParams,
        respond_to: &std::sync::mpsc::Sender<String>,
    ) -> Result<(), String> {
        let CheckedListener {
            ws_idx,
            pane_id,
            agent,
            terminal_id,
            owner,
            listener_runtime,
            block_runtime,
        } = self.check_listener_write(
            &id,
            &params.target,
            params.received_at,
            ListenerWrite::Instruction,
            |agent| params.validate(agent),
        )?;
        let instruction_id = new_instruction_id(&id, terminal_id.as_str());
        let session = agent
            .accepts_actions
            .then(|| {
                agent
                    .agent_session
                    .as_ref()
                    .map(|session| session.value.clone())
            })
            .flatten();
        let block = instruction_block(
            &instruction_id,
            &params.text,
            &block_runtime,
            session.as_deref(),
        );
        self.write_listener_block(&id, ws_idx, pane_id, block)?;
        // Acks are handled by this app loop after this step, so registering now misses none.
        let now = Instant::now();
        let deadline = now + INSTRUCTION_ACK_TIMEOUT;
        let (tx, rx) = std::sync::mpsc::channel();
        record_instruction_outcome(
            &mut self.recent_instructions,
            &terminal_id,
            &instruction_id,
            InstructionOutcome::Written,
            now,
        );
        self.pending_instruction_acks.insert(
            instruction_id.clone(),
            PendingInstructionAck {
                terminal_id,
                owner,
                tx,
                deadline,
                withdraws_listener: true,
                runtime: listener_runtime,
            },
        );
        // Read after the write, so the result's `last_instruction` names this delivery.
        let agent = self.agent_info(ws_idx, pane_id).unwrap_or(agent);
        let respond_to = respond_to.clone();
        std::thread::spawn(move || {
            // A `pending` ack means OMP took the text; wait on for the turn start or the drop,
            // and answer `pending` if neither comes in time.
            let mut taken = false;
            let outcome = loop {
                match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(InstructionOutcome::Pending) => taken = true,
                    Ok(outcome) => break Some(outcome),
                    Err(_) => break taken.then_some(InstructionOutcome::Pending),
                }
            };
            let delivered_as = match outcome {
                Some(InstructionOutcome::Prompt) => InstructionDelivery::Prompt,
                Some(InstructionOutcome::Aside) => InstructionDelivery::Aside,
                Some(InstructionOutcome::Pending) => InstructionDelivery::Pending,
                Some(InstructionOutcome::Dropped) => {
                    let _ = respond_to.send(encode_error(
                        id,
                        "instruction_dropped",
                        format!(
                            "OMP took instruction {instruction_id} but did not start its turn (no model or API key, usage limit, Esc or a session change); nothing ran, and OMP may have put the text into an empty editor"
                        ),
                    ));
                    return;
                }
                _ => {
                    let _ = respond_to.send(encode_error(
                        id,
                        "instruction_unconfirmed",
                        format!(
                            "OMP did not confirm taking instruction {instruction_id}; it discards a block it reads more than {} ms late, but a lost confirmation looks the same, so do not send it again automatically",
                            INSTRUCTION_EXPIRY.as_millis()
                        ),
                    ));
                    return;
                }
            };
            let _ = respond_to.send(encode_success(
                id,
                ResponseResult::AgentInstructed {
                    agent,
                    instruction_id,
                    delivered_as,
                },
            ));
        });
        Ok(())
    }

    /// Like `instruct_agent`, for a structured action: the integration runs the op and acks
    /// with its result, which a waiter thread answers with.
    fn action_agent(
        &mut self,
        id: String,
        params: crate::api::schema::AgentActionParams,
        respond_to: &std::sync::mpsc::Sender<String>,
    ) -> Result<(), String> {
        let CheckedListener {
            terminal_id,
            owner,
            listener_runtime,
            block_runtime,
            ws_idx,
            pane_id,
            agent,
        } = self.check_listener_write(
            &id,
            &params.target,
            params.received_at,
            ListenerWrite::Action,
            |agent| params.validate(agent),
        )?;
        let switch_target = (params.op == crate::api::schema::AgentActionOp::SwitchSession)
            .then(|| {
                params
                    .args
                    .get("session_path")
                    .and_then(|path| path.as_str())
            })
            .flatten();
        if let Some(path) = switch_target {
            if self
                .pending_action_acks
                .values()
                .any(|pending| pending.switch_target.as_deref() == Some(path))
            {
                return Err(encode_error(
                    id,
                    "agent_action_refused",
                    "session_in_use: another pane is switching to that session now; nothing was sent"
                        .to_string(),
                ));
            }
            let runs_it = self.collect_agent_infos().into_iter().find(|other| {
                other.terminal_id != agent.terminal_id
                    && other
                        .agent_session
                        .as_ref()
                        .is_some_and(|session| session.value == path)
            });
            if let Some(other) = runs_it {
                return Err(encode_error(
                    id,
                    "agent_action_refused",
                    format!(
                        "session_in_use: agent {} in pane {} runs that session; nothing was sent",
                        other.name.as_deref().unwrap_or("(unnamed)"),
                        other.pane_id
                    ),
                ));
            }
        }
        let action_id = new_instruction_id(&id, terminal_id.as_str());
        // The session herdr checked `expected_session` against: OMP may have changed it since,
        // before its session report reached herdr.
        let session = agent
            .agent_session
            .as_ref()
            .map(|session| session_tag(&session.value));
        let body = serde_json::json!({ "op": params.op, "args": params.args, "session": session })
            .to_string();
        let block = listener_block(ACTION_BLOCK, &action_id, &body, &block_runtime, true);
        self.write_listener_block(&id, ws_idx, pane_id, block)?;
        let deadline = Instant::now() + action_ack_timeout(params.op);
        let (tx, rx) = std::sync::mpsc::channel();
        record_action_outcome(
            &mut self.last_actions,
            &terminal_id,
            &action_id,
            params.op,
            crate::api::schema::ActionOutcome::Written,
        );
        self.pending_action_acks.insert(
            action_id.clone(),
            PendingActionAck {
                op: params.op,
                terminal_id,
                owner,
                runtime: listener_runtime,
                tx,
                deadline,
                withdraws_listener: true,
                keys_written: false,
                switch_target: switch_target.map(str::to_string),
            },
        );
        let op = params.op;
        let respond_to = respond_to.clone();
        std::thread::spawn(move || {
            let response = match rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(ActionResult::Done { agent, data }) => encode_success(
                    id,
                    ResponseResult::AgentActionDone {
                        agent: *agent,
                        action_id,
                        op,
                        data,
                    },
                ),
                Ok(ActionResult::Refused(reason)) => {
                    encode_error(id, "agent_action_refused", reason)
                }
                Err(_) => encode_error(
                    id,
                    "action_unconfirmed",
                    format!(
                        "OMP did not confirm action {action_id}; it discards a block it reads more than {} ms late, but a lost or late result looks the same and the action may have run, so do not send it again automatically",
                        INSTRUCTION_EXPIRY.as_millis()
                    ),
                ),
            };
            let _ = respond_to.send(response);
        });
        Ok(())
    }

    /// The session files that `switch_session` actions still waiting for their result are about
    /// to open, with the terminal that asked.
    pub(crate) fn pending_switch_claims(&self) -> Vec<(crate::terminal::TerminalId, String)> {
        self.pending_action_acks
            .values()
            .filter_map(|pending| {
                pending
                    .switch_target
                    .clone()
                    .map(|target| (pending.terminal_id.clone(), target))
            })
            .collect()
    }

    /// Every check a guarded write to a pane's OMP listener needs, in one app-loop step.
    fn check_listener_write(
        &mut self,
        id: &str,
        target: &str,
        received_at: Option<Instant>,
        write: ListenerWrite,
        validate: impl FnOnce(&crate::api::schema::AgentInfo) -> Result<(), &'static str>,
    ) -> Result<CheckedListener, String> {
        let id = id.to_string();
        let noun = write.noun();
        if !super::super::agents::INSTRUCTIONS_SUPPORTED {
            return Err(encode_error(
                id,
                write.unsupported_code(),
                format!("agent.{} needs process identity and terminal-reader checks this platform lacks", write.method()),
            ));
        }
        // A request that waited long in the server queue would leave the caller too little of its
        // own timeout for the ack wait, and it may have given up already: write nothing.
        if received_at.is_some_and(|received| received.elapsed() > INSTRUCTION_QUEUE_LIMIT) {
            return Err(encode_error(
                id,
                "agent_not_ready",
                format!("the {noun} waited too long in the server queue; nothing was written"),
            ));
        }
        self.expire_instruction_acks();
        self.reconcile_managed_agent_target(target);
        let agent = self
            .agent_info_for_target(target)
            .map_err(|err| encode_error_body(id.clone(), self.agent_target_error_body(err)))?;
        validate(&agent).map_err(|code| {
            encode_error(id.clone(), code, format!("{noun} refused before sending"))
        })?;
        let accepts = match write {
            ListenerWrite::Instruction => agent.accepts_instructions,
            ListenerWrite::Action => agent.accepts_actions,
        };
        if !accepts {
            return Err(encode_error(
                id,
                write.unsupported_code(),
                format!(
                    "agent {target} has no {noun} listener registered now: OMP is restarting, exiting, switching session or reloading its extensions, or its herdr integration is older than v{}; nothing was written; retry when agent.get shows {}",
                    write.min_integration(),
                    write.accepts_field()
                ),
            ));
        }
        let resolved = self
            .resolve_agent_target(target)
            .map_err(|err| encode_error_body(id.clone(), self.agent_target_error_body(err)))?;
        let Some(terminal) = self
            .state
            .workspaces
            .get(resolved.ws_idx)
            .and_then(|workspace| workspace.terminal_id(resolved.pane_id))
            .and_then(|terminal_id| self.state.terminals.get(terminal_id))
        else {
            return Err(agent_not_found(id, target));
        };
        let Some(owner) = terminal.instruction_listener.clone() else {
            return Err(agent_not_found(id, target));
        };
        let listener_runtime = terminal.instruction_listener_runtime.clone();
        // The block is bound to the listener's token; without one it cannot be told apart from a
        // person's paste, so nothing is written.
        let Some(block_runtime) = terminal
            .block_token
            .clone()
            .or_else(|| listener_runtime.clone())
            .filter(|runtime| valid_listener_runtime(runtime))
        else {
            return Err(encode_error(
                id,
                write.unsupported_code(),
                format!(
                    "agent {target} registered no {noun} token; nothing was written; retry when its OMP integration reports again"
                ),
            ));
        };
        let terminal_id = terminal.id.clone();
        let launch_pending = terminal.managed_agent_launch_pending();
        self.settle_stale_deliveries(&terminal_id, &owner, listener_runtime.as_deref());
        // One block per agent until the last one has its result: while a block is
        // unconfirmed the listener may be gone, and a second block would land in the editor.
        // An instruction also waits for its final outcome, so `last_instruction` keeps following
        // a taken delivery that waits for its turn.
        if self
            .pending_action_acks
            .values()
            .any(|pending| pending.terminal_id == terminal_id)
        {
            return Err(encode_error(
                id,
                "agent_not_ready",
                format!("an earlier action to agent {target} has no result yet"),
            ));
        }
        if self.pending_instruction_acks.values().any(|pending| {
            pending.terminal_id == terminal_id
                && (write == ListenerWrite::Instruction || pending.withdraws_listener)
        }) {
            return Err(encode_error(
                id,
                "agent_not_ready",
                format!("an earlier instruction to agent {target} has no final outcome yet"),
            ));
        }
        let Some(runtime) = self.lookup_runtime_sender(resolved.ws_idx, resolved.pane_id) else {
            return Err(agent_not_found(id, target));
        };
        if !super::super::agents::owner_in_foreground_job(runtime, owner.pid) {
            return Err(encode_error(
                id,
                "agent_identity_changed",
                "the registered agent process is not the pane foreground job",
            ));
        }
        if launch_pending || !runtime.bracketed_paste_enabled() {
            return Err(agent_not_ready(id, target));
        }
        #[cfg(target_os = "linux")]
        if super::super::agents::owner_child_reads_tty(runtime, owner.pid) {
            return Err(encode_error(
                id,
                "agent_not_ready",
                format!(
                    "agent {target} has handed the terminal to another program, such as an external editor"
                ),
            ));
        }
        Ok(CheckedListener {
            ws_idx: resolved.ws_idx,
            pane_id: resolved.pane_id,
            agent,
            terminal_id,
            owner,
            listener_runtime,
            block_runtime,
        })
    }

    fn write_listener_block(
        &self,
        id: &str,
        ws_idx: usize,
        pane_id: crate::layout::PaneId,
        block: String,
    ) -> Result<(), String> {
        let Some(runtime) = self.lookup_runtime_sender(ws_idx, pane_id) else {
            return Err(encode_error(
                id.to_string(),
                "agent_not_found",
                "agent pane is gone",
            ));
        };
        runtime
            .try_send_bytes(Bytes::from(block))
            .map_err(|err| encode_error(id.to_string(), "agent_prompt_failed", err.to_string()))
    }

    /// Only the listener a block was written to can answer for it. A delivery written to an
    /// earlier process in the pane, or to the same process before an exec restart replaced its
    /// runtime, never gets a final outcome, so it is settled as `unconfirmed` at once.
    pub(super) fn settle_stale_deliveries(
        &mut self,
        terminal_id: &crate::terminal::TerminalId,
        owner: &crate::platform::OwnerProcessIncarnation,
        runtime: Option<&str>,
    ) {
        let stale: Vec<String> = self
            .pending_instruction_acks
            .iter()
            .filter(|(_, pending)| {
                pending.terminal_id == *terminal_id
                    && (pending.owner != *owner || pending.runtime.as_deref() != runtime)
            })
            .map(|(instruction_id, _)| instruction_id.clone())
            .collect();
        let now = Instant::now();
        for instruction_id in stale {
            if let Some(pending) = self.pending_instruction_acks.remove(&instruction_id) {
                settle_unconfirmed(
                    &mut self.state.terminals,
                    &mut self.recent_instructions,
                    &instruction_id,
                    &pending,
                    now,
                );
            }
        }
        // A dropped sender tells the action's waiter that no result will come.
        let last_actions = &mut self.last_actions;
        self.pending_action_acks.retain(|action_id, pending| {
            let keep = pending.terminal_id != *terminal_id
                || (pending.owner == *owner && pending.runtime.as_deref() == runtime);
            if !keep {
                record_action_outcome(
                    last_actions,
                    &pending.terminal_id,
                    action_id,
                    pending.op,
                    crate::api::schema::ActionOutcome::Unconfirmed,
                );
            }
            keep
        });
    }

    /// Settles deliveries whose wait has passed. A never-taken delivery may mean the listener is
    /// gone (OMP dropped it on a session change or an exec restart), so the agent stops accepting
    /// instructions until the integration reports its listener again; at most one block can then
    /// reach the editor. Either way the outcome becomes `unconfirmed`.
    pub(crate) fn expire_instruction_acks(&mut self) {
        let now = Instant::now();
        let terminals = &mut self.state.terminals;
        let recent = &mut self.recent_instructions;
        self.pending_instruction_acks
            .retain(|instruction_id, pending| {
                if pending.deadline > now {
                    return true;
                }
                settle_unconfirmed(terminals, recent, instruction_id, pending, now);
                false
            });
        // An action block nobody acked may mean the listener is gone, as for an instruction. An
        // action to a closed pane, or to an OMP that exited or was killed while the change ran
        // (the pane stays a shell), gets no result: it ends at once, and with it a switch claim.
        let last_actions = &mut self.last_actions;
        self.pending_action_acks.retain(|action_id, pending| {
            if pending.deadline > now
                && terminals.contains_key(&pending.terminal_id)
                && !owner_process_gone(&pending.owner)
            {
                return true;
            }
            record_action_outcome(
                last_actions,
                &pending.terminal_id,
                action_id,
                pending.op,
                crate::api::schema::ActionOutcome::Unconfirmed,
            );
            if let Some(terminal) = terminals.get_mut(&pending.terminal_id).filter(|terminal| {
                pending.withdraws_listener
                    && terminal.instruction_listener.as_ref() == Some(&pending.owner)
            }) {
                terminal.instruction_listener = None;
            }
            false
        });
        last_actions
            .retain(|terminal_id, (_, until)| *until > now && terminals.contains_key(terminal_id));
        recent.retain(|terminal_id, entries| {
            entries.retain(|entry| entry.until > now);
            !entries.is_empty() && terminals.contains_key(terminal_id)
        });
    }

    fn queue_agent_prompt(
        &mut self,
        id: String,
        params: AgentPromptParams,
    ) -> Result<
        (
            String,
            crate::api::schema::AgentInfo,
            std::sync::mpsc::Receiver<std::io::Result<()>>,
        ),
        String,
    > {
        if params.text.is_empty() {
            return Err(encode_error(
                id,
                "empty_agent_prompt",
                "agent prompt must not be empty",
            ));
        }
        let resolved = match self.resolve_agent_target(&params.target) {
            Ok(resolved) => resolved,
            Err(err) => return Err(encode_error_body(id, self.agent_target_error_body(err))),
        };
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(resolved.ws_idx)
            .and_then(|workspace| workspace.terminal_id(resolved.pane_id))
            .cloned()
        else {
            return Err(agent_not_found(id, &params.target));
        };
        let Some(terminal) = self.state.terminals.get(&terminal_id) else {
            return Err(agent_not_found(id, &params.target));
        };
        if terminal.state == crate::detect::AgentState::Blocked {
            return Err(encode_error(
                id,
                "agent_blocked",
                format!(
                    "agent {} is blocked and requires interactive input",
                    params.target
                ),
            ));
        }
        let Some(expected_agent) = terminal.effective_known_agent() else {
            return Err(agent_not_ready(id, &params.target));
        };
        if terminal.managed_agent_launch_pending() {
            return Err(agent_not_ready(id, &params.target));
        }
        let Some(runtime) = self.lookup_runtime_sender(resolved.ws_idx, resolved.pane_id) else {
            return Err(agent_not_found(id, &params.target));
        };
        if !super::super::agents::runtime_hosts_agent(runtime, expected_agent) {
            return Err(encode_error(
                id,
                "agent_not_ready",
                format!(
                    "agent {} is no longer the pane foreground process",
                    params.target
                ),
            ));
        }
        #[cfg(windows)]
        let submit_deadline = params
            .wait
            .as_ref()
            .and_then(|wait| wait.submission_deadline);
        #[cfg(not(windows))]
        let submit_deadline = None;
        if expected_agent == crate::detect::Agent::GithubCopilot {
            // Copilot ignores synthetic Enter after focus loss until it receives focus gained.
            let focus = match crate::ghostty::encode_focus(crate::ghostty::FocusEvent::Gained) {
                Ok(focus) => focus,
                Err(err) => {
                    return Err(encode_error(id, "agent_prompt_failed", err.to_string()));
                }
            };
            if let Err(err) = runtime.try_send_bytes(Bytes::from(focus)) {
                return Err(encode_error(id, "agent_prompt_failed", err.to_string()));
            }
        }
        let (text, enter) =
            crate::app::api_helpers::encode_api_submission_parts(runtime, &params.text);
        #[cfg(windows)]
        let text = if expected_agent == crate::detect::Agent::Codex {
            let mut text = text;
            append_codex_paste_boundary(runtime, &mut text);
            text
        } else {
            text
        };
        let Some(agent) = self.agent_info(resolved.ws_idx, resolved.pane_id) else {
            return Err(agent_not_found(id, &params.target));
        };
        let completion = runtime
            .queue_user_input_submission(
                Bytes::from(text),
                Bytes::from(enter),
                AGENT_PROMPT_SUBMIT_DELAY,
                submit_deadline,
            )
            .map_err(|err| encode_error(id.clone(), "agent_prompt_failed", err.to_string()))?;
        Ok((id, agent, completion))
    }

    pub(super) fn handle_agent_read(
        &mut self,
        id: String,
        params: crate::api::schema::AgentReadParams,
    ) -> String {
        let resolved = match self.resolve_agent_target(&params.target) {
            Ok(resolved) => resolved,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };
        let Some((pane, workspace_id)) = self.lookup_runtime(resolved.ws_idx, resolved.pane_id)
        else {
            return agent_not_found(id, &params.target);
        };
        let snapshot = crate::app::api_helpers::read_terminal_snapshot(
            pane,
            params.source,
            params.format,
            params.lines,
        );

        encode_success(
            id,
            ResponseResult::PaneRead {
                read: PaneReadResult {
                    pane_id: self
                        .public_pane_id(resolved.ws_idx, resolved.pane_id)
                        .unwrap_or_else(|| params.target.clone()),
                    workspace_id,
                    tab_id: self
                        .public_tab_id(resolved.ws_idx, resolved.tab_idx)
                        .unwrap(),
                    source: params.source,
                    format: params.format,
                    text: snapshot.text,
                    revision: 0,
                    truncated: snapshot.truncated,
                },
            },
        )
    }

    pub(super) fn handle_agent_explain(&mut self, id: String, target: AgentTarget) -> String {
        let resolved = match self.resolve_agent_target(&target.target) {
            Ok(resolved) => resolved,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };
        let Some((pane, _workspace_id)) = self.lookup_runtime(resolved.ws_idx, resolved.pane_id)
        else {
            return agent_not_found(id, &target.target);
        };
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(resolved.ws_idx)
            .and_then(|workspace| workspace.terminal_id(resolved.pane_id))
        else {
            return agent_not_found(id, &target.target);
        };
        let Some(terminal) = self.state.terminals.get(terminal_id) else {
            return agent_not_found(id, &target.target);
        };
        if terminal.full_lifecycle_hook_authority_active() {
            let explain = serde_json::json!({
                "agent": terminal.effective_agent_label().unwrap_or("unknown"),
                "state": crate::detect::manifest::agent_state_label(terminal.state),
                "manifest_source": null,
                "manifest_version": null,
                "cached_remote_version": null,
                "local_override_shadowing_remote": false,
                "remote_update_status": null,
                "remote_update_error": null,
                "matched_rule": null,
                "visible_idle": false,
                "visible_blocker": false,
                "visible_working": false,
                "screen_detection_skipped": true,
                "screen_detection_skip_reason": "full_lifecycle_hook_authority",
                "skip_state_update": false,
                "skipped_update_reason": null,
                "fallback_reason": null,
                "warning": null,
                "evaluated_rules": [],
            });
            return encode_success(id, ResponseResult::AgentExplain { explain });
        }
        let Some(agent) = terminal.effective_known_agent().or(terminal.detected_agent) else {
            return encode_error(
                id,
                "agent_explain_unavailable",
                format!(
                    "agent target {} does not have a detected agent label",
                    target.target
                ),
            );
        };

        let screen = pane.detection_text();
        let osc_title = pane.agent_osc_title();
        let osc_progress = pane.agent_osc_progress();
        let explain = crate::detect::manifest::explain_with_input(
            agent,
            crate::detect::manifest::DetectionInput {
                screen: &screen,
                osc_title: &osc_title,
                osc_progress: &osc_progress,
            },
        );
        let value = crate::detect::manifest::explain_to_json_value(&explain);

        encode_success(id, ResponseResult::AgentExplain { explain: value })
    }

    pub(super) fn handle_agent_send_keys(
        &mut self,
        id: String,
        params: AgentSendKeysParams,
    ) -> String {
        let resolved = match self.resolve_agent_target(&params.target) {
            Ok(resolved) => resolved,
            Err(err) => return encode_error_body(id, self.agent_target_error_body(err)),
        };
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(resolved.ws_idx)
            .and_then(|workspace| workspace.terminal_id(resolved.pane_id))
        else {
            return agent_not_found(id, &params.target);
        };
        let Some(expected_agent) = self
            .state
            .terminals
            .get(terminal_id)
            .and_then(|terminal| terminal.effective_known_agent())
        else {
            return agent_not_ready(id, &params.target);
        };
        let Some(runtime) = self.lookup_runtime_sender(resolved.ws_idx, resolved.pane_id) else {
            return agent_not_found(id, &params.target);
        };
        if !super::super::agents::runtime_hosts_agent(runtime, expected_agent) {
            return agent_not_ready(id, &params.target);
        }
        let encoded = match super::super::api_helpers::encode_api_keys(runtime, &params.keys) {
            Ok(encoded) => encoded,
            Err(key) => {
                return encode_error(id, "invalid_key", format!("unsupported key {key}"));
            }
        };
        let bytes: Vec<u8> = encoded.into_iter().flatten().collect();
        if let Err(err) = runtime.try_send_bytes(Bytes::from(bytes)) {
            return encode_error(id, "agent_send_keys_failed", err.to_string());
        }

        encode_success(id, ResponseResult::Ok {})
    }
}

fn agent_not_ready(id: String, target: &str) -> String {
    encode_error(
        id,
        "agent_not_ready",
        format!("agent {target} is not an active named agent"),
    )
}

fn agent_not_found(id: String, target: &str) -> String {
    encode_error(
        id,
        "agent_not_found",
        format!("agent target {target} not found"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        api::schema::{AgentStatus, SuccessResponse},
        app::Mode,
        config::Config,
        detect::{Agent, AgentState},
        workspace::Workspace,
    };

    fn app_with_agent() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![Workspace::test_new("agent")];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.mode = Mode::Terminal;
        app
    }

    fn start_deferred_agent_prompt(
        app: &mut App,
        id: &str,
        params: AgentPromptParams,
    ) -> std::sync::mpsc::Receiver<String> {
        let (respond_to, response_rx) = std::sync::mpsc::channel();
        assert!(app.handle_deferred_agent_api_request(
            crate::api::schema::Request {
                id: id.into(),
                method: crate::api::schema::Method::AgentPrompt(params),
            },
            respond_to,
        ));
        response_rx
    }

    fn run_deferred_agent_prompt(app: &mut App, id: &str, params: AgentPromptParams) -> String {
        start_deferred_agent_prompt(app, id, params)
            .recv_timeout(Duration::from_secs(1))
            .expect("agent prompt responds after submission")
    }

    #[cfg(target_os = "linux")]
    fn fixture_terminal_id(app: &App) -> crate::terminal::TerminalId {
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone()
    }

    /// An OMP pane registered by this test process, which stands in for the live OMP owner
    /// whose integration listens for instructions, with bracketed paste on.
    #[cfg(target_os = "linux")]
    fn guarded_fixture(
        state: AgentState,
    ) -> (App, AgentInstructParams, tokio::sync::mpsc::Receiver<Bytes>) {
        let owner = crate::platform::observe_process(std::process::id())
            .unwrap()
            .unwrap();
        let (runtime, rx) =
            crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
                80, 24, 0, b"", 4,
            );
        runtime.test_process_pty_bytes(b"\x1b[?2004h");
        let (app, params) = guarded_app(state, owner, runtime);
        (app, params, rx)
    }

    /// An OMP pane whose registered owner and instruction listener is `owner`, on `runtime`.
    #[cfg(target_os = "linux")]
    fn guarded_app(
        state: AgentState,
        owner: crate::platform::OwnerProcessIncarnation,
        runtime: crate::terminal::TerminalRuntime,
    ) -> (App, AgentInstructParams) {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = fixture_terminal_id(&app);
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.cwd = "/fixture/project".into();
        terminal.set_agent_name("director".into());
        terminal.set_detected_state(Some(Agent::Omp), state);
        assert!(terminal
            .set_agent_session_ref_for_session_start_with_recovery(
                "herdr:omp".into(),
                "omp".into(),
                Some(
                    crate::agent_resume::AgentSessionRef::path(fixture_session_path())
                        .expect("fixture session path is absolute on the host platform"),
                ),
                Some(1),
                Some("startup".into()),
                Some(("default".into(), owner.clone())),
            )
            .is_some());
        terminal.instruction_listener = Some(owner);
        terminal.instruction_listener_runtime = Some(FIXTURE_RUNTIME.into());
        app.state.insert_test_runtime(pane_id, runtime);
        let public_pane = app.public_pane_id(0, pane_id).unwrap();
        let info = app.agent_info_for_target(&public_pane).unwrap();
        assert!(info.accepts_instructions);
        let params = AgentInstructParams {
            target: public_pane,
            text: "Report current status".into(),
            expected_terminal_id: info.terminal_id,
            expected_name: info.name,
            expected_agent: info.agent.unwrap(),
            expected_session: info.agent_session.unwrap().value,
            expected_runtime_id: info.runtime_id.unwrap(),
            expected_workspace_id: info.workspace_id,
            // The foreground cwd of a real pane is its process cwd, not the fixture cwd.
            expected_cwd: info.foreground_cwd.or(info.cwd).unwrap(),
            received_at: None,
        };
        (app, params)
    }

    /// The runtime token the fixture's OMP integration registered.
    #[cfg(target_os = "linux")]
    const FIXTURE_RUNTIME: &str = "4f9c2a10-7b3e-4d21-9a55-0c1e2f3a4b5c";

    #[cfg(target_os = "linux")]
    fn fixture_session_path() -> String {
        std::env::current_dir()
            .unwrap()
            .join("omp-native.jsonl")
            .display()
            .to_string()
    }

    /// A v2 session report from this test process, as the OMP integration sends it.
    #[cfg(target_os = "linux")]
    fn report_session(app: &mut App, target: &str, seq: u64, accepts_instructions: bool) -> String {
        report_session_from(
            app,
            target,
            seq,
            accepts_instructions,
            Some(FIXTURE_RUNTIME),
        )
    }

    /// A v2 session report whose listener lives in the JS runtime `runtime`.
    #[cfg(target_os = "linux")]
    fn report_session_from(
        app: &mut App,
        target: &str,
        seq: u64,
        accepts_instructions: bool,
        runtime: Option<&str>,
    ) -> String {
        let pid = std::process::id();
        app.handle_pane_report_agent_session_v2(
            "v2".into(),
            crate::api::schema::PaneReportAgentSessionV2Params {
                pane_id: target.into(),
                source: "herdr:omp".into(),
                agent: "omp".into(),
                seq: Some(seq),
                agent_session_id: None,
                agent_session_path: Some(fixture_session_path()),
                session_start_source: Some("startup".into()),
                launch_profile: "default".into(),
                agent_pid: pid,
                accepts_instructions,
                accepts_actions: accepts_instructions,
                runtime_instance: runtime.map(str::to_string),
                block_token: None,
                peer_pid: Some(pid),
            },
        )
    }

    #[cfg(target_os = "linux")]
    fn start_instruct(
        app: &mut App,
        params: AgentInstructParams,
    ) -> std::sync::mpsc::Receiver<String> {
        let (tx, rx) = std::sync::mpsc::channel();
        assert!(app.handle_deferred_agent_api_request(
            crate::api::schema::Request {
                id: "guarded".into(),
                method: crate::api::schema::Method::AgentInstruct(params)
            },
            tx
        ));
        rx
    }

    #[cfg(target_os = "linux")]
    fn instruct_error(app: &mut App, params: AgentInstructParams) -> String {
        let response = start_instruct(app, params)
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        serde_json::from_str::<crate::api::schema::ErrorResponse>(&response)
            .unwrap()
            .error
            .code
    }

    /// The single write is the marked paste with no Enter; returns its instruction id.
    #[cfg(target_os = "linux")]
    fn sent_instruction_id(rx: &mut tokio::sync::mpsc::Receiver<Bytes>, text: &str) -> String {
        let bytes = rx.try_recv().expect("the instruction was written");
        let written = std::str::from_utf8(&bytes).unwrap();
        // An action listener (integration v14) gets v4, which adds the session tag.
        let header = written
            .strip_prefix("\x1b[200~herdr-instruction:v3:")
            .or_else(|| written.strip_prefix("\x1b[200~herdr-instruction:v4:"))
            .and_then(|rest| rest.split_once('\n'))
            .expect("marked paste")
            .0;
        let fields = header.split(':').collect::<Vec<_>>();
        let [id, expires_ms, length, runtime] = fields[..fields.len().min(4)] else {
            panic!("header {header:?}");
        };
        assert!(
            id.len() == 32
                && id
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        );
        // Expires within the ack timeout, so a block OMP reads late is discarded unanswered.
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let expires_ms: u128 = expires_ms.parse().unwrap();
        assert!(
            expires_ms > now_ms - 1000
                && expires_ms <= now_ms + INSTRUCTION_ACK_TIMEOUT.as_millis()
        );
        assert_eq!(length, text.len().to_string());
        assert!(valid_listener_runtime(runtime), "{runtime:?}");
        let version = if fields.len() == 5 { "v4" } else { "v3" };
        assert_eq!(
            written,
            if version == "v4" {
                format!("\x1b[200~herdr-instruction:v4:{header}\n{text}\nherdr-end:{id}\x1b[201~")
            } else {
                format!("\x1b[200~herdr-instruction:v3:{header}\n{text}\x1b[201~")
            }
        );
        assert!(rx.try_recv().is_err(), "nothing follows the paste");
        id.to_string()
    }

    #[cfg(target_os = "linux")]
    fn ack(
        app: &mut App,
        target: &str,
        instruction_id: &str,
        (peer_pid, agent_pid): (u32, u32),
        outcome: InstructionOutcome,
    ) -> String {
        app.handle_pane_ack_instruction(
            "ack".into(),
            crate::api::schema::PaneAckInstructionParams {
                pane_id: target.into(),
                instruction_id: instruction_id.into(),
                agent_pid,
                outcome,
                peer_pid: Some(peer_pid),
            },
        )
    }

    #[cfg(target_os = "linux")]
    fn error_code(response: &str) -> String {
        serde_json::from_str::<crate::api::schema::ErrorResponse>(response)
            .unwrap()
            .error
            .code
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn agent_instruct_delivers_marked_paste_and_answers_after_owner_ack() {
        let pid = std::process::id();
        // A blocked agent is not refused: OMP queues the message behind its dialog.
        for (state, outcome, delivered_as) in [
            (
                AgentState::Idle,
                InstructionOutcome::Prompt,
                InstructionDelivery::Prompt,
            ),
            (
                AgentState::Blocked,
                InstructionOutcome::Aside,
                InstructionDelivery::Aside,
            ),
        ] {
            let (mut app, mut p, mut rx) = guarded_fixture(state);
            // The header counts UTF-8 bytes, which the integration compares with what arrived.
            p.text = "Report état".into();
            let target = p.target.clone();
            let response = start_instruct(&mut app, p);
            let id = sent_instruction_id(&mut rx, "Report état");
            assert!(response.try_recv().is_err(), "answered before the ack");
            let acked = ack(&mut app, &target, &id, (pid, pid), outcome);
            assert!(serde_json::from_str::<SuccessResponse>(&acked).is_ok());
            let success: SuccessResponse =
                serde_json::from_str(&response.recv_timeout(Duration::from_secs(1)).unwrap())
                    .unwrap();
            let ResponseResult::AgentInstructed {
                agent,
                instruction_id,
                delivered_as: answered,
            } = success.result
            else {
                panic!("expected agent_instructed");
            };
            assert_eq!(instruction_id, id);
            assert_eq!(answered, delivered_as);
            assert_eq!(agent.name.as_deref(), Some("director"));
            // The agent is read after the write, so it names this delivery.
            assert_eq!(
                agent.last_instruction.map(|last| last.instruction_id),
                Some(id.clone())
            );
            // The ack is consumed, so a replay cannot answer anything.
            let replay = ack(&mut app, &target, &id, (pid, pid), outcome);
            assert_eq!(error_code(&replay), "instruction_not_found");
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn agent_instruct_without_ack_is_unconfirmed() {
        let (mut app, p, mut rx) = guarded_fixture(AgentState::Working);
        let response = start_instruct(&mut app, p);
        sent_instruction_id(&mut rx, "Report current status");
        let response = response
            .recv_timeout(INSTRUCTION_ACK_TIMEOUT + Duration::from_secs(1))
            .unwrap();
        assert_eq!(error_code(&response), "instruction_unconfirmed");
        assert!(rx.try_recv().is_err());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn agent_instruct_refuses_process_without_listener() {
        let (mut app, p, mut rx) = guarded_fixture(AgentState::Idle);
        let terminal_id = fixture_terminal_id(&app);
        // A v11 integration registers the process without the listener flag.
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .instruction_listener = None;
        let info = app.agent_info_for_target(&p.target).unwrap();
        assert!(!info.accepts_instructions);
        assert_eq!(info.runtime_id.as_ref(), Some(&p.expected_runtime_id));
        assert_eq!(instruct_error(&mut app, p), "agent_instruction_unsupported");
        assert!(rx.try_recv().is_err());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn agent_instruct_refuses_stale_owner() {
        let (mut app, p, mut rx) = guarded_fixture(AgentState::Idle);
        let terminal_id = fixture_terminal_id(&app);
        // herdr missed the exit: the saved owner names a process incarnation that is gone.
        let saved = app
            .state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .persisted_agent_session
            .as_mut()
            .unwrap();
        let owner = saved.owner_process.as_mut().unwrap();
        owner.start_time_ticks = owner.start_time_ticks.saturating_add(1);
        let info = app.agent_info_for_target(&p.target).unwrap();
        assert_eq!(info.runtime_id, None);
        assert!(!info.accepts_instructions);
        assert_eq!(instruct_error(&mut app, p), "agent_identity_changed");
        assert!(rx.try_recv().is_err());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn agent_instruct_checks_every_identity_field_before_writing() {
        for field in 0..8 {
            let (mut app, mut p, mut rx) = guarded_fixture(AgentState::Idle);
            match field {
                0 => p.expected_terminal_id = "other-terminal".into(),
                1 => p.expected_name = Some("other-name".into()),
                2 => p.expected_name = None,
                3 => p.expected_agent = "other-agent".into(),
                4 => p.expected_session = "restarted-session".into(),
                5 => p.expected_workspace_id = "other-workspace".into(),
                6 => p.expected_cwd = "/other/project".into(),
                _ => p.expected_runtime_id = "previous-process".into(),
            }
            assert_eq!(instruct_error(&mut app, p), "agent_identity_changed");
            assert!(rx.try_recv().is_err());
        }
        // An OMP that a person started by typing `omp` has no herdr name.
        let (mut app, mut p, mut rx) = guarded_fixture(AgentState::Idle);
        let terminal_id = fixture_terminal_id(&app);
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .agent_name = None;
        assert_eq!(
            instruct_error(&mut app, p.clone()),
            "agent_identity_changed"
        );
        assert!(rx.try_recv().is_err());
        p.expected_name = None;
        let _response = start_instruct(&mut app, p);
        sent_instruction_id(&mut rx, "Report current status");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn agent_instruct_refuses_unregistered_process_and_uses_foreground_scope() {
        let (mut app, p, mut rx) = guarded_fixture(AgentState::Idle);
        let mut info = app.agent_info_for_target(&p.target).unwrap();
        info.foreground_cwd = Some("/other/project".into());
        assert_eq!(p.validate(&info), Err("agent_identity_changed"));
        info.foreground_cwd = Some(p.expected_cwd.clone());
        info.cwd = Some("/shell/elsewhere".into());
        assert!(p.validate(&info).is_ok());
        info.foreground_cwd = None;
        assert_eq!(p.validate(&info), Err("agent_identity_changed"));
        info.cwd = Some(p.expected_cwd.clone());
        assert!(p.validate(&info).is_ok());
        info.foreground_cwd = Some(String::new());
        assert!(p.validate(&info).is_ok());
        let terminal_id = fixture_terminal_id(&app);
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .persisted_agent_session
            .as_mut()
            .unwrap()
            .owner_process = None;
        assert_eq!(instruct_error(&mut app, p), "agent_identity_changed");
        assert!(rx.try_recv().is_err());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn agent_instruct_rejects_unbounded_control_and_command_text_without_writing() {
        for text in [
            String::new(),
            " \n".into(),
            "/new".into(),
            " \t/quit".into(),
            "!ls".into(),
            " !!rm -rf build".into(),
            "$ print(1)".into(),
            "$$ print(1)".into(),
            "\n$\tprint(1)".into(),
            "$".into(),
            "hello\x1b[201~".into(),
            "hello\rthere".into(),
            "é".repeat(4097),
        ] {
            let (mut app, mut p, mut rx) = guarded_fixture(AgentState::Idle);
            p.text = text;
            assert_eq!(instruct_error(&mut app, p), "invalid_instruction");
            assert!(rx.try_recv().is_err());
        }
        // Shell-style variables are prose to OMP, as are `$` and `!` after the start.
        let (app, mut p, _rx) = guarded_fixture(AgentState::Idle);
        let info = app.agent_info_for_target(&p.target).unwrap();
        for text in ["$HOME is unset", "${name}", "say ! and $ here"] {
            p.text = text.into();
            assert!(p.validate(&info).is_ok(), "{text:?}");
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn instruction_ack_from_another_process_is_rejected() {
        let pid = std::process::id();
        let (mut app, p, mut rx) = guarded_fixture(AgentState::Idle);
        let target = p.target.clone();
        let response = start_instruct(&mut app, p);
        let id = sent_instruction_id(&mut rx, "Report current status");
        for pids in [(pid + 1, pid), (pid + 1, pid + 1)] {
            let rejected = ack(&mut app, &target, &id, pids, InstructionOutcome::Prompt);
            assert_eq!(error_code(&rejected), "process_mismatch");
        }
        // The socket peer is right, but the pid now names another incarnation.
        let owner = app.pending_instruction_acks[&id].owner.clone();
        app.pending_instruction_acks
            .get_mut(&id)
            .unwrap()
            .owner
            .start_time_ticks += 1;
        let rejected = ack(
            &mut app,
            &target,
            &id,
            (pid, pid),
            InstructionOutcome::Prompt,
        );
        assert_eq!(error_code(&rejected), "process_mismatch");
        app.pending_instruction_acks.get_mut(&id).unwrap().owner = owner;
        assert!(response.try_recv().is_err(), "the waiter is still pending");
        let acked = ack(
            &mut app,
            &target,
            &id,
            (pid, pid),
            InstructionOutcome::Prompt,
        );
        assert!(serde_json::from_str::<SuccessResponse>(&acked).is_ok());
        assert!(response.recv_timeout(Duration::from_secs(1)).is_ok());
    }

    #[cfg(target_os = "linux")]
    fn wait_unconfirmed(response: std::sync::mpsc::Receiver<String>) {
        let response = response
            .recv_timeout(INSTRUCTION_ACK_TIMEOUT + Duration::from_secs(1))
            .unwrap();
        assert_eq!(error_code(&response), "instruction_unconfirmed");
    }

    #[cfg(target_os = "linux")]
    fn instructed_as(response: &str) -> InstructionDelivery {
        let success: SuccessResponse = serde_json::from_str(response).unwrap();
        let ResponseResult::AgentInstructed { delivered_as, .. } = success.result else {
            panic!("expected agent_instructed: {response}");
        };
        delivered_as
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn an_instruction_that_waited_too_long_in_the_server_queue_is_not_written() {
        let (mut app, mut p, mut rx) = guarded_fixture(AgentState::Idle);
        p.received_at = Some(Instant::now() - INSTRUCTION_QUEUE_LIMIT - Duration::from_millis(1));
        assert_eq!(instruct_error(&mut app, p.clone()), "agent_not_ready");
        assert!(rx.try_recv().is_err());
        p.received_at = Some(Instant::now());
        let _response = start_instruct(&mut app, p);
        sent_instruction_id(&mut rx, "Report current status");
    }

    /// OMP took the text on an idle session but its turn starts late (compaction, hooks): the
    /// caller must learn that OMP holds the text, never "not confirmed".
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn an_idle_take_answers_pending_until_the_turn_starts() {
        let pid = std::process::id();
        let (mut app, p, mut rx) = guarded_fixture(AgentState::Idle);
        let target = p.target.clone();
        let take = |app: &mut App, id: &str, delivered_as| {
            let acked = ack(app, &target, id, (pid, pid), delivered_as);
            assert!(
                serde_json::from_str::<SuccessResponse>(&acked).is_ok(),
                "{acked}"
            );
        };

        let response = start_instruct(&mut app, p.clone());
        let id = sent_instruction_id(&mut rx, "Report current status");
        take(&mut app, &id, InstructionOutcome::Pending);
        assert!(response.try_recv().is_err(), "waits for the turn start");
        take(&mut app, &id, InstructionOutcome::Prompt);
        let answered = response.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(instructed_as(&answered), InstructionDelivery::Prompt);

        let response = start_instruct(&mut app, p.clone());
        let id = sent_instruction_id(&mut rx, "Report current status");
        take(&mut app, &id, InstructionOutcome::Pending);
        let answered = response
            .recv_timeout(INSTRUCTION_ACK_TIMEOUT + Duration::from_secs(1))
            .unwrap();
        assert_eq!(instructed_as(&answered), InstructionDelivery::Pending);
        // The take proved the listener, so it stays, and the outcome is still followed: the
        // agent shows it, and a second instruction waits for it.
        let info = app.agent_info_for_target(&target).unwrap();
        assert!(info.accepts_instructions);
        assert_eq!(
            last_outcome(&app, &target, &id),
            InstructionOutcome::Pending
        );
        assert_eq!(instruct_error(&mut app, p.clone()), "agent_not_ready");
        assert!(rx.try_recv().is_err());
        take(&mut app, &id, InstructionOutcome::Prompt);
        assert_eq!(last_outcome(&app, &target, &id), InstructionOutcome::Prompt);
        let _response = start_instruct(&mut app, p);
        sent_instruction_id(&mut rx, "Report current status");
    }

    #[cfg(target_os = "linux")]
    fn last_outcome(app: &App, target: &str, id: &str) -> InstructionOutcome {
        let last = app
            .agent_info_for_target(target)
            .unwrap()
            .last_instruction
            .expect("last instruction");
        assert_eq!(last.instruction_id, id);
        last.outcome
    }

    /// `/restart` execs a new OMP image in the same process: same incarnation, new runtime. Only
    /// that tells herdr the taken delivery is gone; an extension reload keeps the runtime and the
    /// running prompt, so it keeps following the delivery.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn an_exec_restart_settles_the_taken_delivery_but_a_reload_does_not() {
        let pid = std::process::id();
        let (mut app, p, mut rx) = guarded_fixture(AgentState::Idle);
        let target = p.target.clone();
        let reported = report_session_from(&mut app, &target, 10, true, Some("first"));
        assert!(
            serde_json::from_str::<SuccessResponse>(&reported).is_ok(),
            "{reported}"
        );
        let _response = start_instruct(&mut app, p.clone());
        let id = sent_instruction_id(&mut rx, "Report current status");
        let acked = ack(
            &mut app,
            &target,
            &id,
            (pid, pid),
            InstructionOutcome::Pending,
        );
        assert!(
            serde_json::from_str::<SuccessResponse>(&acked).is_ok(),
            "{acked}"
        );

        // Reload: same runtime, the prompt may still start.
        let reported = report_session_from(&mut app, &target, 11, true, Some("first"));
        assert!(
            serde_json::from_str::<SuccessResponse>(&reported).is_ok(),
            "{reported}"
        );
        assert_eq!(
            last_outcome(&app, &target, &id),
            InstructionOutcome::Pending
        );
        assert_eq!(instruct_error(&mut app, p.clone()), "agent_not_ready");
        assert!(rx.try_recv().is_err());

        // Exec restart: new runtime, the old delivery can never finish.
        let reported = report_session_from(&mut app, &target, 12, true, Some("after-exec"));
        assert!(
            serde_json::from_str::<SuccessResponse>(&reported).is_ok(),
            "{reported}"
        );
        assert_eq!(
            last_outcome(&app, &target, &id),
            InstructionOutcome::Unconfirmed
        );
        let _second = start_instruct(&mut app, p);
        let second = sent_instruction_id(&mut rx, "Report current status");
        assert_ne!(second, id);
    }

    /// A block written just before `/restart` is never taken. The restarted image's report settles
    /// it and keeps the listener it registers.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn an_exec_restart_with_an_untaken_block_keeps_the_new_listener() {
        let (mut app, p, mut rx) = guarded_fixture(AgentState::Idle);
        let target = p.target.clone();
        let reported = report_session_from(&mut app, &target, 10, true, Some("first"));
        assert!(
            serde_json::from_str::<SuccessResponse>(&reported).is_ok(),
            "{reported}"
        );
        let _response = start_instruct(&mut app, p.clone());
        let id = sent_instruction_id(&mut rx, "Report current status");

        let reported = report_session_from(&mut app, &target, 11, true, Some("after-exec"));
        assert!(
            serde_json::from_str::<SuccessResponse>(&reported).is_ok(),
            "{reported}"
        );
        assert_eq!(
            last_outcome(&app, &target, &id),
            InstructionOutcome::Unconfirmed
        );
        assert!(
            app.agent_info_for_target(&target)
                .unwrap()
                .accepts_instructions
        );
        let _second = start_instruct(&mut app, p);
        sent_instruction_id(&mut rx, "Report current status");
    }

    /// At shutdown (`/restart` or a reload) the integration reports its listener gone. herdr then
    /// refuses instead of writing a block into the restart gap, and the report from the same
    /// runtime keeps a reload's taken delivery followed; only a new runtime settles it.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_shutdown_report_withdraws_the_listener_and_keeps_its_runtime_deliveries() {
        let pid = std::process::id();
        let (mut app, p, mut rx) = guarded_fixture(AgentState::Idle);
        let target = p.target.clone();
        let reported = report_session_from(&mut app, &target, 10, true, Some("first"));
        assert!(
            serde_json::from_str::<SuccessResponse>(&reported).is_ok(),
            "{reported}"
        );
        let _response = start_instruct(&mut app, p.clone());
        let id = sent_instruction_id(&mut rx, "Report current status");
        let acked = ack(
            &mut app,
            &target,
            &id,
            (pid, pid),
            InstructionOutcome::Pending,
        );
        assert!(
            serde_json::from_str::<SuccessResponse>(&acked).is_ok(),
            "{acked}"
        );

        let reported = report_session_from(&mut app, &target, 11, false, Some("first"));
        assert!(
            serde_json::from_str::<SuccessResponse>(&reported).is_ok(),
            "{reported}"
        );
        assert!(
            !app.agent_info_for_target(&target)
                .unwrap()
                .accepts_instructions
        );
        assert_eq!(
            last_outcome(&app, &target, &id),
            InstructionOutcome::Pending
        );
        let refused = start_instruct(&mut app, p.clone())
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        assert_eq!(error_code(&refused), "agent_instruction_unsupported");
        // The caller learns that this is a passing state and that nothing was written.
        let message = serde_json::from_str::<crate::api::schema::ErrorResponse>(&refused)
            .unwrap()
            .error
            .message;
        assert!(message.contains("restarting"), "{message}");
        assert!(message.contains("nothing was written"), "{message}");
        assert!(rx.try_recv().is_err(), "nothing is written into the gap");

        // The restarted image registers from a new runtime: the old delivery ends, and the new
        // listener takes instructions.
        let reported = report_session_from(&mut app, &target, 12, true, Some("after-exec"));
        assert!(
            serde_json::from_str::<SuccessResponse>(&reported).is_ok(),
            "{reported}"
        );
        assert_eq!(
            last_outcome(&app, &target, &id),
            InstructionOutcome::Unconfirmed
        );
        let _second = start_instruct(&mut app, p);
        sent_instruction_id(&mut rx, "Report current status");
    }

    /// The block carries the listener's runtime token, which a person's paste cannot know; with
    /// no token registered nothing is written.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn the_block_is_bound_to_the_listener_runtime() {
        let (mut app, p, mut rx) = guarded_fixture(AgentState::Idle);
        let _response = start_instruct(&mut app, p.clone());
        let bytes = rx.try_recv().expect("the instruction was written");
        let header = std::str::from_utf8(&bytes)
            .unwrap()
            .split('\n')
            .next()
            .unwrap()
            .to_string();
        assert!(header.ends_with(&format!(":{FIXTURE_RUNTIME}")), "{header}");

        let (mut app, p, mut rx) = guarded_fixture(AgentState::Idle);
        let terminal_id = fixture_terminal_id(&app);
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .instruction_listener_runtime = None;
        assert_eq!(instruct_error(&mut app, p), "agent_instruction_unsupported");
        assert!(rx.try_recv().is_err());
    }

    /// A shutdown report (OMP `/restart` or reload) marks the listener withdrawn until the next
    /// registration; an agent that never registered one is not marked.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_withdrawn_listener_is_told_apart_from_one_never_registered() {
        let (mut app, p, _rx) = guarded_fixture(AgentState::Idle);
        let target = p.target;
        let row = |app: &App| {
            let info = app.agent_info_for_target(&target).unwrap();
            (info.accepts_instructions, info.listener_withdrawn)
        };
        let report = |app: &mut App, seq, accepts, runtime| {
            let reported = report_session_from(app, &target, seq, accepts, Some(runtime));
            assert!(
                serde_json::from_str::<SuccessResponse>(&reported).is_ok(),
                "{reported}"
            );
        };
        assert_eq!(row(&app), (true, false));
        report(&mut app, 10, false, FIXTURE_RUNTIME);
        assert_eq!(row(&app), (false, true));
        // A repeated report without a listener keeps the mark.
        report(&mut app, 11, false, FIXTURE_RUNTIME);
        assert_eq!(row(&app), (false, true));
        report(&mut app, 12, true, "after-exec");
        assert_eq!(row(&app), (true, false));

        // Not forever: an image that never registers again is not "restarting".
        report(&mut app, 13, false, "after-exec");
        let terminal_id = fixture_terminal_id(&app);
        let withdrawn = app
            .state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .instruction_listener_withdrawn
            .as_mut()
            .unwrap();
        withdrawn.1 -= crate::app::agents::LISTENER_WITHDRAWN_FOR;
        assert_eq!(row(&app), (false, false));

        // A process that never had a listener (integration without one) is not marked.
        let (mut app, p, _rx) = guarded_fixture(AgentState::Idle);
        let terminal_id = fixture_terminal_id(&app);
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .instruction_listener = None;
        let reported = report_session_from(&mut app, &p.target, 10, false, Some(FIXTURE_RUNTIME));
        assert!(
            serde_json::from_str::<SuccessResponse>(&reported).is_ok(),
            "{reported}"
        );
        let info = app.agent_info_for_target(&p.target).unwrap();
        assert!(!info.accepts_instructions && !info.listener_withdrawn);
    }

    /// A later delivery does not hide an earlier one's outcome from a caller that follows it.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn recent_instructions_keep_earlier_outcomes() {
        let pid = std::process::id();
        let (mut app, p, mut rx) = guarded_fixture(AgentState::Idle);
        let target = p.target.clone();
        let mut ids = Vec::new();
        for _ in 0..RECENT_INSTRUCTIONS + 1 {
            let response = start_instruct(&mut app, p.clone());
            let id = sent_instruction_id(&mut rx, "Report current status");
            let acked = ack(
                &mut app,
                &target,
                &id,
                (pid, pid),
                InstructionOutcome::Prompt,
            );
            assert!(
                serde_json::from_str::<SuccessResponse>(&acked).is_ok(),
                "{acked}"
            );
            assert!(response.recv_timeout(Duration::from_secs(1)).is_ok());
            ids.push(id);
        }
        let info = app.agent_info_for_target(&target).unwrap();
        let listed: Vec<_> = info
            .recent_instructions
            .iter()
            .map(|entry| (entry.instruction_id.clone(), entry.outcome))
            .collect();
        let expected: Vec<_> = ids
            .iter()
            .rev()
            .take(RECENT_INSTRUCTIONS)
            .map(|id| (id.clone(), InstructionOutcome::Prompt))
            .collect();
        assert_eq!(listed, expected, "newest first, the oldest evicted");
        assert_eq!(
            info.last_instruction,
            info.recent_instructions.first().cloned()
        );
    }

    /// OMP took a delivery and exited before its turn: only that process could answer, so the
    /// next OMP in the pane must not wait two minutes for it.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_delivery_to_an_earlier_process_does_not_block_the_next_one() {
        let pid = std::process::id();
        let (mut app, p, mut rx) = guarded_fixture(AgentState::Idle);
        let target = p.target.clone();
        let _first = start_instruct(&mut app, p.clone());
        let id = sent_instruction_id(&mut rx, "Report current status");
        let acked = ack(
            &mut app,
            &target,
            &id,
            (pid, pid),
            InstructionOutcome::Pending,
        );
        assert!(
            serde_json::from_str::<SuccessResponse>(&acked).is_ok(),
            "{acked}"
        );
        // The process that took it is gone; the pane's listener is a newer process.
        app.pending_instruction_acks
            .get_mut(&id)
            .unwrap()
            .owner
            .start_time_ticks += 1;

        let _second = start_instruct(&mut app, p);
        let second = sent_instruction_id(&mut rx, "Report current status");
        assert!(!app.pending_instruction_acks.contains_key(&id));
        assert_eq!(
            last_outcome(&app, &target, &second),
            InstructionOutcome::Written
        );
    }

    /// OMP took the text but went idle without starting its turn (no API key, Esc): the
    /// caller learns that nothing ran, whether the drop comes inside the wait or after it.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_dropped_take_is_reported_as_dropped() {
        let pid = std::process::id();
        let (mut app, p, mut rx) = guarded_fixture(AgentState::Idle);
        let target = p.target.clone();
        let take = |app: &mut App, id: &str, outcome| {
            let acked = ack(app, &target, id, (pid, pid), outcome);
            assert!(
                serde_json::from_str::<SuccessResponse>(&acked).is_ok(),
                "{acked}"
            );
        };

        let response = start_instruct(&mut app, p.clone());
        let id = sent_instruction_id(&mut rx, "Report current status");
        assert_eq!(
            last_outcome(&app, &target, &id),
            InstructionOutcome::Written
        );
        take(&mut app, &id, InstructionOutcome::Pending);
        take(&mut app, &id, InstructionOutcome::Dropped);
        let answered = response.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(error_code(&answered), "instruction_dropped");
        assert_eq!(
            last_outcome(&app, &target, &id),
            InstructionOutcome::Dropped
        );

        // Dropped after the wait answered `pending`.
        let response = start_instruct(&mut app, p.clone());
        let id = sent_instruction_id(&mut rx, "Report current status");
        take(&mut app, &id, InstructionOutcome::Pending);
        let answered = response
            .recv_timeout(INSTRUCTION_ACK_TIMEOUT + Duration::from_secs(1))
            .unwrap();
        assert_eq!(instructed_as(&answered), InstructionDelivery::Pending);
        take(&mut app, &id, InstructionOutcome::Dropped);
        assert_eq!(
            last_outcome(&app, &target, &id),
            InstructionOutcome::Dropped
        );

        // Only the integration's outcomes are accepted.
        let response = start_instruct(&mut app, p);
        let id = sent_instruction_id(&mut rx, "Report current status");
        for invalid in [InstructionOutcome::Written, InstructionOutcome::Unconfirmed] {
            let refused = ack(&mut app, &target, &id, (pid, pid), invalid);
            assert_eq!(error_code(&refused), "invalid_request");
        }
        // Nothing final within the outcome window: unknown.
        wait_unconfirmed(response);
        let deadline = Instant::now() + INSTRUCTION_OUTCOME_TTL + Duration::from_secs(1);
        while app.pending_instruction_acks.contains_key(&id) && Instant::now() < deadline {
            app.expire_instruction_acks();
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            last_outcome(&app, &target, &id),
            InstructionOutcome::Unconfirmed
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_second_instruction_waits_until_the_first_is_taken() {
        let pid = std::process::id();
        let (mut app, p, mut rx) = guarded_fixture(AgentState::Working);
        let target = p.target.clone();
        let first = start_instruct(&mut app, p.clone());
        let id = sent_instruction_id(&mut rx, "Report current status");
        assert_eq!(instruct_error(&mut app, p.clone()), "agent_not_ready");
        assert!(
            rx.try_recv().is_err(),
            "one block while the first is unconfirmed"
        );

        let acked = ack(
            &mut app,
            &target,
            &id,
            (pid, pid),
            InstructionOutcome::Aside,
        );
        assert!(
            serde_json::from_str::<SuccessResponse>(&acked).is_ok(),
            "{acked}"
        );
        assert!(first.recv_timeout(Duration::from_secs(1)).is_ok());
        let second = start_instruct(&mut app, p);
        sent_instruction_id(&mut rx, "Report current status");
        wait_unconfirmed(second);
        // Reads settle the expired delivery without another instruct.
        let response = app.handle_agent_get(
            "get".into(),
            AgentTarget {
                target: target.clone(),
            },
        );
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::AgentInfo { agent } = success.result else {
            panic!("expected agent info");
        };
        assert!(!agent.accepts_instructions);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn unconfirmed_delivery_withdraws_the_listener_until_the_integration_reports_again() {
        let (mut app, p, mut rx) = guarded_fixture(AgentState::Idle);
        let target = p.target.clone();
        // OMP dropped its listener without telling herdr: the block is never acked.
        wait_unconfirmed(start_instruct(&mut app, p.clone()));
        sent_instruction_id(&mut rx, "Report current status");
        assert_eq!(
            instruct_error(&mut app, p.clone()),
            "agent_instruction_unsupported"
        );
        assert!(
            rx.try_recv().is_err(),
            "at most one block reaches the editor"
        );
        assert!(
            !app.agent_info_for_target(&target)
                .unwrap()
                .accepts_instructions
        );

        let reported = report_session(&mut app, &target, 10, true);
        assert!(
            serde_json::from_str::<SuccessResponse>(&reported).is_ok(),
            "{reported}"
        );
        assert!(
            app.agent_info_for_target(&target)
                .unwrap()
                .accepts_instructions
        );
        let _response = start_instruct(&mut app, p);
        sent_instruction_id(&mut rx, "Report current status");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_listener_reported_after_a_delivery_outlives_that_delivery() {
        let (mut app, p, mut rx) = guarded_fixture(AgentState::Idle);
        let target = p.target.clone();
        let response = start_instruct(&mut app, p.clone());
        sent_instruction_id(&mut rx, "Report current status");
        // The integration dropped this block (late or cut short) and reported its listener.
        let reported = report_session(&mut app, &target, 10, true);
        assert!(
            serde_json::from_str::<SuccessResponse>(&reported).is_ok(),
            "{reported}"
        );
        wait_unconfirmed(response);
        let _response = start_instruct(&mut app, p);
        sent_instruction_id(&mut rx, "Report current status");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn session_reports_grant_and_withdraw_the_instruction_listener() {
        let (mut app, p, _rx) = guarded_fixture(AgentState::Idle);
        let target = p.target;
        let accepts = |app: &App| {
            app.agent_info_for_target(&target)
                .unwrap()
                .accepts_instructions
        };
        // An integration without a listener (v11, or no terminal input) withdraws it.
        let reported = report_session(&mut app, &target, 10, false);
        assert!(
            serde_json::from_str::<SuccessResponse>(&reported).is_ok(),
            "{reported}"
        );
        assert!(!accepts(&app));
        let reported = report_session(&mut app, &target, 11, true);
        assert!(
            serde_json::from_str::<SuccessResponse>(&reported).is_ok(),
            "{reported}"
        );
        assert!(accepts(&app));
        // A rejected report changes nothing.
        assert_eq!(
            error_code(&report_session(&mut app, &target, 11, false)),
            "stale_report"
        );
        assert!(accepts(&app));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn late_or_misdirected_instruction_acks_are_not_found() {
        let pid = std::process::id();
        let (mut app, p, mut rx) = guarded_fixture(AgentState::Idle);
        let target = p.target.clone();
        let response = start_instruct(&mut app, p);
        let id = sent_instruction_id(&mut rx, "Report current status");
        let pending = app.pending_instruction_acks.get_mut(&id).unwrap();
        let terminal_id = std::mem::replace(
            &mut pending.terminal_id,
            crate::terminal::TerminalId::alloc(),
        );
        let misdirected = ack(
            &mut app,
            &target,
            &id,
            (pid, pid),
            InstructionOutcome::Prompt,
        );
        assert_eq!(error_code(&misdirected), "instruction_not_found");
        let pending = app.pending_instruction_acks.get_mut(&id).unwrap();
        pending.terminal_id = terminal_id;
        pending.deadline = Instant::now() - Duration::from_millis(1);
        let late = ack(
            &mut app,
            &target,
            &id,
            (pid, pid),
            InstructionOutcome::Prompt,
        );
        assert_eq!(error_code(&late), "instruction_not_found");
        wait_unconfirmed(response);
    }

    /// OMP's external editor (Ctrl+G) is a child of OMP in its foreground job that reads the
    /// pane; a paste would land in the person's file. `cat` plays the editor here.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn agent_instruct_refuses_while_a_child_of_the_agent_reads_the_terminal() {
        let pane_id = app_with_agent().state.workspaces[0].tabs[0].root_pane;
        let (events, _events_rx) = tokio::sync::mpsc::channel(32);
        let runtime = crate::terminal::TerminalRuntime::spawn(
            pane_id,
            24,
            80,
            std::env::temp_dir(),
            0,
            Default::default(),
            None,
            crate::pane::PaneShellConfig::new("/bin/sh", crate::config::ShellModeConfig::NonLogin),
            &crate::pane::PaneLaunchEnv::default(),
            events,
            std::sync::Arc::new(tokio::sync::Notify::new()),
            std::sync::Arc::new(crate::render_signal::RenderSignal::new()),
        )
        .unwrap();
        let owner_pid = runtime.child_pid().unwrap();
        // The pane shell becomes the "agent"; its child `cat` reads the terminal, then the agent
        // replaces itself with `sleep` (same process incarnation) once `cat` ends.
        runtime
            .try_send_bytes(Bytes::from_static(
                b"printf '\\033[?2004h'; exec sh -c 'cat; exec sleep 30'\n",
            ))
            .unwrap();
        let job_size =
            || crate::detect::foreground_job(owner_pid).map_or(0, |job| job.processes.len());
        let deadline = Instant::now() + Duration::from_secs(5);
        while (job_size() != 2 || !runtime.bracketed_paste_enabled()) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(job_size(), 2, "the agent and its tty-reading child");
        let owner = crate::platform::observe_process(owner_pid)
            .unwrap()
            .unwrap();
        let (mut app, p) = guarded_app(AgentState::Idle, owner, runtime);
        let response = instruct_error(&mut app, p.clone());
        assert_eq!(response, "agent_not_ready");

        let editor = crate::detect::foreground_job(owner_pid)
            .unwrap()
            .processes
            .into_iter()
            .find(|process| process.pid != owner_pid)
            .unwrap()
            .pid;
        unsafe { libc::kill(editor as i32, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(5);
        while job_size() != 1 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // With the terminal back, the same agent gets the paste (and no ack: unconfirmed).
        wait_unconfirmed(start_instruct(&mut app, p));
        unsafe { libc::kill(owner_pid as i32, libc::SIGKILL) };
    }

    #[test]
    fn agent_instruct_wire_requires_guards_and_refuses_unknown_parameters() {
        let params = serde_json::json!({"target":"w1:p1","text":"Report status","expected_terminal_id":"term1","expected_name":"director","expected_agent":"omp","expected_session":"native1","expected_runtime_id":"process1","expected_workspace_id":"w1","expected_cwd":"/project"});
        let request = serde_json::json!({"id":"guarded","method":"agent.instruct","params":params});
        let parsed: crate::api::schema::Request = serde_json::from_value(request.clone()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), request);
        let mut unnamed = request.clone();
        unnamed["params"]
            .as_object_mut()
            .unwrap()
            .remove("expected_name");
        let parsed: crate::api::schema::Request = serde_json::from_value(unnamed).unwrap();
        let crate::api::schema::Method::AgentInstruct(parsed) = parsed.method else {
            panic!("expected agent.instruct");
        };
        assert_eq!(parsed.expected_name, None);
        for field in [
            "expected_terminal_id",
            "expected_agent",
            "expected_session",
            "expected_runtime_id",
            "expected_workspace_id",
            "expected_cwd",
        ] {
            let mut broken = request.clone();
            broken["params"].as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<crate::api::schema::Request>(broken).is_err());
        }
        let mut broken = request;
        broken["params"]["keys"] = serde_json::json!(["enter"]);
        assert!(serde_json::from_value::<crate::api::schema::Request>(broken).is_err());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_codex_prompt_flushes_paste_burst_before_enter() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::Codex), AgentState::Idle);
        let (runtime, mut rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.state.insert_test_runtime(pane_id, runtime);

        let response = run_deferred_agent_prompt(
            &mut app,
            "req",
            AgentPromptParams {
                target: "reviewer".into(),
                text: "A != B".into(),
                wait: None,
            },
        );
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(
            success.result,
            ResponseResult::AgentPrompted { .. }
        ));
        // The non-character key must precede Enter so Codex commits the paste burst first.
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"A != B\x1b[C"));
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"\r"));
    }

    #[tokio::test]
    async fn a_false_process_exit_makes_a_named_live_agent_unreachable_by_name() {
        // Reproduces the registration loss reported on #3225 by rszrszrsz:
        // a live agent pane with an assigned name stops resolving by that name
        // while its process keeps running, and renaming is the only recovery.
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let observed_at = std::time::Instant::now();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_detected_state(Some(Agent::Pi), AgentState::Working);
        terminal.set_agent_name("reviewer".into());

        let found = app.handle_agent_get(
            "req:before".into(),
            AgentTarget {
                target: "reviewer".into(),
            },
        );
        assert!(
            serde_json::from_str::<SuccessResponse>(&found).is_ok(),
            "the assigned name must resolve while the agent is running: {found}"
        );

        // One process-exit observation, then the same agent is observed alive
        // again on the next probe - the process never actually went away.
        app.handle_internal_event(crate::events::AppEvent::StateChanged {
            pane_id,
            agent: Some(Agent::Pi),
            state: AgentState::Idle,
            visible_blocker: false,
            visible_working: false,
            process_exited: true,
            observed_at,
        });
        app.handle_internal_event(crate::events::AppEvent::AgentProcessDetected {
            pane_id,
            agent: Agent::Pi,
            observed_at: observed_at + std::time::Duration::from_secs(1),
        });

        let terminal = &app.state.terminals[&terminal_id];
        assert_eq!(
            terminal.detected_agent,
            Some(Agent::Pi),
            "the agent process is still there"
        );

        let after = app.handle_agent_get(
            "req:after".into(),
            AgentTarget {
                target: "reviewer".into(),
            },
        );
        assert!(
            serde_json::from_str::<SuccessResponse>(&after).is_ok(),
            "a live agent must stay reachable by its assigned name: {after}"
        );
    }

    #[tokio::test]
    async fn agent_prompt_sends_text_then_delays_enter() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::OpenCode), AgentState::Working);
        let (runtime, mut rx) =
            crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
                80, 24, 0, b"", 2,
            );
        runtime.test_process_pty_bytes(b"\x1b[?2004h");
        app.state.insert_test_runtime(pane_id, runtime);

        let public_pane_id = app.public_pane_id(0, pane_id).unwrap();
        let bracketed_started = std::time::Instant::now();
        let response_rx = start_deferred_agent_prompt(
            &mut app,
            "req",
            AgentPromptParams {
                target: public_pane_id,
                text: "A != B".into(),
                wait: None,
            },
        );
        assert!(response_rx.try_recv().is_err());
        let response = response_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("agent prompt responds after submission");
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::AgentPrompted { agent, .. } = success.result else {
            panic!("expected prompted response");
        };
        assert_eq!(agent.name.as_deref(), Some("reviewer"));
        assert_eq!(
            rx.try_recv().unwrap(),
            Bytes::from_static(b"\x1b[200~A != B\x1b[201~")
        );
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"\r"));
        assert!(bracketed_started.elapsed() >= AGENT_PROMPT_SUBMIT_DELAY);

        app.lookup_runtime_sender(0, pane_id)
            .unwrap()
            .test_process_pty_bytes(b"\x1b[?2004l");
        let raw_started = std::time::Instant::now();
        let raw = run_deferred_agent_prompt(
            &mut app,
            "req-raw",
            AgentPromptParams {
                target: "reviewer".into(),
                text: "A != B".into(),
                wait: None,
            },
        );
        let raw: SuccessResponse = serde_json::from_str(&raw).unwrap();
        assert!(matches!(raw.result, ResponseResult::AgentPrompted { .. }));
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"A != B"));
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"\r"));
        assert!(raw_started.elapsed() >= AGENT_PROMPT_SUBMIT_DELAY);

        let rejected = run_deferred_agent_prompt(
            &mut app,
            "req-label",
            AgentPromptParams {
                target: "opencode".into(),
                text: "wrong target".into(),
                wait: None,
            },
        );
        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&rejected).unwrap();
        assert_eq!(error.error.code, "agent_not_found");
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn agent_prompt_rejects_blocked_agent_without_writing() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::GithubCopilot), AgentState::Blocked);
        let (runtime, mut rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.state.insert_test_runtime(pane_id, runtime);

        let response = run_deferred_agent_prompt(
            &mut app,
            "req",
            AgentPromptParams {
                target: "reviewer".into(),
                text: "unrelated prompt".into(),
                wait: None,
            },
        );

        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "agent_blocked");
        assert!(
            tokio::time::timeout(
                AGENT_PROMPT_SUBMIT_DELAY + Duration::from_millis(100),
                rx.recv()
            )
            .await
            .is_err(),
            "blocked prompt wrote or scheduled terminal input"
        );
    }

    #[tokio::test]
    async fn agent_prompt_focuses_copilot_before_submitting() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::GithubCopilot), AgentState::Idle);
        let (runtime, mut rx) =
            crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
                80, 24, 0, b"", 3,
            );
        runtime.test_process_pty_bytes(b"\x1b[?2004h");
        app.state.insert_test_runtime(pane_id, runtime);

        let response = run_deferred_agent_prompt(
            &mut app,
            "req",
            AgentPromptParams {
                target: "reviewer".into(),
                text: "A != B".into(),
                wait: None,
            },
        );
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        assert!(matches!(
            success.result,
            ResponseResult::AgentPrompted { .. }
        ));
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"\x1b[I"));
        assert_eq!(
            rx.try_recv().unwrap(),
            Bytes::from_static(b"\x1b[200~A != B\x1b[201~")
        );
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"\r"));
    }

    #[tokio::test]
    async fn agent_send_keys_validates_every_key_before_writing() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_agent_name("reviewer".into());
        terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
        let (runtime, mut rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.state.insert_test_runtime(pane_id, runtime);

        let rejected = app.handle_agent_send_keys(
            "req-invalid".into(),
            AgentSendKeysParams {
                target: "reviewer".into(),
                keys: vec!["enter".into(), "not-a-key".into()],
            },
        );
        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&rejected).unwrap();
        assert_eq!(error.error.code, "invalid_key");
        assert!(rx.try_recv().is_err());

        let sent = app.handle_agent_send_keys(
            "req-valid".into(),
            AgentSendKeysParams {
                target: "reviewer".into(),
                keys: vec!["up".into(), "enter".into()],
            },
        );
        let success: SuccessResponse = serde_json::from_str(&sent).unwrap();
        assert!(matches!(success.result, ResponseResult::Ok {}));
        assert_eq!(rx.try_recv().unwrap(), Bytes::from_static(b"\x1b[A\r"));
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn agent_prompt_rejects_managed_agent_while_startup_is_pending() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        let now = std::time::Instant::now();
        terminal.begin_managed_agent(
            "reviewer".into(),
            Agent::OpenCode,
            now,
            std::time::Duration::from_secs(3),
            std::time::Duration::from_secs(10),
        );
        terminal.set_detected_state(Some(Agent::OpenCode), AgentState::Idle);
        let (runtime, mut rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.state.insert_test_runtime(pane_id, runtime);

        let response = run_deferred_agent_prompt(
            &mut app,
            "req-pending",
            AgentPromptParams {
                target: "reviewer".into(),
                text: "A != B".into(),
                wait: None,
            },
        );
        let error: crate::api::schema::ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "agent_not_ready");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn agent_focus_marks_already_focused_done_agent_seen() {
        let mut app = app_with_agent();
        app.state.outer_terminal_focus = Some(false);

        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .set_detected_state(Some(Agent::Pi), AgentState::Idle);
        app.state.workspaces[0].tabs[0]
            .panes
            .get_mut(&pane_id)
            .unwrap()
            .seen = false;
        app.state.workspaces[0].tabs[0].layout.focus_pane(pane_id);

        let response = app.handle_agent_focus(
            "req".into(),
            AgentTarget {
                target: app.public_pane_id(0, pane_id).unwrap(),
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::AgentInfo { agent } = success.result else {
            panic!("expected agent info response");
        };
        assert_eq!(agent.agent_status, AgentStatus::Idle);
    }

    #[test]
    fn agent_rename_does_not_replace_the_pane_label() {
        let mut app = app_with_agent();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let terminal_id = app.state.workspaces[0].tabs[0].panes[&pane_id]
            .attached_terminal_id
            .clone();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.set_manual_label("shell-pane".into());
        terminal.set_detected_state(Some(Agent::Pi), AgentState::Idle);
        let target = app.public_pane_id(0, pane_id).unwrap();

        for name in [Some("reviewer".to_string()), None] {
            let response = app.handle_agent_rename(
                "req".into(),
                AgentRenameParams {
                    target: target.clone(),
                    name,
                },
            );
            let success: SuccessResponse = serde_json::from_str(&response).unwrap();
            assert!(matches!(success.result, ResponseResult::AgentInfo { .. }));
            assert_eq!(
                app.state.terminals[&terminal_id].manual_label.as_deref(),
                Some("shell-pane")
            );
        }
    }

    /// `agent.action` params for the fixture agent that `instruct` params were made for.
    #[cfg(target_os = "linux")]
    fn action_params(
        instruct: &AgentInstructParams,
        op: crate::api::schema::AgentActionOp,
        args: serde_json::Value,
    ) -> crate::api::schema::AgentActionParams {
        crate::api::schema::AgentActionParams {
            target: instruct.target.clone(),
            op,
            args: args.as_object().unwrap().clone(),
            expected_terminal_id: instruct.expected_terminal_id.clone(),
            expected_name: instruct.expected_name.clone(),
            expected_agent: instruct.expected_agent.clone(),
            expected_session: instruct.expected_session.clone(),
            expected_runtime_id: instruct.expected_runtime_id.clone(),
            expected_workspace_id: instruct.expected_workspace_id.clone(),
            expected_cwd: instruct.expected_cwd.clone(),
            received_at: None,
        }
    }

    #[cfg(target_os = "linux")]
    fn start_action(
        app: &mut App,
        params: crate::api::schema::AgentActionParams,
    ) -> std::sync::mpsc::Receiver<String> {
        let (tx, rx) = std::sync::mpsc::channel();
        assert!(app.handle_deferred_agent_api_request(
            crate::api::schema::Request {
                id: "action".into(),
                method: crate::api::schema::Method::AgentAction(params)
            },
            tx
        ));
        rx
    }

    /// The written `herdr-action:v1` block's id and JSON body.
    #[cfg(target_os = "linux")]
    fn sent_action(rx: &mut tokio::sync::mpsc::Receiver<Bytes>) -> (String, serde_json::Value) {
        let bytes = rx.try_recv().expect("the action was written");
        let written = std::str::from_utf8(&bytes).unwrap();
        let (header, body) = written
            .strip_prefix("\x1b[200~herdr-action:v1:")
            .and_then(|rest| rest.strip_suffix("\x1b[201~"))
            .and_then(|rest| rest.split_once('\n'))
            .expect("marked action block");
        let fields: Vec<_> = header.split(':').collect();
        // The body ends with the end line that names the block.
        let body = body
            .strip_suffix(&format!("\nherdr-end:{}", fields[0]))
            .expect("end line");
        assert_eq!(fields[2].parse::<usize>().unwrap(), body.len());
        assert_eq!(fields[3], FIXTURE_RUNTIME);
        (fields[0].to_string(), serde_json::from_str(body).unwrap())
    }

    #[cfg(target_os = "linux")]
    fn ack_action(
        app: &mut App,
        target: &str,
        action_id: &str,
        peer_pid: u32,
        ok: bool,
        error: Option<&str>,
        keys: Vec<String>,
    ) -> String {
        app.handle_pane_ack_action(
            "ack".into(),
            crate::api::schema::PaneAckActionParams {
                pane_id: target.into(),
                action_id: action_id.into(),
                agent_pid: std::process::id(),
                ok,
                error: error.map(str::to_string),
                data: serde_json::json!({ "dialog_id": "c1" })
                    .as_object()
                    .unwrap()
                    .clone(),
                keys,
                peer_pid: Some(peer_pid),
            },
        )
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn agent_action_reaches_only_a_listener_with_action_support_and_returns_its_result() {
        use crate::api::schema::AgentActionOp;
        let (mut app, instruct, mut rx) = guarded_fixture(AgentState::Blocked);
        let target = instruct.target.clone();
        let answer = serde_json::json!({ "dialog_id": "c1", "approve": false });
        let error_code = |response: String| {
            serde_json::from_str::<crate::api::schema::ErrorResponse>(&response)
                .unwrap()
                .error
        };
        // An integration older than v14 never registered action support.
        let refused = start_action(
            &mut app,
            action_params(&instruct, AgentActionOp::Answer, answer.clone()),
        )
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
        assert_eq!(error_code(refused).code, "agent_action_unsupported");
        assert!(rx.try_recv().is_err());

        app.state
            .terminals
            .get_mut(&fixture_terminal_id(&app))
            .unwrap()
            .action_listener = true;
        let response = start_action(
            &mut app,
            action_params(&instruct, AgentActionOp::Answer, answer.clone()),
        );
        let (action_id, body) = sent_action(&mut rx);
        // The block names the session herdr checked, so OMP refuses it in another session.
        assert_eq!(
            body,
            serde_json::json!({
                "op": "answer",
                "args": answer,
                "session": session_tag(&fixture_session_path()),
            })
        );
        // A second write waits until the first block has its result.
        let busy = start_action(
            &mut app,
            action_params(&instruct, AgentActionOp::Abort, serde_json::json!({})),
        )
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
        assert_eq!(error_code(busy).code, "agent_not_ready");
        // Only the process the block went to can answer for it.
        let foreign = ack_action(
            &mut app,
            &target,
            &action_id,
            std::process::id() + 1,
            true,
            None,
            vec![],
        );
        assert!(foreign.contains("process_mismatch"), "{foreign}");
        let ok = ack_action(
            &mut app,
            &target,
            &action_id,
            std::process::id(),
            true,
            None,
            vec!["\x1b[B\r".into()],
        );
        assert!(ok.contains("\"ok\""), "{ok}");
        // The answer keys are written, once; the caller waits for the dialog to close.
        assert_eq!(&rx.try_recv().unwrap()[..], b"\x1b[B\r");
        let again = ack_action(
            &mut app,
            &target,
            &action_id,
            std::process::id(),
            true,
            None,
            vec!["\r".into()],
        );
        // A resent keyed ack is answered ok, and the keys are not written twice.
        assert!(again.contains("\"ok\""), "{again}");
        assert!(rx.try_recv().is_err());
        assert!(response.recv_timeout(Duration::from_millis(50)).is_err());
        ack_action(
            &mut app,
            &target,
            &action_id,
            std::process::id(),
            true,
            None,
            vec![],
        );
        let done: SuccessResponse =
            serde_json::from_str(&response.recv_timeout(Duration::from_secs(1)).unwrap()).unwrap();
        let ResponseResult::AgentActionDone {
            action_id: done_id,
            op,
            data,
            ..
        } = done.result
        else {
            panic!("{:?}", done.result);
        };
        assert_eq!(
            (done_id.as_str(), op),
            (action_id.as_str(), AgentActionOp::Answer)
        );
        assert_eq!(data["dialog_id"], "c1");
        let outcome = |app: &App| {
            app.agent_info_for_target(&target)
                .unwrap()
                .last_action
                .map(|last| (last.action_id, last.outcome))
        };
        assert_eq!(
            outcome(&app),
            Some((action_id.clone(), crate::api::schema::ActionOutcome::Done))
        );
        // A late ack for a settled action is not found.
        let late = ack_action(
            &mut app,
            &target,
            &action_id,
            std::process::id(),
            true,
            None,
            vec![],
        );
        assert!(late.contains("action_not_found"), "{late}");

        let response = start_action(
            &mut app,
            action_params(&instruct, AgentActionOp::Answer, answer.clone()),
        );
        let (action_id, _) = sent_action(&mut rx);
        ack_action(
            &mut app,
            &target,
            &action_id,
            std::process::id(),
            false,
            Some("dialog_changed: another dialog"),
            vec![],
        );
        let refused = error_code(response.recv_timeout(Duration::from_secs(1)).unwrap());
        assert_eq!(refused.code, "agent_action_refused");
        assert!(refused.message.starts_with("dialog_changed:"));
        assert_eq!(
            outcome(&app),
            Some((action_id, crate::api::schema::ActionOutcome::Refused))
        );

        // No result in time: unconfirmed, and the listener counts as gone until it reports again.
        let response = start_action(
            &mut app,
            action_params(&instruct, AgentActionOp::Abort, serde_json::json!({})),
        );
        let (action_id, _) = sent_action(&mut rx);
        assert_eq!(
            outcome(&app),
            Some((
                action_id.clone(),
                crate::api::schema::ActionOutcome::Written
            ))
        );
        let unconfirmed = error_code(response.recv_timeout(Duration::from_secs(2)).unwrap());
        assert_eq!(unconfirmed.code, "action_unconfirmed");
        app.expire_instruction_acks();
        let info = app.agent_info_for_target(&target).unwrap();
        assert!(!info.accepts_instructions && !info.accepts_actions);
        // A caller can tell this withdrawal from an integration without action support.
        assert_eq!(
            outcome(&app),
            Some((action_id, crate::api::schema::ActionOutcome::Unconfirmed))
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn omp_detail_keeps_up_to_32_kib_with_tools_and_subagents() {
        let (mut app, instruct, _rx) = guarded_fixture(AgentState::Idle);
        let target = instruct.target.clone();
        let detail = |tool_bytes: usize| -> crate::api::schema::OmpDetail {
            serde_json::from_value(serde_json::json!({
                "updated_ms": 1,
                "service_tiers": {"openai": "flex"},
                "active_tools": ["t".repeat(tool_bytes)],
                "inactive_tools": ["bash"],
                "status_text": "sahur: reviewing",
                "session_name": "Fix the parser",
                "leaf_id": "a1b2c3d4",
                "subagents": {
                    "total": 2,
                    "running": 1,
                    "items": [{
                        "id": "0-Explore",
                        "name": "Explore",
                        "type": "explore",
                        "status": "running",
                        "run": 2,
                        "revived": true,
                        "person": true,
                        "tool": {"name": "grep", "started_ms": 5},
                        "started_ms": 3,
                        "active_ms": 4
                    }],
                    "truncated": true
                }
            }))
            .unwrap()
        };
        let report = |app: &mut App, omp: crate::api::schema::OmpDetail| {
            app.handle_pane_report_omp_detail(
                "detail".into(),
                crate::api::schema::PaneReportOmpDetailParams {
                    pane_id: target.clone(),
                    agent_pid: std::process::id(),
                    runtime_instance: FIXTURE_RUNTIME.into(),
                    omp,
                    peer_pid: Some(std::process::id()),
                },
            )
        };
        let kept = detail(20 * 1024);
        assert!(report(&mut app, kept.clone()).contains("\"ok\""));
        let info = app.agent_info_for_target(&target).unwrap();
        assert_eq!(info.omp, Some(kept.clone()));
        let json = serde_json::to_value(&info.omp).unwrap();
        assert_eq!(json["subagents"]["items"][0]["type"], "explore");
        assert_eq!(json["subagents"]["truncated"], true);
        assert_eq!(json["subagents"]["items"][0]["run"], 2);
        assert_eq!(json["subagents"]["items"][0]["person"], true);
        assert_eq!(json["session_name"], "Fix the parser");
        assert_eq!(json["leaf_id"], "a1b2c3d4");
        assert!(report(&mut app, detail(33 * 1024)).contains("too large"));
        assert_eq!(app.agent_info_for_target(&target).unwrap().omp, Some(kept));
    }

    #[test]
    fn agent_action_takes_the_v2_ops() {
        use crate::api::schema::AgentActionOp as Op;
        for (op, expected) in [
            ("set_service_tier", Op::SetServiceTier),
            ("set_tools", Op::SetTools),
            ("notify", Op::Notify),
            ("status", Op::Status),
            ("subagent_steer", Op::SubagentSteer),
            ("subagent_cancel", Op::SubagentCancel),
            ("tree", Op::Tree),
            ("fork", Op::Fork),
            ("new_session", Op::NewSession),
            ("switch_session", Op::SwitchSession),
            ("label", Op::Label),
            ("rename", Op::Rename),
        ] {
            let parsed: Op = serde_json::from_value(serde_json::json!(op)).unwrap();
            assert_eq!(parsed, expected);
            assert_eq!(serde_json::to_value(parsed).unwrap(), serde_json::json!(op));
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn omp_detail_comes_only_from_the_registered_listener_and_ends_with_it() {
        let (mut app, instruct, _rx) = guarded_fixture(AgentState::Idle);
        let target = instruct.target.clone();
        let detail = crate::api::schema::OmpDetail {
            model: Some("openai/gpt-5.1".into()),
            thinking: Some("high".into()),
            tool: None,
            context: Some(crate::api::schema::OmpContext {
                tokens: 1200,
                window: 200_000,
                percent: 1,
            }),
            todos: None,
            dialog: None,
            service_tiers: None,
            active_tools: None,
            inactive_tools: None,
            status_text: None,
            subagents: None,
            session_name: None,
            leaf_id: None,
            updated_ms: 1,
        };
        let report = |app: &mut App, runtime: &str, peer_pid: u32| {
            app.handle_pane_report_omp_detail(
                "detail".into(),
                crate::api::schema::PaneReportOmpDetailParams {
                    pane_id: target.clone(),
                    agent_pid: std::process::id(),
                    runtime_instance: runtime.into(),
                    omp: detail.clone(),
                    peer_pid: Some(peer_pid),
                },
            )
        };
        assert!(
            report(&mut app, "another-runtime", std::process::id()).contains("process_mismatch")
        );
        assert!(
            report(&mut app, FIXTURE_RUNTIME, std::process::id() + 1).contains("process_mismatch")
        );
        assert_eq!(app.agent_info_for_target(&target).unwrap().omp, None);
        assert!(report(&mut app, FIXTURE_RUNTIME, std::process::id()).contains("\"ok\""));
        assert_eq!(
            app.agent_info_for_target(&target).unwrap().omp,
            Some(detail.clone())
        );
        // An exec restart registers a new runtime: the old detail is gone.
        report_session_from(&mut app, &target, 50, true, Some("next-runtime"));
        let info = app.agent_info_for_target(&target).unwrap();
        assert!(info.accepts_actions);
        assert_eq!(info.omp, None);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_session_change_waits_longer_for_its_result_than_other_actions() {
        use crate::api::schema::AgentActionOp;
        let (mut app, instruct, mut rx) = guarded_fixture(AgentState::Idle);
        let target = instruct.target.clone();
        app.state
            .terminals
            .get_mut(&fixture_terminal_id(&app))
            .unwrap()
            .action_listener = true;
        let response = start_action(
            &mut app,
            action_params(&instruct, AgentActionOp::NewSession, serde_json::json!({})),
        );
        let (action_id, body) = sent_action(&mut rx);
        assert_eq!(body["op"], "new_session");
        // Past the deadline of every other op, the session change still waits for its result.
        std::thread::sleep(INSTRUCTION_ACK_TIMEOUT + Duration::from_millis(150));
        app.expire_instruction_acks();
        let ok = ack_action(
            &mut app,
            &target,
            &action_id,
            std::process::id(),
            true,
            None,
            vec![],
        );
        assert!(ok.contains("\"ok\""), "{ok}");
        let done = response.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(done.contains("\"new_session\""), "{done}");
        assert!(!done.contains("action_unconfirmed"), "{done}");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn switch_session_is_refused_for_a_file_another_pane_runs() {
        use crate::api::schema::AgentActionOp;
        let (mut app, instruct, mut rx) = guarded_fixture(AgentState::Idle);
        app.state
            .terminals
            .get_mut(&fixture_terminal_id(&app))
            .unwrap()
            .action_listener = true;
        let other_path = std::env::current_dir()
            .unwrap()
            .join("other-omp.jsonl")
            .display()
            .to_string();
        app.state.workspaces.push(Workspace::test_new("other"));
        app.state.ensure_test_terminals();
        let other_pane = app.state.workspaces[1].tabs[0].root_pane;
        let other_terminal = app.state.workspaces[1].tabs[0].panes[&other_pane]
            .attached_terminal_id
            .clone();
        let owner = crate::platform::observe_process(std::process::id())
            .unwrap()
            .unwrap();
        let terminal = app.state.terminals.get_mut(&other_terminal).unwrap();
        terminal.set_detected_state(Some(Agent::Omp), AgentState::Idle);
        assert!(terminal
            .set_agent_session_ref_for_session_start_with_recovery(
                "herdr:omp".into(),
                "omp".into(),
                crate::agent_resume::AgentSessionRef::path(other_path.clone()),
                Some(1),
                Some("startup".into()),
                Some(("default".into(), owner)),
            )
            .is_some());
        let refused = start_action(
            &mut app,
            action_params(
                &instruct,
                AgentActionOp::SwitchSession,
                serde_json::json!({ "session_path": other_path }),
            ),
        )
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
        let refused: crate::api::schema::ErrorResponse = serde_json::from_str(&refused).unwrap();
        assert_eq!(refused.error.code, "agent_action_refused");
        assert!(
            refused.error.message.starts_with("session_in_use:"),
            "{}",
            refused.error.message
        );
        assert!(rx.try_recv().is_err(), "nothing was written");
        // A file no other pane runs is sent on to OMP.
        start_action(
            &mut app,
            action_params(
                &instruct,
                AgentActionOp::SwitchSession,
                serde_json::json!({ "session_path": "/elsewhere/free.jsonl" }),
            ),
        );
        assert_eq!(sent_action(&mut rx).1["op"], "switch_session");
        // While that switch waits for its result, no other pane may switch to the same file. The
        // pending switch is handed to the other pane, so this agent can write again.
        for pending in app.pending_action_acks.values_mut() {
            pending.terminal_id = other_terminal.clone();
        }
        let claimed = start_action(
            &mut app,
            action_params(
                &instruct,
                AgentActionOp::SwitchSession,
                serde_json::json!({ "session_path": "/elsewhere/free.jsonl" }),
            ),
        )
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
        let claimed: crate::api::schema::ErrorResponse = serde_json::from_str(&claimed).unwrap();
        assert!(
            claimed.error.message.starts_with("session_in_use:"),
            "{}",
            claimed.error.message
        );
        assert!(rx.try_recv().is_err(), "nothing was written");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn agent_start_refuses_to_resume_a_session_a_live_omp_pane_runs() {
        let (mut app, _instruct, _rx) = guarded_fixture(AgentState::Idle);
        app.state
            .workspaces
            .push(Workspace::test_new("start-target"));
        app.state.ensure_test_terminals();
        let target_pane = app.state.workspaces[1].tabs[0].root_pane;
        let pane_id = app.public_pane_id(1, target_pane).unwrap();
        let start = |app: &mut App, args: &[&str]| -> serde_json::Value {
            let response = app.handle_api_request(crate::api::schema::Request {
                id: "start".into(),
                method: crate::api::schema::Method::AgentStart(
                    crate::api::schema::AgentStartParams {
                        name: "second".into(),
                        kind: "omp".into(),
                        pane_id: pane_id.clone(),
                        args: args.iter().map(|arg| arg.to_string()).collect(),
                        timeout_ms: Some(4_000),
                        profile: None,
                    },
                ),
            });
            serde_json::from_str(&response).unwrap()
        };
        let path = fixture_session_path();
        let target_terminal = app.state.workspaces[1]
            .terminal_id(target_pane)
            .cloned()
            .unwrap();
        // A prefix names a file of this pane's own project first: from another directory it is not
        // the live pane's file, while a path or a full session id is wherever it is.
        let refused = start(&mut app, &["--resume", path.as_str()]);
        assert_eq!(
            refused["error"]["code"], "agent_session_in_use",
            "{refused}"
        );
        for args in [vec!["--resume=OMP-NAT"], vec!["-r", "omp-nat"]] {
            let response = start(&mut app, &args);
            assert_eq!(
                response["error"]["code"], "agent_pane_unavailable",
                "{args:?}: {response}"
            );
        }
        app.state.terminals.get_mut(&target_terminal).unwrap().cwd = "/fixture/project".into();
        // The live pane runs `omp-native.jsonl`: every form OMP resolves to that file is refused.
        for args in [
            vec!["--resume", path.as_str()],
            vec!["--resume=OMP-NAT"],
            vec!["-r", "omp-nat"],
            vec!["--session", "omp-native"],
        ] {
            let response = start(&mut app, &args);
            assert_eq!(
                response["error"]["code"], "agent_session_in_use",
                "{args:?}: {response}"
            );
        }
        // Another session, and a flag value OMP would not take, pass this check (the fixture has
        // no terminal runtime, so the start stops at the next one).
        for args in [vec!["--resume", "zz9"], vec!["-r", "-omp"]] {
            let response = start(&mut app, &args);
            assert_eq!(
                response["error"]["code"], "agent_pane_unavailable",
                "{args:?}: {response}"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn agent_start_refuses_to_resume_a_session_another_pane_is_switching_to() {
        use crate::api::schema::AgentActionOp;
        let (mut app, instruct, mut rx) = guarded_fixture(AgentState::Idle);
        let terminal_id = fixture_terminal_id(&app);
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .action_listener = true;
        let claimed = "/elsewhere/2026-02-02T00-00-00-000Z_claimed.jsonl";
        let _response = start_action(
            &mut app,
            action_params(
                &instruct,
                AgentActionOp::SwitchSession,
                serde_json::json!({ "session_path": claimed }),
            ),
        );
        sent_action(&mut rx);
        app.state
            .workspaces
            .push(Workspace::test_new("start-target"));
        app.state.ensure_test_terminals();
        let target_pane = app.state.workspaces[1].tabs[0].root_pane;
        let target_terminal = app.state.workspaces[1]
            .terminal_id(target_pane)
            .cloned()
            .unwrap();
        app.state.terminals.get_mut(&target_terminal).unwrap().cwd = "/fixture/project".into();
        let pane_id = app.public_pane_id(1, target_pane).unwrap();
        let start = |app: &mut App, arg: &str| -> serde_json::Value {
            let response = app.handle_api_request(crate::api::schema::Request {
                id: "start".into(),
                method: crate::api::schema::Method::AgentStart(
                    crate::api::schema::AgentStartParams {
                        name: "second".into(),
                        kind: "omp".into(),
                        pane_id: pane_id.clone(),
                        args: vec!["--resume".into(), arg.into()],
                        timeout_ms: Some(4_000),
                        profile: None,
                    },
                ),
            });
            serde_json::from_str(&response).unwrap()
        };
        // The file the switch is about to open is as taken as one a pane runs now.
        for arg in [claimed, "2026-02-02T00-00", "claimed"] {
            let response = start(&mut app, arg);
            assert_eq!(
                response["error"]["code"], "agent_session_in_use",
                "{arg}: {response}"
            );
        }
        let free = start(&mut app, "/elsewhere/free.jsonl");
        assert_eq!(free["error"]["code"], "agent_pane_unavailable", "{free}");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn agent_start_refuses_to_continue_a_session_a_live_omp_pane_runs() {
        let (mut app, _instruct, _rx) = guarded_fixture(AgentState::Idle);
        let dir = std::env::temp_dir().join(format!(
            "herdr-continue-guard-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let live = dir.join("2026-01-01T00-00-00-000Z_0a1b2c3d-4e5f-6789-abcd-ef0123456789.jsonl");
        std::fs::write(&live, "{}\n").unwrap();
        // The live pane runs the newest file of its directory.
        let owner = crate::platform::observe_process(std::process::id())
            .unwrap()
            .unwrap();
        let terminal_id = fixture_terminal_id(&app);
        assert!(app
            .state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .set_agent_session_ref_for_session_start_with_recovery(
                "herdr:omp".into(),
                "omp".into(),
                Some(
                    crate::agent_resume::AgentSessionRef::path(live.display().to_string()).unwrap()
                ),
                Some(2),
                Some("startup".into()),
                Some(("default".into(), owner)),
            )
            .is_some());
        app.state
            .workspaces
            .push(Workspace::test_new("start-target"));
        app.state.ensure_test_terminals();
        let target_pane = app.state.workspaces[1].tabs[0].root_pane;
        let target_terminal = app.state.workspaces[1]
            .terminal_id(target_pane)
            .cloned()
            .unwrap();
        let pane_id = app.public_pane_id(1, target_pane).unwrap();
        let start = |app: &mut App, args: &[&str]| -> serde_json::Value {
            let response = app.handle_api_request(crate::api::schema::Request {
                id: "start".into(),
                method: crate::api::schema::Method::AgentStart(
                    crate::api::schema::AgentStartParams {
                        name: "second".into(),
                        kind: "omp".into(),
                        pane_id: pane_id.clone(),
                        args: args.iter().map(|arg| arg.to_string()).collect(),
                        timeout_ms: Some(4_000),
                        profile: None,
                    },
                ),
            });
            serde_json::from_str(&response).unwrap()
        };
        // Another directory: the live pane's sessions are not this pane's to continue (the
        // fixture has no terminal runtime, so the start stops at the next check).
        let elsewhere = start(&mut app, &["--continue"]);
        assert_eq!(
            elsewhere["error"]["code"], "agent_pane_unavailable",
            "{elsewhere}"
        );
        app.state.terminals.get_mut(&target_terminal).unwrap().cwd = "/fixture/project".into();
        for args in [
            vec!["--continue"],
            vec!["-c"],
            vec!["--model", "m", "--continue"],
        ] {
            let response = start(&mut app, &args);
            assert_eq!(
                response["error"]["code"], "agent_session_in_use",
                "{args:?}: {response}"
            );
            assert!(
                response["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("`--continue`"),
                "{response}"
            );
        }
        // `--continue <session id>` is a resume of that id: refused for the live pane's id, free
        // for another. `--session-dir` moves the lookup away from the live pane's directory.
        let live_id = "0a1b2c3d-4e5f-6789-abcd-ef0123456789";
        let refused = start(&mut app, &["--continue", live_id]);
        assert_eq!(
            refused["error"]["code"], "agent_session_in_use",
            "{refused}"
        );
        assert!(
            refused["error"]["message"]
                .as_str()
                .unwrap()
                .contains(&format!("--resume {live_id}")),
            "{refused}"
        );
        for args in [
            vec!["--continue", "ffffffff-4e5f-6789-abcd-ef0123456789"],
            vec!["--session-dir", "/elsewhere", "--continue"],
            vec!["--continue", "--session-dir=/elsewhere"],
        ] {
            let response = start(&mut app, &args);
            assert_eq!(
                response["error"]["code"], "agent_pane_unavailable",
                "{args:?}: {response}"
            );
        }
        // A newer file that no live pane runs is what `--continue` opens.
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(dir.join("2026-01-02T00-00-00-000Z_dead.jsonl"), "{}\n").unwrap();
        let free = start(&mut app, &["--continue"]);
        assert_eq!(free["error"]["code"], "agent_pane_unavailable", "{free}");
        // Without the flag nothing changes.
        let plain = start(&mut app, &["--model", "m"]);
        assert_eq!(plain["error"]["code"], "agent_pane_unavailable", "{plain}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn an_action_ends_at_once_when_the_omp_that_ran_it_exits() {
        use crate::api::schema::AgentActionOp;
        let (mut app, instruct, mut rx) = guarded_fixture(AgentState::Idle);
        let terminal_id = fixture_terminal_id(&app);
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .action_listener = true;
        let response = start_action(
            &mut app,
            action_params(&instruct, AgentActionOp::NewSession, serde_json::json!({})),
        );
        sent_action(&mut rx);
        // The server looks at pending actions on its own, once a second, not only when a request
        // comes in; with none pending it has nothing to look at.
        assert!(app.next_pending_action_owner_check().is_some());
        let later = Instant::now() + Duration::from_secs(2);
        // The OMP is alive: the action keeps waiting.
        app.check_pending_action_owners(later);
        assert!(response.recv_timeout(Duration::from_millis(50)).is_err());
        assert_eq!(app.pending_action_acks.len(), 1);
        // The OMP that owns it is killed while its session change runs; the pane stays open.
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let owner = crate::platform::observe_process(child.id())
            .unwrap()
            .unwrap();
        for pending in app.pending_action_acks.values_mut() {
            pending.owner = owner.clone();
        }
        app.check_pending_action_owners(later + Duration::from_secs(2));
        assert_eq!(app.pending_action_acks.len(), 1, "the owner still runs");
        child.kill().unwrap();
        child.wait().unwrap();
        app.check_pending_action_owners(later + Duration::from_secs(4));
        // The caller hears at once, not after the session ack deadline of 120 s.
        let ended = response.recv_timeout(Duration::from_millis(100)).unwrap();
        assert!(ended.contains("action_unconfirmed"), "{ended}");
        assert!(app.pending_action_acks.is_empty());
        assert!(app.next_pending_action_owner_check().is_none());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn closing_a_pane_ends_its_pending_switch_and_its_claim() {
        use crate::api::schema::AgentActionOp;
        let (mut app, instruct, mut rx) = guarded_fixture(AgentState::Idle);
        let terminal_id = fixture_terminal_id(&app);
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .action_listener = true;
        let target = "/elsewhere/claimed.jsonl";
        let response = start_action(
            &mut app,
            action_params(
                &instruct,
                AgentActionOp::SwitchSession,
                serde_json::json!({ "session_path": target }),
            ),
        );
        sent_action(&mut rx);
        // The pane closes while the switch waits for its result.
        app.state.workspaces[0].tabs[0].panes.clear();
        app.state.remove_unattached_terminal_ids([terminal_id]);
        app.shutdown_detached_terminal_runtimes();
        // The caller hears at once, long before the session ack deadline.
        let ended = response.recv_timeout(Duration::from_millis(100)).unwrap();
        assert!(ended.contains("action_unconfirmed"), "{ended}");
        assert!(app
            .pending_action_acks
            .values()
            .all(|pending| pending.switch_target.as_deref() != Some(target)));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn an_omp_branch_report_moves_the_session_to_the_branch_file() {
        let (mut app, instruct, _rx) = guarded_fixture(AgentState::Idle);
        let target = instruct.target.clone();
        let branch_path = std::env::current_dir()
            .unwrap()
            .join("omp-branch.jsonl")
            .display()
            .to_string();
        // The integration's state reports name the session, which makes it the hook's session.
        app.state
            .terminals
            .get_mut(&fixture_terminal_id(&app))
            .unwrap()
            .set_hook_authority_with_session_ref(
                "herdr:omp".into(),
                "omp".into(),
                AgentState::Idle,
                None,
                crate::agent_resume::AgentSessionRef::path(fixture_session_path()),
                Some(60),
            );
        let pid = std::process::id();
        let reply = app.handle_pane_report_agent_session_v2(
            "v2".into(),
            crate::api::schema::PaneReportAgentSessionV2Params {
                pane_id: target.clone(),
                source: "herdr:omp".into(),
                agent: "omp".into(),
                seq: Some(70),
                agent_session_id: None,
                agent_session_path: Some(branch_path.clone()),
                session_start_source: Some("branch".into()),
                launch_profile: "default".into(),
                agent_pid: pid,
                accepts_instructions: true,
                accepts_actions: true,
                runtime_instance: Some(FIXTURE_RUNTIME.into()),
                block_token: None,
                peer_pid: Some(pid),
            },
        );
        assert!(reply.contains("\"ok\""), "{reply}");
        let info = app.agent_info_for_target(&target).unwrap();
        assert_eq!(info.agent_session.unwrap().value, branch_path);
        assert!(info.accepts_actions);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn an_unconfirmed_action_keeps_a_listener_that_reported_since() {
        use crate::api::schema::AgentActionOp;
        let (mut app, instruct, mut rx) = guarded_fixture(AgentState::Idle);
        let target = instruct.target.clone();
        app.state
            .terminals
            .get_mut(&fixture_terminal_id(&app))
            .unwrap()
            .action_listener = true;
        let response = start_action(
            &mut app,
            action_params(&instruct, AgentActionOp::Abort, serde_json::json!({})),
        );
        sent_action(&mut rx);
        // The integration reports again (a session change) before the result is due.
        report_session(&mut app, &target, 60, true);
        response.recv_timeout(Duration::from_secs(2)).unwrap();
        app.expire_instruction_acks();
        assert!(app.agent_info_for_target(&target).unwrap().accepts_actions);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn answer_keys_are_not_written_once_another_program_reads_the_terminal() {
        use crate::api::schema::AgentActionOp;
        let pane_id = app_with_agent().state.workspaces[0].tabs[0].root_pane;
        let (events, _events_rx) = tokio::sync::mpsc::channel(32);
        let runtime = crate::terminal::TerminalRuntime::spawn(
            pane_id,
            24,
            80,
            std::env::temp_dir(),
            0,
            Default::default(),
            None,
            crate::pane::PaneShellConfig::new("/bin/sh", crate::config::ShellModeConfig::NonLogin),
            &crate::pane::PaneLaunchEnv::default(),
            events,
            std::sync::Arc::new(tokio::sync::Notify::new()),
            std::sync::Arc::new(crate::render_signal::RenderSignal::new()),
        )
        .unwrap();
        let owner_pid = runtime.child_pid().unwrap();
        // The "agent" reads one line (the block header) itself, then starts `cat`, which reads
        // the terminal in its place, as OMP's external editor does.
        runtime
            .try_send_bytes(Bytes::from_static(
                b"printf '\\033[?2004h'; exec sh -c 'read line; cat; exec sleep 30'\n",
            ))
            .unwrap();
        let job_size =
            || crate::detect::foreground_job(owner_pid).map_or(0, |job| job.processes.len());
        let execed = || {
            std::fs::read_to_string(format!("/proc/{owner_pid}/cmdline"))
                .is_ok_and(|cmd| cmd.contains("read line"))
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !(execed() && job_size() == 1 && runtime.bracketed_paste_enabled())
            && Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let owner = crate::platform::observe_process(owner_pid)
            .unwrap()
            .unwrap();
        let (mut app, instruct) = guarded_app(AgentState::Blocked, owner, runtime);
        let target = instruct.target.clone();
        app.state
            .terminals
            .get_mut(&fixture_terminal_id(&app))
            .unwrap()
            .action_listener = true;
        let response = start_action(
            &mut app,
            action_params(
                &instruct,
                AgentActionOp::Answer,
                serde_json::json!({ "dialog_id": "c1", "approve": true }),
            ),
        );
        let action_id = app.pending_action_acks.keys().next().unwrap().clone();
        let deadline = Instant::now() + Duration::from_secs(5);
        while job_size() != 2 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(job_size(), 2, "the agent and its tty-reading child");
        let ack = app.handle_pane_ack_action(
            "ack".into(),
            crate::api::schema::PaneAckActionParams {
                pane_id: target,
                action_id,
                agent_pid: owner_pid,
                ok: true,
                error: None,
                data: Default::default(),
                keys: vec!["\x1b[A\r".into()],
                peer_pid: Some(owner_pid),
            },
        );
        assert!(ack.contains("keys_not_written"), "{ack}");
        let refused: crate::api::schema::ErrorResponse =
            serde_json::from_str(&response.recv_timeout(Duration::from_secs(1)).unwrap()).unwrap();
        assert_eq!(refused.error.code, "agent_action_refused");
        assert!(
            refused.error.message.contains("another program reads"),
            "{}",
            refused.error.message
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn an_instruction_to_an_action_listener_names_its_session() {
        let (mut app, params, mut rx) = guarded_fixture(AgentState::Idle);
        app.state
            .terminals
            .get_mut(&fixture_terminal_id(&app))
            .unwrap()
            .action_listener = true;
        let _response = start_instruct(&mut app, params);
        let bytes = rx.try_recv().expect("the instruction was written");
        let written = std::str::from_utf8(&bytes).unwrap();
        let header = written
            .strip_prefix("\x1b[200~herdr-instruction:v4:")
            .and_then(|rest| rest.split_once('\n'))
            .expect("a v4 block")
            .0;
        let fields: Vec<_> = header.split(':').collect();
        assert_eq!(fields[3], FIXTURE_RUNTIME);
        assert_eq!(fields[4], session_tag(&fixture_session_path()));
        // The body ends with the end line, so the integration can cut a pasted block out exactly.
        assert!(
            written.ends_with(&format!("\nherdr-end:{}\x1b[201~", fields[0])),
            "{written:?}"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn blocks_carry_the_token_the_listener_registered_last() {
        let (mut app, params, mut rx) = guarded_fixture(AgentState::Idle);
        let target = params.target.clone();
        let pid = std::process::id();
        for (seq, token) in [(70, "token-one"), (71, "token-two")] {
            let reply = app.handle_pane_report_agent_session_v2(
                "v2".into(),
                crate::api::schema::PaneReportAgentSessionV2Params {
                    pane_id: target.clone(),
                    source: "herdr:omp".into(),
                    agent: "omp".into(),
                    seq: Some(seq),
                    agent_session_id: None,
                    agent_session_path: Some(fixture_session_path()),
                    session_start_source: Some("startup".into()),
                    launch_profile: "default".into(),
                    agent_pid: pid,
                    accepts_instructions: true,
                    accepts_actions: true,
                    runtime_instance: Some(FIXTURE_RUNTIME.into()),
                    block_token: Some(token.into()),
                    peer_pid: Some(pid),
                },
            );
            assert!(reply.contains("\"ok\""), "{reply}");
        }
        let _response = start_instruct(&mut app, params);
        let bytes = rx.try_recv().expect("the instruction was written");
        let header = std::str::from_utf8(&bytes)
            .unwrap()
            .strip_prefix("\x1b[200~herdr-instruction:v4:")
            .and_then(|rest| rest.split_once('\n'))
            .expect("a v4 block")
            .0
            .to_string();
        // The latest token, not the runtime id or an earlier token.
        assert_eq!(header.split(':').nth(3), Some("token-two"));
    }
}
