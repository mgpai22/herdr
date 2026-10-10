//! The state socket: a second API socket that accepts only the reports an agent integration
//! sends about its own pane. A pane created with `HERDR_STATE_SOCKET=1` in its env gets the
//! socket path and a token bound to its pane id, so a launcher can withhold the full API socket
//! from its agent and the agent still reports its state.
//!
//! This stops an agent that only has the env from steering Herdr; it does not stop a same-uid
//! process that reads the key or connects to the full socket by path.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::api::schema::{ErrorBody, ErrorResponse, Method, Request};

pub const STATE_SOCKET_PATH_ENV_VAR: &str = "HERDR_STATE_SOCKET_PATH";
pub const STATE_TOKEN_ENV_VAR: &str = "HERDR_STATE_TOKEN";
/// Set to `1` in the env a pane is created with to give it the state socket.
// ponytail: not stored per pane, so a respawned shell, a resumed agent and a restored pane go
// without; a terminal-state flag saved in the snapshot would carry it.
pub const STATE_SOCKET_REQUEST_ENV_VAR: &str = "HERDR_STATE_SOCKET";
/// Top-level request field that carries the pane's token on the state socket.
const TOKEN_FIELD: &str = "state_token";
const KEY_LEN: usize = 32;
#[cfg(unix)]
const KEY_FILE_MODE: u32 = 0o600;

pub(crate) type StateKey = [u8; KEY_LEN];

/// The OMP integration's reports and acks, plus `pane.report_metadata` for metadata extensions
/// (life-os `herdr-metadata.ts`): labels of the token's own pane, nothing that acts. Each method
/// takes `params.pane_id`.
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
    // Checked on the handle that is read, so the file cannot be swapped between check and read.
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    match options.open(path) {
        Ok(mut file) => {
            trusted_key_file(path, &file.metadata()?)?;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            bytes.try_into().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{} is not a {KEY_LEN}-byte key", path.display()),
                )
            })
        }
        #[cfg(unix)]
        Err(error) if error.raw_os_error() == Some(libc::ELOOP) => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is a symlink", path.display()),
        )),
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

/// A key file someone else could have written or read would let them mint tokens.
fn trusted_key_file(path: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    let refuse = |why: &str| {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} {why}", path.display()),
        ))
    };
    if !metadata.file_type().is_file() {
        return refuse("is not a regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid has no preconditions and cannot fail.
        if metadata.uid() != unsafe { libc::geteuid() } {
            return refuse("belongs to another user");
        }
        if metadata.mode() & 0o077 != 0 {
            return refuse("is readable or writable by other users");
        }
    }
    Ok(())
}

/// A cold start has no pane that outlived the previous server, so its tokens (pane ids repeat
/// across server lifetimes) must stop working.
pub(crate) fn discard_key(api_socket_path: &Path) -> io::Result<()> {
    match fs::remove_file(key_path(api_socket_path)) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// The pane's token: HMAC-SHA256 of the pane id, hex encoded.
pub(crate) fn token(key: &StateKey, pane_id: &str) -> String {
    hmac_sha256(key, pane_id.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// RFC 2104 HMAC over SHA-256.
fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let hashed;
    let key = if key.len() > 64 {
        hashed = Sha256::digest(key);
        hashed.as_slice()
    } else {
        key
    };
    let mut inner = [0x36u8; 64];
    let mut outer = [0x5cu8; 64];
    for (index, byte) in key.iter().enumerate() {
        inner[index] ^= byte;
        outer[index] ^= byte;
    }
    let inner_hash = Sha256::new()
        .chain_update(inner)
        .chain_update(data)
        .finalize();
    Sha256::new()
        .chain_update(outer)
        .chain_update(inner_hash)
        .finalize()
        .into()
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
    // Refusals name no sender-supplied value: the method or pane id could be up to the 1 MiB
    // request cap.
    if !ALLOWED_METHODS.contains(&method) {
        return Err(error(
            id,
            "forbidden",
            "method is not available on the state socket".into(),
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
            format!("{TOKEN_FIELD} does not belong to params.pane_id"),
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
    fn malformed_requests_are_invalid_not_dispatched() {
        let token = token(&KEY, "w1:p1");
        for bad in [
            "not json".to_string(),
            line(
                "pane.report_agent",
                serde_json::json!({ "pane_id": "w1:p1" }),
                Some(&token),
            ),
        ] {
            assert_eq!(
                authorize(&bad, &KEY).expect_err("refused").error.code,
                "invalid_request"
            );
        }
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

    #[cfg(unix)]
    #[test]
    fn a_key_others_can_read_or_a_symlinked_key_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("herdr-state-trust-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("dir");
        let path = key_path(&dir.join("herdr.sock"));
        let _ = fs::remove_file(&path);
        load_or_create_key(&path).expect("created");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod");
        assert_eq!(
            load_or_create_key(&path).expect_err("readable").kind(),
            io::ErrorKind::PermissionDenied
        );
        let real = dir.join("real.key");
        fs::rename(&path, &real).expect("move");
        fs::set_permissions(&real, fs::Permissions::from_mode(0o600)).expect("chmod");
        std::os::unix::fs::symlink(&real, &path).expect("symlink");
        assert_eq!(
            load_or_create_key(&path).expect_err("symlink").kind(),
            io::ErrorKind::PermissionDenied
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// RFC 4231 test cases 1, 2 and 6 (a key longer than the block).
    #[test]
    fn hmac_matches_rfc_4231() {
        let hex =
            |bytes: [u8; 32]| -> String { bytes.iter().map(|b| format!("{b:02x}")).collect() };
        assert_eq!(
            hex(hmac_sha256(&[0x0b; 20], b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        assert_eq!(
            hex(hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        assert_eq!(
            hex(hmac_sha256(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }
}
