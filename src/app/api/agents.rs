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
/// The longest an instruction may wait in the server queue before it is written. With the ack
/// wait this stays under the 15 s that callers allow for an answer.
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
/// imitates a block is not taken as an instruction.
fn instruction_block(instruction_id: &str, text: &str, runtime: &str) -> String {
    let expires_ms = (std::time::SystemTime::now() + INSTRUCTION_EXPIRY)
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis());
    format!(
        "\x1b[200~herdr-instruction:v3:{instruction_id}:{expires_ms}:{}:{runtime}\n{text}\x1b[201~",
        text.len()
    )
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
        if !super::super::agents::INSTRUCTIONS_SUPPORTED {
            return Err(encode_error(
                id,
                "agent_instruction_unsupported",
                "agent.instruct needs process identity and terminal-reader checks this platform lacks",
            ));
        }
        // A request that waited long in the server queue would leave the caller too little of its
        // own timeout for the ack wait, and it may have given up already: write nothing.
        if params
            .received_at
            .is_some_and(|received| received.elapsed() > INSTRUCTION_QUEUE_LIMIT)
        {
            return Err(encode_error(
                id,
                "agent_not_ready",
                "the instruction waited too long in the server queue; nothing was written",
            ));
        }
        self.expire_instruction_acks();
        self.reconcile_managed_agent_target(&params.target);
        let agent = self
            .agent_info_for_target(&params.target)
            .map_err(|err| encode_error_body(id.clone(), self.agent_target_error_body(err)))?;
        params
            .validate(&agent)
            .map_err(|code| encode_error(id.clone(), code, "instruction refused before sending"))?;
        if !agent.accepts_instructions {
            return Err(encode_error(
                id,
                "agent_instruction_unsupported",
                format!(
                    "agent {} has no instruction listener registered now: OMP is restarting, exiting or reloading its extensions, or its herdr integration is older than v12; nothing was written; retry when agent.get shows accepts_instructions",
                    params.target
                ),
            ));
        }
        let resolved = self
            .resolve_agent_target(&params.target)
            .map_err(|err| encode_error_body(id.clone(), self.agent_target_error_body(err)))?;
        let Some(terminal) = self
            .state
            .workspaces
            .get(resolved.ws_idx)
            .and_then(|workspace| workspace.terminal_id(resolved.pane_id))
            .and_then(|terminal_id| self.state.terminals.get(terminal_id))
        else {
            return Err(agent_not_found(id, &params.target));
        };
        let Some(owner) = terminal.instruction_listener.clone() else {
            return Err(agent_not_found(id, &params.target));
        };
        let listener_runtime = terminal.instruction_listener_runtime.clone();
        // The block is bound to the listener's runtime token; without one it cannot be told apart
        // from a person's paste, so nothing is written.
        let Some(block_runtime) = listener_runtime
            .clone()
            .filter(|runtime| valid_listener_runtime(runtime))
        else {
            return Err(encode_error(
                id,
                "agent_instruction_unsupported",
                format!(
                    "agent {} registered no instruction token; nothing was written; retry when its OMP integration reports again",
                    params.target
                ),
            ));
        };
        let terminal_id = terminal.id.clone();
        let launch_pending = terminal.managed_agent_launch_pending();
        self.settle_stale_deliveries(&terminal_id, &owner, listener_runtime.as_deref());
        // One delivery per agent until its outcome is final: while a block is unconfirmed the
        // listener may be gone (a second block would land in the editor), and while a taken
        // delivery waits for its turn, `last_instruction` must keep following it.
        if self
            .pending_instruction_acks
            .values()
            .any(|pending| pending.terminal_id == terminal_id)
        {
            return Err(encode_error(
                id,
                "agent_not_ready",
                format!(
                    "an earlier instruction to agent {} has no final outcome yet",
                    params.target
                ),
            ));
        }
        let Some(runtime) = self.lookup_runtime_sender(resolved.ws_idx, resolved.pane_id) else {
            return Err(agent_not_found(id, &params.target));
        };
        if !super::super::agents::owner_in_foreground_job(runtime, owner.pid) {
            return Err(encode_error(
                id,
                "agent_identity_changed",
                "the registered agent process is not the pane foreground job",
            ));
        }
        if launch_pending || !runtime.bracketed_paste_enabled() {
            return Err(agent_not_ready(id, &params.target));
        }
        #[cfg(target_os = "linux")]
        if super::super::agents::owner_child_reads_tty(runtime, owner.pid) {
            return Err(encode_error(
                id,
                "agent_not_ready",
                format!(
                    "agent {} has handed the terminal to another program, such as an external editor",
                    params.target
                ),
            ));
        }
        let instruction_id = new_instruction_id(&id, terminal_id.as_str());
        let block = instruction_block(&instruction_id, &params.text, &block_runtime);
        if let Err(err) = runtime.try_send_bytes(Bytes::from(block)) {
            return Err(encode_error(id, "agent_prompt_failed", err.to_string()));
        }
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
        let agent = self
            .agent_info(resolved.ws_idx, resolved.pane_id)
            .unwrap_or(agent);
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
    }

    /// Settles deliveries whose wait has passed. A never-taken delivery may mean the listener is
    /// gone (OMP dropped it on a session change or an exec restart), so the agent stops accepting
    /// instructions until the integration reports its listener again; at most one block can then
    /// reach the editor. Either way the outcome becomes `unconfirmed`.
    pub(super) fn expire_instruction_acks(&mut self) {
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
                runtime_instance: runtime.map(str::to_string),
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
        let header = written
            .strip_prefix("\x1b[200~herdr-instruction:v3:")
            .and_then(|rest| rest.split_once('\n'))
            .expect("marked paste")
            .0;
        let [id, expires_ms, length, runtime] = header.split(':').collect::<Vec<_>>()[..] else {
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
        assert_eq!(
            written,
            format!("\x1b[200~herdr-instruction:v3:{header}\n{text}\x1b[201~")
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
}
