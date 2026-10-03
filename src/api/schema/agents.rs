use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::common::{AgentStatus, ReadFormat, ReadSource};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentReadParams {
    pub target: String,
    pub source: ReadSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<u32>,
    #[serde(default)]
    pub format: ReadFormat,
    #[serde(default = "super::common::default_true")]
    pub strip_ansi: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentSendKeysParams {
    pub target: String,
    pub keys: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentWaitParams {
    pub target: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub until: Vec<AgentStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentPromptWaitOptions {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub until: Vec<AgentStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(skip)]
    #[schemars(skip)]
    pub(crate) submission_deadline: Option<std::time::Instant>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentRenameParams {
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentViewSetParams {
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<AgentViewFilter>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sort: Vec<AgentViewSort>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct AgentViewClearParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum AgentViewFilter {
    All {
        filters: Vec<AgentViewFilter>,
    },
    Any {
        filters: Vec<AgentViewFilter>,
    },
    Not {
        filter: Box<AgentViewFilter>,
    },
    Eq {
        field: AgentViewField,
        value: AgentViewValue,
    },
    In {
        field: AgentViewField,
        values: Vec<AgentViewValue>,
    },
    Exists {
        field: AgentViewField,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum AgentViewField {
    Builtin(AgentViewBuiltinField),
    Token { token: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentViewBuiltinField {
    Status,
    WorkspaceId,
    TabId,
    PaneId,
    Agent,
    Seen,
    StateChangeSeq,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum AgentViewValue {
    String(String),
    Bool(bool),
    Number(u64),
    Context { context: AgentViewContext },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentViewContext {
    CurrentWorkspaceId,
    CurrentTabId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentViewSort {
    pub field: AgentViewSortField,
    #[serde(default)]
    pub order: AgentViewSortOrder,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum AgentViewSortField {
    Builtin(AgentViewBuiltinSortField),
    Token { token: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentViewBuiltinSortField {
    WorkspaceOrder,
    TabOrder,
    PaneOrder,
    Attention,
    Status,
    Agent,
    Seen,
    StateChangeSeq,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum AgentViewSortOrder {
    #[default]
    Asc,
    Desc,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentStartParams {
    pub name: String,
    pub kind: String,
    pub pane_id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Startup timeout in milliseconds. Values must be greater than 3000 and at most 300000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// OMP launch profile. Only valid with kind `omp`; the profile must be a key
    /// of `session.omp_launchers`, whose value is run as the executable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentPromptParams {
    pub target: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<AgentPromptWaitOptions>,
}

/// A bounded instruction bound to the exact agent incarnation read by a caller.
/// A separate method prevents older servers from silently ignoring the guards.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentInstructParams {
    pub target: String,
    pub text: String,
    pub expected_terminal_id: String,
    /// Must equal the agent name; omit only for an agent that has no name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_name: Option<String>,
    pub expected_agent: String,
    pub expected_session: String,
    pub expected_runtime_id: String,
    pub expected_workspace_id: String,
    pub expected_cwd: String,
    /// When the socket server read the request; set by the server, never by the caller.
    #[serde(skip)]
    #[schemars(skip)]
    pub received_at: Option<std::time::Instant>,
}

impl AgentInstructParams {
    pub(crate) fn validate(&self, agent: &AgentInfo) -> Result<(), &'static str> {
        let lead = self.text.trim_start();
        // OMP runs `/` as a command, `!` as user bash, and `$`/`$$` before whitespace as
        // Python. Refuse them so a fallback typed path can never execute the text.
        let python = lead
            .strip_prefix("$$")
            .or_else(|| lead.strip_prefix('$'))
            .is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', '\t', '\n']));
        if lead.is_empty()
            || self.text.len() > 8192
            || self
                .text
                .chars()
                .any(|c| c.is_control() && c != '\n' && c != '\t')
            || lead.starts_with(['/', '!'])
            || python
        {
            return Err("invalid_instruction");
        }
        ExpectedAgent {
            terminal_id: &self.expected_terminal_id,
            name: self.expected_name.as_deref(),
            agent: &self.expected_agent,
            session: &self.expected_session,
            runtime_id: &self.expected_runtime_id,
            workspace_id: &self.expected_workspace_id,
            cwd: &self.expected_cwd,
        }
        .check(agent)
    }
}

/// The agent incarnation a caller read before a guarded write.
struct ExpectedAgent<'a> {
    terminal_id: &'a str,
    name: Option<&'a str>,
    agent: &'a str,
    session: &'a str,
    runtime_id: &'a str,
    workspace_id: &'a str,
    cwd: &'a str,
}

impl ExpectedAgent<'_> {
    fn check(&self, agent: &AgentInfo) -> Result<(), &'static str> {
        if self.terminal_id.is_empty()
            || self.agent.is_empty()
            || self.session.is_empty()
            || self.runtime_id.is_empty()
            || self.workspace_id.is_empty()
            || self.cwd.is_empty()
            || agent.terminal_id != self.terminal_id
            || agent.name.as_deref() != self.name
            || agent.agent.as_deref() != Some(self.agent)
            || agent.workspace_id != self.workspace_id
            || agent
                .foreground_cwd
                .as_deref()
                .filter(|cwd| !cwd.is_empty())
                .or(agent.cwd.as_deref())
                != Some(self.cwd)
            || agent.runtime_id.as_deref() != Some(self.runtime_id)
            || agent.agent_session.as_ref().map(|s| s.value.as_str()) != Some(self.session)
        {
            return Err("agent_identity_changed");
        }
        Ok(())
    }
}

/// What `agent.action` asks the OMP integration to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentActionOp {
    /// Abort the running turn.
    Abort,
    /// `args.spec`: a model OMP resolves (`provider/id`, bare id, role alias).
    SetModel,
    /// `args.level`: an OMP thinking level.
    SetThinking,
    /// `args.instructions` (optional): compact the session context.
    Compact,
    /// `args.dialog_id` and the answer: answer the open approval or `ask` dialog.
    Answer,
    /// `args.name` and its `args.args`: run an allow-listed session command while idle.
    Command,
}

/// What became of an `agent.action`, as `AgentInfo.last_action` shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ActionOutcome {
    /// Written to the terminal; no result yet.
    Written,
    /// The integration reported the op done.
    Done,
    /// The integration refused it, or Herdr could not write its keys.
    Refused,
    /// No result came in time; the action may have run, and the listener counts as gone until
    /// the integration reports again.
    Unconfirmed,
}

/// The latest `agent.action` to an agent and its outcome so far.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LastActionInfo {
    pub action_id: String,
    pub op: AgentActionOp,
    pub outcome: ActionOutcome,
}

/// A structured action for the exact OMP agent incarnation read by a caller, with the same
/// identity guards as `agent.instruct`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentActionParams {
    pub target: String,
    pub op: AgentActionOp,
    /// The op's arguments: a JSON object of at most 4096 bytes.
    #[serde(default)]
    pub args: serde_json::Map<String, serde_json::Value>,
    pub expected_terminal_id: String,
    /// Must equal the agent name; omit only for an agent that has no name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_name: Option<String>,
    pub expected_agent: String,
    pub expected_session: String,
    pub expected_runtime_id: String,
    pub expected_workspace_id: String,
    pub expected_cwd: String,
    /// When the socket server read the request; set by the server, never by the caller.
    #[serde(skip)]
    #[schemars(skip)]
    pub received_at: Option<std::time::Instant>,
}

impl AgentActionParams {
    pub(crate) fn validate(&self, agent: &AgentInfo) -> Result<(), &'static str> {
        if serde_json::to_string(&self.args).map_or(true, |args| args.len() > 4096) {
            return Err("invalid_request");
        }
        ExpectedAgent {
            terminal_id: &self.expected_terminal_id,
            name: self.expected_name.as_deref(),
            agent: &self.expected_agent,
            session: &self.expected_session,
            runtime_id: &self.expected_runtime_id,
            workspace_id: &self.expected_workspace_id,
            cwd: &self.expected_cwd,
        }
        .check(agent)
    }
}

/// Live state the OMP integration reports for its root session. A missing field is unknown or
/// none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct OmpDetail {
    /// The session model, `provider/id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The thinking level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    /// The latest tool the root session started and has not finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<OmpTool>,
    /// Context usage of the active model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<OmpContext>,
    /// A summary of the session's todo list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub todos: Option<OmpTodos>,
    /// The approval or `ask` dialog OMP shows now (the oldest open one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dialog: Option<OmpDialog>,
    /// When the integration built the report, unix ms.
    pub updated_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct OmpTool {
    pub name: String,
    pub call_id: String,
    pub started_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct OmpContext {
    pub tokens: u64,
    pub window: u64,
    /// Rounded percent of the window in use.
    pub percent: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct OmpTodos {
    pub total: u32,
    pub done: u32,
    /// The task in progress, else the first pending one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
    pub phases: Vec<OmpTodoPhase>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct OmpTodoPhase {
    pub name: String,
    pub total: u32,
    pub done: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OmpDialogKind {
    Approval,
    Ask,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct OmpDialog {
    /// The tool call that opened the dialog; `agent.action` `answer` names it as `dialog_id`.
    pub id: String,
    pub kind: OmpDialogKind,
    pub tool: String,
    /// For an approval, a summary of the tool's arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// For `ask`, its questions in order, at most 10 with at most 20 options each.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub questions: Vec<OmpQuestion>,
    /// The ask has more questions or options than `questions` shows; `answer` refuses it.
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub truncated: bool,
    /// How many more dialogs OMP shows after this one. Only this one can be answered.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub queued: u32,
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct OmpQuestion {
    pub text: String,
    pub options: Vec<String>,
    pub multi: bool,
    /// A custom text answer is allowed.
    pub other_allowed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentInfo {
    pub terminal_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_title_stripped: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_agent: Option<String>,
    pub agent_status: AgentStatus,
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub screen_detection_skipped: bool,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub state_labels: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    #[schemars(schema_with = "super::common::metadata_token_values_schema")]
    pub tokens: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_session: Option<AgentSessionInfo>,
    /// Opaque identity of a verified registered process, independent of resumable session.
    /// Absent when the harness has no verified process-owner registration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_id: Option<String>,
    /// The live `runtime_id` process registered the OMP integration listener that consumes
    /// `agent.instruct` deliveries.
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub accepts_instructions: bool,
    /// The live `runtime_id` process registered an OMP integration listener that also consumes
    /// `agent.action` (OMP integration v14+).
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub accepts_actions: bool,
    /// Live detail the agent's OMP integration reports: model, running tool, context use, todos,
    /// open dialog.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub omp: Option<OmpDetail>,
    /// The latest `agent.action` to this agent and its outcome so far, kept for two minutes after
    /// its outcome last changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_action: Option<LastActionInfo>,
    /// The agent's OMP integration had an instruction listener and withdrew it when OMP shut
    /// its session down (`/restart`, extension reload); it registers again within seconds.
    /// False for an agent that never registered one. Shown for at most 30 seconds.
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub listener_withdrawn: bool,
    /// The latest `agent.instruct` delivery to this agent and its outcome so far, kept for
    /// two minutes after it was written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_instruction: Option<super::panes::LastInstructionInfo>,
    /// Up to the 8 latest `agent.instruct` deliveries to this agent, newest first, each with its
    /// outcome so far and kept for two minutes after its outcome last changed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_instructions: Vec<super::panes::LastInstructionInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_executable: Option<String>,
    pub workspace_id: String,
    pub tab_id: String,
    pub pane_id: String,
    pub focused: bool,
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub launch_pending: bool,
    #[serde(default, skip_serializing_if = "super::is_false")]
    pub interactive_ready: bool,
    #[serde(default)]
    pub state_change_seq: u64,
    /// The current idle transition completed work, independently of who has viewed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreground_cwd: Option<String>,
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AgentSessionInfo {
    pub source: String,
    pub agent: String,
    pub kind: crate::agent_resume::AgentSessionRefKind,
    pub value: String,
}
