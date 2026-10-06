use std::path::Path;

use serde::{Deserialize, Serialize};

const MAX_SESSION_ID_LEN: usize = 512;
const MAX_SESSION_PATH_LEN: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSessionRef {
    pub kind: AgentSessionRefKind,
    pub value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentSessionRefKind {
    Id,
    Path,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentResumePlan {
    pub agent: String,
    pub argv: Vec<String>,
    pub dedupe_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedAgentSession {
    pub source: String,
    pub agent: String,
    pub session_ref: AgentSessionRef,
    pub launch_profile: Option<String>,
    pub owner_process: Option<crate::platform::OwnerProcessIncarnation>,
}

/// Environment variable that selects the OMP profile. Herdr sets it to the profile name for the
/// commands that run a `[session.omp_launchers]` entry, so a launcher that does not choose a
/// profile itself still runs, and reports, the profile it is registered under.
pub const OMP_PROFILE_ENV: &str = "OMP_PROFILE";

/// Why automatic OMP recovery refuses a saved session. `Display` names the cause and the fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OmpRecoveryBlock {
    Untrusted,
    MissingDefaultLauncher,
    UnknownProfile,
    UnregisteredProfile(String),
    UnknownOwner(String),
    LiveOwner(u32),
    UnverifiableOwner,
}

impl std::fmt::Display for OmpRecoveryBlock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Untrusted => f.write_str("the saved OMP session has no trusted recovery metadata"),
            Self::MissingDefaultLauncher => f.write_str(
                "session.omp_launchers.default is not set; add default = \"<omp executable>\" to [session.omp_launchers]",
            ),
            Self::UnknownProfile => f.write_str(
                "the saved OMP session has no launch profile because an older Herdr or OMP integration saved it; resume it by hand once so OMP integration 11 reports its profile",
            ),
            Self::UnregisteredProfile(profile) => write!(
                f,
                "launch profile {profile:?} is not in [session.omp_launchers]; add {profile:?} = \"<launcher executable>\" there"
            ),
            Self::UnknownOwner(profile) => write!(
                f,
                "no verified OMP report reached Herdr after the last resume, because Herdr stopped first, the launcher for profile {profile:?} started OMP under another profile, or OMP integration 11 is not installed in profile {profile:?} (run `herdr integration install omp`); fix the cause, then resume the session by hand once"
            ),
            Self::LiveOwner(pid) => write!(
                f,
                "OMP process {pid} still owns this session; stop it or use the pane where it runs"
            ),
            Self::UnverifiableOwner => {
                f.write_str("Herdr cannot verify the saved OMP process on this platform")
            }
        }
    }
}

const OMP_RECOVERY_NOTICE_PREFIX: &str = "Automatic OMP recovery is blocked: ";

/// Text shown in a pane whose saved OMP session Herdr did not resume.
pub fn omp_recovery_blocked_notice(reason: impl std::fmt::Display) -> String {
    format!("{OMP_RECOVERY_NOTICE_PREFIX}{reason}. Recover this session manually.")
}

/// Screen history for a pane that restores as a shell because OMP recovery is blocked: the saved
/// history, then `notice` as the last line. Notice lines from earlier restores are dropped, so a
/// pane that stays blocked across restarts shows one notice.
pub fn screen_history_with_omp_recovery_notice(history: Option<&str>, notice: &str) -> String {
    let marker = format!("herdr: {OMP_RECOVERY_NOTICE_PREFIX}");
    let mut screen = history
        .unwrap_or_default()
        .split_inclusive('\n')
        .filter(|line| !line.contains(&marker))
        .collect::<String>();
    if !screen.is_empty() && !screen.ends_with('\n') {
        screen.push_str("\r\n");
    }
    screen.push_str(&format!("\x1b[0;1;33mherdr: {notice}\x1b[0m\r\n"));
    screen
}

pub fn omp_recovery_executable(
    session: &PersistedAgentSession,
    launchers: &std::collections::BTreeMap<String, String>,
) -> Result<String, OmpRecoveryBlock> {
    if session.source != "herdr:omp" || session.agent != "omp" {
        return Err(OmpRecoveryBlock::Untrusted);
    }
    if !launchers.contains_key("default") {
        return Err(OmpRecoveryBlock::MissingDefaultLauncher);
    }
    let profile = session
        .launch_profile
        .as_deref()
        .filter(|profile| !profile.is_empty())
        .ok_or(OmpRecoveryBlock::UnknownProfile)?;
    let executable = launchers
        .get(profile)
        .filter(|executable| !executable.is_empty())
        .ok_or_else(|| OmpRecoveryBlock::UnregisteredProfile(profile.to_string()))?;
    let owner = session
        .owner_process
        .as_ref()
        .filter(|owner| {
            owner.pid != 0 && owner.start_time_ticks != 0 && !owner.boot_id.trim().is_empty()
        })
        .ok_or_else(|| OmpRecoveryBlock::UnknownOwner(profile.to_string()))?;
    match crate::platform::observe_process(owner.pid) {
        Ok(Some(live)) if live == *owner => Err(OmpRecoveryBlock::LiveOwner(owner.pid)),
        Ok(_) => Ok(executable.clone()),
        Err(_) => Err(OmpRecoveryBlock::UnverifiableOwner),
    }
}

