//! The state socket: a second API socket that accepts only the reports an agent integration
//! sends about its own pane. Each pane gets the socket path and a token bound to its pane id, so
//! an agent can report its state without holding the full API socket.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::api::schema::{ErrorBody, ErrorResponse, Method, Request};

pub const STATE_SOCKET_PATH_ENV_VAR: &str = "HERDR_STATE_SOCKET_PATH";
pub const STATE_TOKEN_ENV_VAR: &str = "HERDR_STATE_TOKEN";
/// Top-level request field that carries the pane's token on the state socket.
const TOKEN_FIELD: &str = "state_token";
const KEY_LEN: usize = 32;
#[cfg(unix)]
const KEY_FILE_MODE: u32 = 0o600;

pub(crate) type StateKey = [u8; KEY_LEN];

/// Every method an agent integration sends about its own pane; each takes `params.pane_id`.
pub(crate) const ALLOWED_METHODS: [&str; 6] = [
    "pane.report_agent",
    "pane.report_agent_session_v2",
    "pane.report_omp_detail",
    "pane.report_metadata",
    "pane.ack_instruction",
    "pane.ack_action",
];

/// The state socket this process serves, read when a pane spawns, with the generation of the
/// server handle that activated it.
static ACTIVE: RwLock<Option<Active>> = RwLock::new(None);
static GENERATION: AtomicU64 = AtomicU64::new(0);

struct Active {
    generation: u64,
    path: PathBuf,
    key: StateKey,
}

pub(crate) fn state_socket_path(api_socket_path: &Path) -> PathBuf {
    sibling(api_socket_path, "state.sock")
}

pub(crate) fn key_path(api_socket_path: &Path) -> PathBuf {
    sibling(api_socket_path, "state.key")
}

fn sibling(api_socket_path: &Path, suffix: &str) -> PathBuf {
    let stem = api_socket_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("herdr");
    let parent = api_socket_path.parent().unwrap_or_else(|| Path::new(""));
    parent.join(format!("{stem}-{suffix}"))
}

/// The key survives live handoff, so tokens in the environments of panes that outlive the old
/// process stay valid. A cold start discards it first (`discard_key`).
pub(crate) fn load_or_create_key(path: &Path) -> io::Result<StateKey> {
    match fs::read(path) {
        Ok(bytes) => bytes.try_into().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} is not a {KEY_LEN}-byte key", path.display()),
            )
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut key = [0u8; KEY_LEN];
            getrandom::fill(&mut key).map_err(|error| io::Error::other(error.to_string()))?;
            let temporary = path.with_extension("key.tmp");
            match fs::remove_file(&temporary) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
                _ => {}
            }
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(KEY_FILE_MODE);
            }
            let mut file = options.open(&temporary)?;
            file.write_all(&key)?;
            file.sync_all()?;
            fs::rename(&temporary, path)?;
            Ok(key)
        }
        Err(error) => Err(error),
    }
}