impl AgentSessionRef {
    pub fn id(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        valid_session_id(&value).then_some(Self {
            kind: AgentSessionRefKind::Id,
            value,
        })
    }

    pub fn path(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        valid_session_path(&value).then_some(Self {
            kind: AgentSessionRefKind::Path,
            value,
        })
    }
}

pub fn session_ref_from_report(
    source: &str,
    agent: &str,
    agent_session_id: Option<String>,
    _agent_session_path: Option<String>,
) -> Option<AgentSessionRef> {
    if !is_official_agent_source(source, agent) {
        return None;
    }

    if agent == "pi" || agent == "omp" {
        return _agent_session_path
            .and_then(AgentSessionRef::path)
            .or_else(|| agent_session_id.and_then(AgentSessionRef::id));
    }

    agent_session_id.and_then(AgentSessionRef::id)
}

pub fn persisted_session_from_launch_args(
    agent: crate::detect::Agent,
    args: &[String],
) -> Option<PersistedAgentSession> {
    let [command, session_id] = args else {
        return None;
    };
    if agent != crate::detect::Agent::Codex || command != "resume" || session_id.starts_with('-') {
        return None;
    }

    Some(PersistedAgentSession {
        source: "herdr:codex".into(),
        agent: "codex".into(),
        session_ref: AgentSessionRef::id(session_id.clone())?,
        launch_profile: None,
        owner_process: None,
    })
}

/// The session values an OMP command line asks to resume: `--resume`, `-r` and `--session`, as
/// `--flag value` (a value that does not start with `-`) or `--flag=value`.
pub fn omp_resume_args(args: &[String]) -> Vec<&str> {
    let mut values = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) => (flag, Some(value)),
            None => (arg, None),
        };
        if matches!(flag, "--resume" | "-r" | "--session") {
            match inline {
                Some(value) if !value.is_empty() => values.push(value),
                Some(_) => {}
                None => {
                    if let Some(next) = args
                        .get(i + 1)
                        .filter(|next| !next.is_empty() && !next.starts_with('-'))
                    {
                        values.push(next.as_str());
                        i += 1;
                    }
                }
            }
        }
        i += 1;
    }
    values
}

/// What `--continue` / `-c` asks of OMP, read as OMP's own parser and `normalizeContinueSessionArgs`
/// read it.
#[derive(Debug, PartialEq, Eq)]
pub enum OmpContinue<'a> {
    /// No continue, or one that does not open the newest session of the cwd's session directory:
    /// another flag decides the session (`--resume`, `-r`, `--session`, `--fork`, `--no-session`)
    /// or the directory (`--session-dir`), or the flag is only text after `--`.
    Other,
    /// A plain `--continue`: the newest session of the cwd's session directory.
    Newest,
    /// `--continue <session id>`: OMP takes that as `--resume <id>`.
    Session(&'a str),
}

pub fn omp_continue(args: &[String]) -> OmpContinue<'_> {
    let mut continue_at = None;
    for (i, arg) in args.iter().enumerate() {
        if arg == "--" {
            break;
        }
        let flag = arg.split_once('=').map_or(arg.as_str(), |(flag, _)| flag);
        match flag {
            "--continue" | "-c" if continue_at.is_none() => continue_at = Some(i),
            "--resume" | "-r" | "--session" | "--fork" | "--no-session" | "--session-dir" => {
                return OmpContinue::Other;
            }
            _ => {}
        }
    }
    let Some(at) = continue_at else {
        return OmpContinue::Other;
    };
    match args.get(at + 1).map(|next| next.trim()) {
        Some(next) if is_session_uuid(next) => OmpContinue::Session(next),
        _ => OmpContinue::Newest,
    }
}

/// OMP's `SESSION_ID_ARG_RE`: a session id as a UUID.
fn is_session_uuid(value: &str) -> bool {
    let groups: Vec<&str> = value.split('-').collect();
    groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(group, len)| group.len() == len && group.chars().all(|c| c.is_ascii_hexdigit()))
}

/// The session file `omp --continue` opens among the sessions beside `session_path`: the
/// `.jsonl` file in that directory with the newest modification time (the name breaks a tie).
/// OMP also prefers the terminal's own last session when it has one, which Herdr cannot see.
pub fn newest_session_beside(session_path: &str) -> Option<std::path::PathBuf> {
    let dir = Path::new(session_path).parent()?;
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "jsonl"))
        .filter_map(|entry| {
            let meta = entry.metadata().ok().filter(|meta| meta.is_file())?;
            Some((meta.modified().ok()?, entry.path()))
        })
        .max()
        .map(|(_, path)| path)
}

/// Whether a `--resume` value is a prefix of a session's file name or id, which OMP looks up in
/// its own project first. A path (it has a separator or ends in `.jsonl`) and a full session id
/// name one file wherever it is.
pub fn omp_resume_value_is_prefix(value: &str) -> bool {
    !(value.contains('/')
        || value.contains('\\')
        || value.ends_with(".jsonl")
        || is_session_uuid(value))
}

/// Whether `omp --resume <value>`, run in `cwd`, can open the session file `session_path`: the
/// same file for a path value (one with a separator or ending in `.jsonl`); otherwise OMP's
/// prefix rule, case-insensitive, on the file's name and on the session id at its end.
pub fn omp_resume_value_matches(value: &str, cwd: &Path, session_path: &str) -> bool {
    if value.contains('/') || value.contains('\\') || value.ends_with(".jsonl") {
        let joined = cwd.join(value);
        let same = |a: &Path, b: &Path| {
            a == b
                || matches!(
                    (std::fs::canonicalize(a), std::fs::canonicalize(b)),
                    (Ok(a), Ok(b)) if a == b
                )
        };
        return same(&joined, Path::new(session_path));
    }
    let prefix = value.to_lowercase();
    let Some(stem) = Path::new(session_path)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .map(str::to_lowercase)
    else {
        return false;
    };
    stem.starts_with(&prefix)
        || stem
            .rsplit_once('_')
            .is_some_and(|(_, id)| id.starts_with(&prefix))
}

pub fn normalize_session_start_source(value: Option<String>) -> Option<String> {
    match value.as_deref().map(str::trim) {
        Some(
            source @ ("startup" | "resume" | "clear" | "compact" | "branch" | "new" | "fork"
            | "select"),
        ) => Some(source.to_string()),
        _ => None,
    }
}

pub fn is_reserved_native_state_source(source: &str, agent: &str) -> bool {
    matches!(
        (source, agent),
        ("herdr:claude", "claude")
            | ("herdr:codex", "codex")
            | ("herdr:copilot", "copilot")
            | ("herdr:devin", "devin")
            | ("herdr:droid", "droid")
            | ("herdr:qodercli", "qodercli")
            | ("herdr:qwen", "qwen")
            | ("herdr:cursor", "cursor")
            | ("herdr:grok", "grok")
    )
}

pub fn session_ref_from_snapshot(
    source: &str,
    agent: &str,
    kind: AgentSessionRefKind,
    value: &str,
) -> Option<PersistedAgentSession> {
    if !is_official_agent_source(source, agent) {
        return None;
    }
    let session_ref = match (agent, kind) {
        ("pi" | "omp", AgentSessionRefKind::Path) => AgentSessionRef::path(value)?,
        (_, AgentSessionRefKind::Id) => AgentSessionRef::id(value)?,
        _ => return None,
    };
    Some(PersistedAgentSession {
        source: source.to_string(),
        agent: agent.to_string(),
        session_ref,
        launch_profile: None,
        owner_process: None,
    })
}

pub fn plan(source: &str, agent: &str, session_ref: &AgentSessionRef) -> Option<AgentResumePlan> {
    if !is_official_agent_source(source, agent) {
        return None;
    }

    let argv = match (source, agent, session_ref.kind) {
        ("herdr:claude", "claude", AgentSessionRefKind::Id) => {
            vec![
                "claude".into(),
                "--resume".into(),
                session_ref.value.clone(),
            ]
        }
        ("herdr:codex", "codex", AgentSessionRefKind::Id) => {
            vec!["codex".into(), "resume".into(), session_ref.value.clone()]
        }
        ("herdr:copilot", "copilot", AgentSessionRefKind::Id) => {
            vec!["copilot".into(), format!("--resume={}", session_ref.value)]
        }
        ("herdr:devin", "devin", AgentSessionRefKind::Id) => {
            vec!["devin".into(), "--resume".into(), session_ref.value.clone()]
        }
        ("herdr:droid", "droid", AgentSessionRefKind::Id) => {
            vec!["droid".into(), "--resume".into(), session_ref.value.clone()]
        }
        ("herdr:kimi", "kimi", AgentSessionRefKind::Id) => {
            vec!["kimi".into(), "--session".into(), session_ref.value.clone()]
        }
        ("herdr:mastracode", "mastracode", AgentSessionRefKind::Id) => {
            vec![
                "mastracode".into(),
                "--thread".into(),
                session_ref.value.clone(),
            ]
        }
        ("herdr:pi", "pi", AgentSessionRefKind::Path | AgentSessionRefKind::Id) => {
            vec!["pi".into(), "--session".into(), session_ref.value.clone()]
        }
        ("herdr:omp", "omp", AgentSessionRefKind::Path | AgentSessionRefKind::Id) => {
            // omp resume is `-r, --resume=<value>` (ID prefix or path); it has no
            // `--session` flag, unlike pi.
            vec!["omp".into(), format!("--resume={}", session_ref.value)]
        }
        ("herdr:hermes", "hermes", AgentSessionRefKind::Id) => {
            vec![
                "hermes".into(),
                "--resume".into(),
                session_ref.value.clone(),
            ]
        }
        ("herdr:opencode", "opencode", AgentSessionRefKind::Id) => {
            vec![
                "opencode".into(),
                "--session".into(),
                session_ref.value.clone(),
            ]
        }
        ("herdr:qodercli", "qodercli", AgentSessionRefKind::Id) => {
            vec![
                "qodercli".into(),
                "--resume".into(),
                session_ref.value.clone(),
            ]
        }
        ("herdr:qwen", "qwen", AgentSessionRefKind::Id) => {
            vec!["qwen".into(), "--resume".into(), session_ref.value.clone()]
        }
        ("herdr:kilo", "kilo", AgentSessionRefKind::Id) => {
            vec!["kilo".into(), "--session".into(), session_ref.value.clone()]
        }
        ("herdr:cursor", "cursor", AgentSessionRefKind::Id) => {
            vec![
                if cfg!(windows) {
                    "cursor-agent.cmd"
                } else {
                    "cursor-agent"
                }
                .into(),
                "--resume".into(),
                session_ref.value.clone(),
            ]
        }
        ("herdr:antigravity_cli", "agy", AgentSessionRefKind::Id) => {
            vec![
                "agy".into(),
                "--conversation".into(),
                session_ref.value.clone(),
            ]
        }
        ("herdr:grok", "grok", AgentSessionRefKind::Id) => {
            vec!["grok".into(), "--resume".into(), session_ref.value.clone()]
        }
        ("herdr:letta", "letta", AgentSessionRefKind::Id) => {
            if let Some(agent_id) = session_ref.value.strip_prefix("default:") {
                if agent_id.is_empty() {
                    return None;
                }
                vec![
                    "letta".into(),
                    "--conversation".into(),
                    "default".into(),
                    "--agent".into(),
                    agent_id.into(),
                ]
            } else {
                vec![
                    "letta".into(),
                    "--conversation".into(),
                    session_ref.value.clone(),
                ]
            }
        }
        _ => return None,
    };

    Some(AgentResumePlan {
        agent: agent.to_string(),
        argv,
        dedupe_key: dedupe_key(source, agent, session_ref),
    })
}