/// A cold start has no pane that outlived the previous server, so its tokens (pane ids repeat
/// across server lifetimes) must stop working.
pub(crate) fn discard_key(api_socket_path: &Path) -> io::Result<()> {
    match fs::remove_file(key_path(api_socket_path)) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// HMAC-SHA256 of the pane id, hex encoded.
pub(crate) fn token(key: &StateKey, pane_id: &str) -> String {
    let mut inner = [0x36u8; 64];
    let mut outer = [0x5cu8; 64];
    for (index, byte) in key.iter().enumerate() {
        inner[index] ^= byte;
        outer[index] ^= byte;
    }
    let inner_hash = Sha256::new()
        .chain_update(inner)
        .chain_update(pane_id.as_bytes())
        .finalize();
    let mac = Sha256::new()
        .chain_update(outer)
        .chain_update(inner_hash)
        .finalize();
    mac.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn same_token(left: &str, right: &str) -> bool {
    left.len() == right.len()
        && left
            .bytes()
            .zip(right.bytes())
            .fold(0u8, |diff, (a, b)| diff | (a ^ b))
            == 0
}

/// Returns the generation that `deactivate` needs, so a handle dropped after its replacement
/// started (restart, failed-handoff rollback) cannot clear the replacement.
pub(crate) fn activate(path: PathBuf, key: StateKey) -> u64 {
    let generation = GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
    if let Ok(mut active) = ACTIVE.write() {
        *active = Some(Active {
            generation,
            path,
            key,
        });
    }
    generation
}

pub(crate) fn deactivate(generation: u64) {
    if let Ok(mut active) = ACTIVE.write() {
        if active
            .as_ref()
            .is_some_and(|active| active.generation == generation)
        {
            *active = None;
        }
    }
}

/// The variables a pane gets while this process serves a state socket.
pub(crate) fn pane_env(pane_id: &str) -> Option<[(&'static str, String); 2]> {
    let active = ACTIVE.read().ok()?;
    let active = active.as_ref()?;
    Some([
        (
            STATE_SOCKET_PATH_ENV_VAR,
            active.path.to_string_lossy().into_owned(),
        ),
        (STATE_TOKEN_ENV_VAR, token(&active.key, pane_id)),
    ])
}

/// The pane an allowed method reports for.
fn reported_pane(method: &Method) -> Option<&str> {
    match method {
        Method::PaneReportAgent(params) => Some(&params.pane_id),
        Method::PaneReportAgentSessionV2(params) => Some(&params.pane_id),
        Method::PaneReportOmpDetail(params) => Some(&params.pane_id),
        Method::PaneReportMetadata(params) => Some(&params.pane_id),
        Method::PaneAckInstruction(params) => Some(&params.pane_id),
        Method::PaneAckAction(params) => Some(&params.pane_id),
        _ => None,
    }
}

fn error(id: String, code: &str, message: String) -> ErrorResponse {
    ErrorResponse {
        id,
        error: ErrorBody {
            code: code.into(),
            message,
        },
    }
}

/// Parses a state-socket request line and admits it only when it is an allowed report whose
/// `state_token` matches its `pane_id`. Other methods are refused before their params are read.
pub(crate) fn authorize(line: &str, key: &StateKey) -> Result<Request, ErrorResponse> {
    let mut value: Value = serde_json::from_str(line).map_err(|err| {
        error(
            String::new(),
            "invalid_request",
            format!("invalid request: {err}"),
        )
    })?;
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let method = value
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !ALLOWED_METHODS.contains(&method) {
        return Err(error(
            id,
            "forbidden",
            format!("{method:?} is not available on the state socket"),
        ));
    }
    let presented = value
        .as_object_mut()
        .and_then(|object| object.remove(TOKEN_FIELD));
    let request: Request = serde_json::from_value(value).map_err(|err| {
        error(
            id.clone(),
            "invalid_request",
            format!("invalid request: {err}"),
        )
    })?;
    let Some(pane_id) = reported_pane(&request.method) else {
        return Err(error(
            id,
            "forbidden",
            "method is not available on the state socket".into(),
        ));
    };
    let Some(Value::String(presented)) = presented else {
        return Err(error(
            id,
            "forbidden",
            format!("{TOKEN_FIELD} is required on the state socket"),
        ));
    };
    if !same_token(&presented, &token(key, pane_id)) {
        return Err(error(
            id,
            "forbidden",
            format!("{TOKEN_FIELD} does not belong to pane {pane_id}"),
        ));
    }
    Ok(request)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: StateKey = [7; KEY_LEN];

    fn line(method: &str, params: Value, token: Option<&str>) -> String {
        let mut request = serde_json::json!({ "id": "r1", "method": method, "params": params });
        if let Some(token) = token {
            request[TOKEN_FIELD] = Value::String(token.into());
        }
        request.to_string()
    }

    fn report(pane_id: &str) -> Value {
        serde_json::json!({
            "pane_id": pane_id,
            "source": "herdr:omp",
            "agent": "omp",
            "state": "working",
            "seq": 1,
        })
    }

    #[test]
    fn admits_a_report_for_the_tokens_pane() {
        let request = authorize(
            &line(
                "pane.report_agent",
                report("w1:p1"),
                Some(&token(&KEY, "w1:p1")),
            ),
            &KEY,
        )
        .expect("admitted");
        assert!(matches!(request.method, Method::PaneReportAgent(_)));
    }

    #[test]
    fn refuses_a_report_for_another_pane() {
        let refused = authorize(
            &line(
                "pane.report_agent",
                report("w1:p2"),
                Some(&token(&KEY, "w1:p1")),
            ),
            &KEY,
        )
        .expect_err("refused");
        assert_eq!(refused.id, "r1");
        assert_eq!(refused.error.code, "forbidden");
    }

    #[test]
    fn refuses_a_report_without_a_token_or_with_another_keys_token() {
        for presented in [None, Some(token(&[8; KEY_LEN], "w1:p1"))] {
            let refused = authorize(
                &line("pane.report_agent", report("w1:p1"), presented.as_deref()),
                &KEY,
            )
            .expect_err("refused");
            assert_eq!(refused.error.code, "forbidden");
        }
    }

    #[test]
    fn refuses_every_method_outside_the_allowlist() {
        let schema = serde_json::to_string(&schemars::schema_for!(Request)).expect("schema");
        let names: Vec<String> = regex::Regex::new(r#""method":\{[^{}]*"const":"([^"]+)""#)
            .expect("regex")
            .captures_iter(&schema)
            .map(|captures| captures[1].to_string())
            .collect();
        for allowed in ALLOWED_METHODS {
            assert!(
                names.iter().any(|name| name == allowed),
                "{allowed} is a method"
            );
        }
        let token = token(&KEY, "w1:p1");
        let refused: Vec<&String> = names
            .iter()
            .filter(|name| !ALLOWED_METHODS.contains(&name.as_str()))
            .collect();
        assert!(refused.len() > 50, "found {} methods", names.len());
        for name in refused {
            let response =
                authorize(&line(name, report("w1:p1"), Some(&token)), &KEY).expect_err("refused");
            assert_eq!(response.error.code, "forbidden", "{name}");
        }
    }

    #[test]
    fn a_replaced_handle_does_not_deactivate_its_replacement() {
        let old = activate(PathBuf::from("/old/herdr-state.sock"), KEY);
        let new = activate(PathBuf::from("/new/herdr-state.sock"), KEY);
        deactivate(old);
        let env = pane_env("w1:p1").expect("replacement stays active");
        assert_eq!(env[0].1, "/new/herdr-state.sock");
        deactivate(new);
        assert!(pane_env("w1:p1").is_none());
    }

    #[test]
    fn discarding_the_key_makes_a_new_one() {
        let dir = std::env::temp_dir().join(format!("herdr-state-rotate-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("dir");
        let socket = dir.join("herdr.sock");
        let first = load_or_create_key(&key_path(&socket)).expect("first");
        discard_key(&socket).expect("discard");
        discard_key(&socket).expect("discard is idempotent");
        assert_ne!(
            load_or_create_key(&key_path(&socket)).expect("second"),
            first
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn key_is_created_once_and_reloaded() {
        let dir = std::env::temp_dir().join(format!("herdr-state-key-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("dir");
        let path = key_path(&dir.join("herdr.sock"));
        let _ = fs::remove_file(&path);
        let created = load_or_create_key(&path).expect("created");
        assert_eq!(load_or_create_key(&path).expect("reloaded"), created);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).expect("metadata").permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