pub fn dedupe_key(source: &str, agent: &str, session_ref: &AgentSessionRef) -> String {
    format!(
        "{source}\u{0}{agent}\u{0}{:?}\u{0}{}",
        session_ref.kind, session_ref.value
    )
}

pub(crate) fn is_official_agent_source(source: &str, agent: &str) -> bool {
    matches!(
        (source, agent),
        ("herdr:claude", "claude")
            | ("herdr:codex", "codex")
            | ("herdr:copilot", "copilot")
            | ("herdr:devin", "devin")
            | ("herdr:droid", "droid")
            | ("herdr:kimi", "kimi")
            | ("herdr:omp", "omp")
            | ("herdr:mastracode", "mastracode")
            | ("herdr:pi", "pi")
            | ("herdr:hermes", "hermes")
            | ("herdr:opencode", "opencode")
            | ("herdr:qodercli", "qodercli")
            | ("herdr:qwen", "qwen")
            | ("herdr:kilo", "kilo")
            | ("herdr:cursor", "cursor")
            | ("herdr:antigravity_cli", "agy")
            | ("herdr:grok", "grok")
            | ("herdr:letta", "letta")
    )
}

fn valid_session_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_SESSION_ID_LEN && !value.chars().any(char::is_control)
}

fn valid_session_path(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_SESSION_PATH_LEN
        && !value.chars().any(char::is_control)
        && Path::new(value).is_absolute()
}

/// Trimmed OMP launch profile name, or `None` when it is empty, longer than
/// 64 bytes, or contains control characters. Shared by `agent.start` and the
/// OMP session hook so both accept the same names.
pub fn validate_omp_launch_profile(raw: &str) -> Option<String> {
    let profile = raw.trim();
    (!profile.is_empty() && profile.len() <= 64 && !profile.chars().any(char::is_control))
        .then(|| profile.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn absolute_test_path(name: &str) -> String {
        std::env::current_dir()
            .unwrap()
            .join(name)
            .display()
            .to_string()
    }

    #[cfg(target_os = "linux")]
    fn omp_session(
        owner_process: Option<crate::platform::OwnerProcessIncarnation>,
    ) -> PersistedAgentSession {
        PersistedAgentSession {
            source: "herdr:omp".into(),
            agent: "omp".into(),
            session_ref: AgentSessionRef::path("/tmp/omp-session.jsonl").unwrap(),
            launch_profile: Some("restricted".into()),
            owner_process,
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn omp_recovery_requires_default_profile_and_known_owner() {
        let mut launchers = std::collections::BTreeMap::from([(
            "restricted".into(),
            "/opt/omp restricted/wrapper".into(),
        )]);
        let unknown = omp_session(None);
        assert_eq!(
            omp_recovery_executable(&unknown, &launchers),
            Err(OmpRecoveryBlock::MissingDefaultLauncher)
        );
        launchers.insert("default".into(), "/opt/omp-default".into());
        assert_eq!(
            omp_recovery_executable(&unknown, &launchers),
            Err(OmpRecoveryBlock::UnknownOwner("restricted".into()))
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn omp_recovery_rejects_unattested_owner_sentinels() {
        let launchers = std::collections::BTreeMap::from([
            ("default".into(), "/opt/omp-default".into()),
            ("restricted".into(), "/opt/omp-restricted".into()),
        ]);
        for owner in [
            crate::platform::OwnerProcessIncarnation {
                pid: 0,
                boot_id: "boot-a".into(),
                start_time_ticks: 1,
            },
            crate::platform::OwnerProcessIncarnation {
                pid: 42,
                boot_id: "  ".into(),
                start_time_ticks: 1,
            },
            crate::platform::OwnerProcessIncarnation {
                pid: 42,
                boot_id: "boot-a".into(),
                start_time_ticks: 0,
            },
        ] {
            assert_eq!(
                omp_recovery_executable(&omp_session(Some(owner)), &launchers),
                Err(OmpRecoveryBlock::UnknownOwner("restricted".into()))
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn omp_recovery_blocks_live_owner_and_returns_exact_stale_profile_executable() {
        let launchers = std::collections::BTreeMap::from([
            ("default".into(), "/opt/omp-default".into()),
            ("restricted".into(), "/opt/omp restricted/wrapper".into()),
        ]);
        let live = crate::platform::observe_process(std::process::id())
            .unwrap()
            .unwrap();
        assert_eq!(
            omp_recovery_executable(&omp_session(Some(live.clone())), &launchers),
            Err(OmpRecoveryBlock::LiveOwner(live.pid))
        );
        let mut stale = live;
        stale.start_time_ticks = stale.start_time_ticks.saturating_add(1);
        assert_eq!(
            omp_recovery_executable(&omp_session(Some(stale)), &launchers).unwrap(),
            "/opt/omp restricted/wrapper",
        );
    }

    #[test]
    fn native_state_reservation_excludes_full_lifecycle_sources() {
        assert!(is_reserved_native_state_source("herdr:claude", "claude"));
        assert!(is_reserved_native_state_source("herdr:codex", "codex"));
        assert!(is_reserved_native_state_source("herdr:devin", "devin"));
        assert!(!is_reserved_native_state_source("herdr:kimi", "kimi"));
        assert!(!is_reserved_native_state_source(
            "herdr:opencode",
            "opencode"
        ));
    }

    #[test]
    fn codex_noncanonical_resume_launch_has_no_explicit_session() {
        assert_eq!(
            persisted_session_from_launch_args(
                crate::detect::Agent::Codex,
                &["resume".into(), "codex-session".into()]
            )
            .unwrap()
            .session_ref
            .value,
            "codex-session"
        );
        assert!(persisted_session_from_launch_args(
            crate::detect::Agent::Codex,
            &["resume".into(), "--last".into()]
        )
        .is_none());
        assert!(persisted_session_from_launch_args(
            crate::detect::Agent::Codex,
            &["resume".into(), "not-a-session".into(), "--last".into()]
        )
        .is_none());
        assert!(persisted_session_from_launch_args(
            crate::detect::Agent::Codex,
            &[
                "--remote".into(),
                "ws://example.test".into(),
                "resume".into(),
                "remote-session".into(),
            ]
        )
        .is_none());
    }

    #[test]
    fn planner_allows_supported_agents() {
        let pi_session = absolute_test_path("pi-session.jsonl");
        let omp_session = absolute_test_path("omp-session.jsonl");
        assert_eq!(
            plan(
                "herdr:claude",
                "claude",
                &AgentSessionRef::id("claude-session").unwrap()
            )
            .unwrap()
            .argv,
            vec!["claude", "--resume", "claude-session"]
        );
        assert_eq!(
            plan(
                "herdr:codex",
                "codex",
                &AgentSessionRef::id("codex-session").unwrap()
            )
            .unwrap()
            .argv,
            vec!["codex", "resume", "codex-session"]
        );
        assert_eq!(
            plan(
                "herdr:copilot",
                "copilot",
                &AgentSessionRef::id("copilot-session").unwrap()
            )
            .unwrap()
            .argv,
            vec!["copilot", "--resume=copilot-session"]
        );
        assert_eq!(
            plan(
                "herdr:devin",
                "devin",
                &AgentSessionRef::id("devin-session").unwrap()
            )
            .unwrap()
            .argv,
            vec!["devin", "--resume", "devin-session"]
        );
        assert_eq!(
            plan(
                "herdr:droid",
                "droid",
                &AgentSessionRef::id("droid-session").unwrap()
            )
            .unwrap()
            .argv,
            vec!["droid", "--resume", "droid-session"]
        );
        assert_eq!(
            plan(
                "herdr:kimi",
                "kimi",
                &AgentSessionRef::id("kimi-session").unwrap()
            )
            .unwrap()
            .argv,
            vec!["kimi", "--session", "kimi-session"]
        );
        assert_eq!(
            plan(
                "herdr:mastracode",
                "mastracode",
                &AgentSessionRef::id("mastracode-session").unwrap()
            )
            .unwrap()
            .argv,
            vec!["mastracode", "--thread", "mastracode-session"]
        );
        assert_eq!(
            plan(
                "herdr:pi",
                "pi",
                &AgentSessionRef::path(&pi_session).unwrap()
            )
            .unwrap()
            .argv,
            vec!["pi", "--session", pi_session.as_str()]
        );
        assert_eq!(
            plan(
                "herdr:omp",
                "omp",
                &AgentSessionRef::path(&omp_session).unwrap()
            )
            .unwrap()
            .argv,
            vec!["omp", format!("--resume={omp_session}").as_str()]
        );
        assert_eq!(
            plan(
                "herdr:hermes",
                "hermes",
                &AgentSessionRef::id("hermes-session").unwrap()
            )
            .unwrap()
            .argv,
            vec!["hermes", "--resume", "hermes-session"]
        );
        assert_eq!(
            plan(
                "herdr:opencode",
                "opencode",
                &AgentSessionRef::id("opencode-session").unwrap()
            )
            .unwrap()
            .argv,
            vec!["opencode", "--session", "opencode-session"]
        );
        assert_eq!(
            plan(
                "herdr:qodercli",
                "qodercli",
                &AgentSessionRef::id("qoder-session").unwrap()
            )
            .unwrap()
            .argv,
            vec!["qodercli", "--resume", "qoder-session"]
        );
        assert_eq!(
            plan(
                "herdr:qwen",
                "qwen",
                &AgentSessionRef::id("qwen-session").unwrap()
            )
            .unwrap()
            .argv,
            vec!["qwen", "--resume", "qwen-session"]
        );
        assert_eq!(
            plan(
                "herdr:kilo",
                "kilo",
                &AgentSessionRef::id("kilo-session").unwrap()
            )
            .unwrap()
            .argv,
            vec!["kilo", "--session", "kilo-session"]
        );
        assert_eq!(
            plan(
                "herdr:cursor",
                "cursor",
                &AgentSessionRef::id("cursor-session").unwrap()
            )
            .unwrap()
            .argv,
            vec![
                if cfg!(windows) {
                    "cursor-agent.cmd"
                } else {
                    "cursor-agent"
                },
                "--resume",
                "cursor-session",
            ]
        );
        assert_eq!(
            plan(
                "herdr:antigravity_cli",
                "agy",
                &AgentSessionRef::id("agy-session").unwrap()
            )
            .unwrap()
            .argv,
            vec!["agy", "--conversation", "agy-session"]
        );
        assert_eq!(
            plan(
                "herdr:grok",
                "grok",
                &AgentSessionRef::id("grok-session").unwrap()
            )
            .unwrap()
            .argv,
            vec!["grok", "--resume", "grok-session"]
        );
        assert_eq!(
            plan(
                "herdr:letta",
                "letta",
                &AgentSessionRef::id("conversation-123").unwrap()
            )
            .unwrap()
            .argv,
            vec!["letta", "--conversation", "conversation-123"]
        );
        assert_eq!(
            plan(
                "herdr:letta",
                "letta",
                &AgentSessionRef::id("default:agent-123").unwrap()
            )
            .unwrap()
            .argv,
            vec!["letta", "--conversation", "default", "--agent", "agent-123"]
        );
        assert!(plan(
            "herdr:letta",
            "letta",
            &AgentSessionRef::id("default:").unwrap()
        )
        .is_none());
    }

    #[test]
    fn planner_rejects_custom_and_unsupported_path_refs() {
        let claude_session = absolute_test_path("claude-session");
        assert!(plan(
            "custom:claude",
            "claude",
            &AgentSessionRef::id("session").unwrap()
        )
        .is_none());
        assert!(plan(
            "herdr:claude",
            "claude",
            &AgentSessionRef::path(&claude_session).unwrap()
        )
        .is_none());
    }

    #[test]
    fn report_ref_prefers_pi_and_omp_paths_and_validates_values() {
        let pi_session = absolute_test_path("pi-session.jsonl");
        let omp_session = absolute_test_path("omp-session.jsonl");
        let claude_session = absolute_test_path("claude-session");
        let copilot_session = absolute_test_path("copilot-session");
        let session_ref = session_ref_from_report(
            "herdr:pi",
            "pi",
            Some("pi-id".into()),
            Some(pi_session.clone()),
        )
        .unwrap();
        assert_eq!(session_ref.kind, AgentSessionRefKind::Path);
        assert_eq!(session_ref.value, pi_session);

        assert!(session_ref_from_report("herdr:pi", "pi", Some("bad\nid".into()), None).is_none());
        assert!(
            session_ref_from_report("herdr:pi", "pi", None, Some("relative.jsonl".into()))
                .is_none()
        );
        assert!(session_ref_from_report("custom:pi", "pi", Some("pi-id".into()), None).is_none());

        let session_ref = session_ref_from_report(
            "herdr:omp",
            "omp",
            Some("omp-id".into()),
            Some(omp_session.clone()),
        )
        .unwrap();
        assert_eq!(session_ref.kind, AgentSessionRefKind::Path);
        assert_eq!(session_ref.value, omp_session);

        let session_ref =
            session_ref_from_report("herdr:omp", "omp", Some("omp-id".into()), None).unwrap();
        assert_eq!(session_ref.kind, AgentSessionRefKind::Id);
        assert_eq!(session_ref.value, "omp-id");
        let session_ref = session_ref_from_report(
            "herdr:omp",
            "omp",
            Some("omp-id".into()),
            Some("relative.jsonl".into()),
        )
        .unwrap();
        assert_eq!(session_ref.kind, AgentSessionRefKind::Id);
        assert_eq!(session_ref.value, "omp-id");
        assert!(
            session_ref_from_report("herdr:omp", "omp", None, Some("relative.jsonl".into()))
                .is_none()
        );

        assert!(
            session_ref_from_report("herdr:claude", "claude", None, Some(claude_session)).is_none()
        );

        let session_ref =
            session_ref_from_report("herdr:copilot", "copilot", Some("copilot-id".into()), None)
                .unwrap();
        assert_eq!(session_ref.kind, AgentSessionRefKind::Id);
        assert_eq!(session_ref.value, "copilot-id");
        assert!(
            session_ref_from_report("herdr:copilot", "copilot", None, Some(copilot_session))
                .is_none()
        );

        let session_ref =
            session_ref_from_report("herdr:devin", "devin", Some("devin-id".into()), None).unwrap();
        assert_eq!(session_ref.kind, AgentSessionRefKind::Id);
        assert_eq!(session_ref.value, "devin-id");

        let session_ref =
            session_ref_from_report("herdr:droid", "droid", Some("droid-id".into()), None).unwrap();
        assert_eq!(session_ref.kind, AgentSessionRefKind::Id);
        assert_eq!(session_ref.value, "droid-id");
        assert!(session_ref_from_report(
            "herdr:droid",
            "droid",
            None,
            Some("/tmp/droid-session".into())
        )
        .is_none());

        let session_ref =
            session_ref_from_report("herdr:kimi", "kimi", Some("kimi-id".into()), None).unwrap();
        assert_eq!(session_ref.kind, AgentSessionRefKind::Id);
        assert_eq!(session_ref.value, "kimi-id");

        let session_ref = session_ref_from_report(
            "herdr:mastracode",
            "mastracode",
            Some("mastracode-id".into()),
            None,
        )
        .unwrap();
        assert_eq!(session_ref.kind, AgentSessionRefKind::Id);
        assert_eq!(session_ref.value, "mastracode-id");

        let session_ref =
            session_ref_from_report("herdr:kilo", "kilo", Some("kilo-id".into()), None).unwrap();
        assert_eq!(session_ref.kind, AgentSessionRefKind::Id);
        assert_eq!(session_ref.value, "kilo-id");

        let session_ref =
            session_ref_from_report("herdr:qodercli", "qodercli", Some("qoder-id".into()), None)
                .unwrap();
        assert_eq!(session_ref.kind, AgentSessionRefKind::Id);
        assert_eq!(session_ref.value, "qoder-id");

        let session_ref =
            session_ref_from_report("herdr:qwen", "qwen", Some("qwen-id".into()), None).unwrap();
        assert_eq!(session_ref.kind, AgentSessionRefKind::Id);
        assert_eq!(session_ref.value, "qwen-id");

        let session_ref =
            session_ref_from_report("herdr:antigravity_cli", "agy", Some("agy-id".into()), None)
                .unwrap();
        assert_eq!(session_ref.kind, AgentSessionRefKind::Id);
        assert_eq!(session_ref.value, "agy-id");
    }

    #[test]
    fn normalize_session_start_source_allows_known_values() {
        assert_eq!(
            normalize_session_start_source(Some("startup".into())),
            Some("startup".into())
        );
        assert_eq!(
            normalize_session_start_source(Some("resume".into())),
            Some("resume".into())
        );
        assert_eq!(
            normalize_session_start_source(Some("clear".into())),
            Some("clear".into())
        );
        assert_eq!(
            normalize_session_start_source(Some("compact".into())),
            Some("compact".into())
        );
        assert_eq!(
            normalize_session_start_source(Some("branch".into())),
            Some("branch".into())
        );
        assert_eq!(
            normalize_session_start_source(Some("new".into())),
            Some("new".into())
        );
        assert_eq!(
            normalize_session_start_source(Some("fork".into())),
            Some("fork".into())
        );
        assert_eq!(
            normalize_session_start_source(Some("select".into())),
            Some("select".into())
        );
        assert_eq!(
            normalize_session_start_source(Some(" resume ".into())),
            Some("resume".into())
        );
        assert_eq!(normalize_session_start_source(Some("other".into())), None);
        assert_eq!(normalize_session_start_source(None), None);
    }

    #[test]
    fn ids_are_data_not_shell_text() {
        let id = "abc; rm -rf /";
        let codex_plan = plan("herdr:codex", "codex", &AgentSessionRef::id(id).unwrap()).unwrap();
        assert_eq!(codex_plan.argv, vec!["codex", "resume", id]);

        let copilot_plan = plan(
            "herdr:copilot",
            "copilot",
            &AgentSessionRef::id(id).unwrap(),
        )
        .unwrap();
        assert_eq!(copilot_plan.argv, vec!["copilot", "--resume=abc; rm -rf /"]);

        let devin_plan = plan("herdr:devin", "devin", &AgentSessionRef::id(id).unwrap()).unwrap();
        assert_eq!(devin_plan.argv, vec!["devin", "--resume", id]);
    }

    #[test]
    fn planner_rejects_path_refs_for_id_only_agents() {
        let hermes_session = absolute_test_path("hermes-session");
        let opencode_session = absolute_test_path("opencode-session");
        let kilo_session = absolute_test_path("kilo-session");
        let copilot_session = absolute_test_path("copilot-session");
        let devin_session = absolute_test_path("devin-session");
        assert!(plan(
            "herdr:hermes",
            "hermes",
            &AgentSessionRef::path(&hermes_session).unwrap()
        )
        .is_none());
        assert!(plan(
            "herdr:opencode",
            "opencode",
            &AgentSessionRef::path(&opencode_session).unwrap()
        )
        .is_none());
        assert!(plan(
            "herdr:kilo",
            "kilo",
            &AgentSessionRef::path(&kilo_session).unwrap()
        )
        .is_none());
        assert!(plan(
            "herdr:copilot",
            "copilot",
            &AgentSessionRef::path(&copilot_session).unwrap()
        )
        .is_none());
        assert!(plan(
            "herdr:devin",
            "devin",
            &AgentSessionRef::path(&devin_session).unwrap()
        )
        .is_none());
        assert!(session_ref_from_snapshot(
            "herdr:mastracode",
            "mastracode",
            AgentSessionRefKind::Id,
            "mastracode-session"
        )
        .is_some());
        assert!(session_ref_from_snapshot(
            "herdr:hermes",
            "hermes",
            AgentSessionRefKind::Id,
            "hermes-session"
        )
        .is_some());
        assert!(session_ref_from_snapshot(
            "herdr:opencode",
            "opencode",
            AgentSessionRefKind::Id,
            "opencode-session"
        )
        .is_some());
        assert!(session_ref_from_snapshot(
            "herdr:kilo",
            "kilo",
            AgentSessionRefKind::Id,
            "kilo-session"
        )
        .is_some());
        assert!(session_ref_from_snapshot(
            "herdr:copilot",
            "copilot",
            AgentSessionRefKind::Id,
            "copilot-session"
        )
        .is_some());
        assert!(session_ref_from_snapshot(
            "herdr:devin",
            "devin",
            AgentSessionRefKind::Id,
            "devin-session"
        )
        .is_some());
        assert!(session_ref_from_snapshot(
            "herdr:antigravity_cli",
            "agy",
            AgentSessionRefKind::Id,
            "agy-session"
        )
        .is_some());
        let agy_session = absolute_test_path("agy-session");
        assert!(plan(
            "herdr:antigravity_cli",
            "agy",
            &AgentSessionRef::path(&agy_session).unwrap()
        )
        .is_none());
    }

    #[test]
    fn omp_continue_flags_are_found_and_the_newest_session_is_the_latest_file() {
        let args = |list: &[&str]| list.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
        let id = "0A1B2C3D-4e5f-6789-abcd-ef0123456789";
        assert_eq!(omp_continue(&args(&["--continue"])), OmpContinue::Newest);
        assert_eq!(
            omp_continue(&args(&["--model", "x", "-c"])),
            OmpContinue::Newest
        );
        assert_eq!(
            omp_continue(&args(&["-c", "fix the bug"])),
            OmpContinue::Newest
        );
        let with_id = args(&["--continue", id]);
        assert_eq!(omp_continue(&with_id), OmpContinue::Session(id));
        // Another flag decides the session or the directory, or the flag is text after `--`.
        for other in [
            vec!["--resume", "abc"],
            vec!["--continue", "--resume", "abc"],
            vec!["--session-dir", "/elsewhere", "--continue"],
            vec!["--continue", "--session-dir=/elsewhere"],
            vec!["--continue", "--fork", "abc"],
            vec!["--continue", "--no-session"],
            vec!["--continue-later"],
            vec!["--", "--continue"],
            vec![],
        ] {
            assert_eq!(omp_continue(&args(&other)), OmpContinue::Other, "{other:?}");
        }

        let dir = std::env::temp_dir().join(format!(
            "herdr-continue-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let older = dir.join("2026-01-01T00-00-00-000Z_aaa.jsonl");
        let newer = dir.join("2026-01-02T00-00-00-000Z_bbb.jsonl");
        std::fs::write(&older, "{}\n").unwrap();
        std::fs::write(dir.join("notes.txt"), "x").unwrap();
        std::fs::create_dir(dir.join("2026-01-03T00-00-00-000Z_ccc.jsonl")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&newer, "{}\n").unwrap();
        let found = newest_session_beside(older.to_str().unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(found, Some(newer));
        assert_eq!(
            newest_session_beside("/nonexistent-dir-for-herdr/x.jsonl"),
            None
        );
    }
}
