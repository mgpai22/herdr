use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
#[cfg(test)]
use std::sync::{Mutex, MutexGuard, OnceLock};

use portable_pty::CommandBuilder;

pub(crate) const HERDR_PANE_ID_ENV_VAR: &str = "HERDR_PANE_ID";
pub(crate) const HERDR_TAB_ID_ENV_VAR: &str = "HERDR_TAB_ID";
pub(crate) const HERDR_WORKSPACE_ID_ENV_VAR: &str = "HERDR_WORKSPACE_ID";

pub(crate) const PI_CODING_AGENT_DIR_ENV_VAR: &str = "PI_CODING_AGENT_DIR";
pub(crate) const OMP_CONFIG_DIR_ENV_VAR: &str = "PI_CONFIG_DIR";
pub(crate) const CLAUDE_CONFIG_DIR_ENV_VAR: &str = "CLAUDE_CONFIG_DIR";
pub(crate) const CODEX_HOME_ENV_VAR: &str = "CODEX_HOME";
pub(crate) const KIMI_CODE_HOME_ENV_VAR: &str = "KIMI_CODE_HOME";
pub(crate) const COPILOT_HOME_ENV_VAR: &str = "COPILOT_HOME";
pub(crate) const QODERCLI_CONFIG_DIR_ENV_VAR: &str = "QODER_CONFIG_DIR";
pub(crate) const QWEN_HOME_ENV_VAR: &str = "QWEN_HOME";
pub(crate) const CURSOR_CONFIG_DIR_ENV_VAR: &str = "CURSOR_CONFIG_DIR";
pub(crate) const ANTIGRAVITY_CLI_CONFIG_DIR_ENV_VAR: &str = "ANTIGRAVITY_CLI_CONFIG_DIR";
pub(crate) const GROK_CONFIG_DIR_ENV_VAR: &str = "GROK_CONFIG_DIR";
/// The grok CLI's own config-home override (documented alongside
/// `$GROK_HOME/config.toml` and `$GROK_HOME/auth.json`).
pub(crate) const GROK_HOME_ENV_VAR: &str = "GROK_HOME";
pub(crate) const HERMES_HOME_ENV_VAR: &str = "HERMES_HOME";

pub(crate) fn apply_pane_base_env(cmd: &mut CommandBuilder) {
    cmd.env(crate::api::SOCKET_PATH_ENV_VAR, crate::api::socket_path());
    if let Ok(executable) = crate::platform::launch_executable() {
        cmd.env("HERDR_BIN_PATH", executable);
    }
}

pub(crate) fn pi_extension_dir() -> io::Result<PathBuf> {
    Ok(
        config_dir_from_env_or_home(PI_CODING_AGENT_DIR_ENV_VAR, &[".pi", "agent"])?
            .join("extensions"),
    )
}

/// Extension dir of the default OMP profile.
pub(crate) fn omp_extension_dir() -> io::Result<PathBuf> {
    if let Some(value) =
        std::env::var_os(PI_CODING_AGENT_DIR_ENV_VAR).filter(|value| !value.is_empty())
    {
        let agent_dir = expand_tilde_path(PathBuf::from(value))?;
        // A named OMP profile exports its own agent dir to child processes. Like OMP, do not
        // take that as the default profile's dir; the profile scan lists it anyway.
        if !is_omp_profile_agent_dir(&agent_dir) {
            return Ok(agent_dir.join("extensions"));
        }
    }

    Ok(omp_config_root()?.join("agent").join("extensions"))
}

/// True for `<OMP config root>/profiles/<name>/agent`, the agent dir OMP derives for a named
/// profile.
pub(crate) fn is_omp_profile_agent_dir(dir: &Path) -> bool {
    let Ok(profiles) = omp_config_root().map(|root| root.join("profiles")) else {
        return false;
    };
    let Ok(rest) = dir.strip_prefix(profiles) else {
        return false;
    };
    let mut parts = rest.components();
    matches!(
        (parts.next(), parts.next(), parts.next()),
        (Some(Component::Normal(_)), Some(Component::Normal(agent)), None) if agent == "agent"
    )
}

/// Extension dirs of every OMP profile: the default agent dir first, then the
/// existing agent dir of each named profile. OMP loads extensions only from
/// the active profile's agent dir, so each profile needs its own copy.
pub(crate) fn omp_extension_dirs() -> io::Result<Vec<PathBuf>> {
    let mut dirs = vec![omp_extension_dir()?];
    // No home or no profiles dir means no named profiles; the default dir
    // may still come from PI_CODING_AGENT_DIR.
    let Some(entries) = omp_config_root()
        .and_then(|root| fs::read_dir(root.join("profiles")))
        .ok()
    else {
        return Ok(dirs);
    };
    let mut profile_dirs = entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            is_omp_profile_name(entry.file_name().to_str()?).then(|| entry.path().join("agent"))
        })
        .filter(|agent_dir| agent_dir.is_dir())
        .map(|agent_dir| agent_dir.join("extensions"))
        .filter(|dir| *dir != dirs[0])
        .collect::<Vec<_>>();
    profile_dirs.sort();
    dirs.append(&mut profile_dirs);
    Ok(dirs)
}

fn omp_config_root() -> io::Result<PathBuf> {
    let config_dir = std::env::var_os(OMP_CONFIG_DIR_ENV_VAR)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| ".omp".into());
    Ok(home_dir()?.join(config_dir))
}

/// OMP's `normalizeProfileName` rule. OMP never activates a profile it would
/// reject (or `default`, which means the default profile), so herdr skips it.
fn is_omp_profile_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    let windows_reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit());
    matches!(bytes.first(), Some(b'a'..=b'z' | b'0'..=b'9'))
        && bytes.len() <= 64
        && bytes
            .iter()
            .all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
        && !name.ends_with('.')
        && name != "default"
        && !windows_reserved
}

pub(crate) fn claude_dir() -> io::Result<PathBuf> {
    config_dir_from_env_or_home(CLAUDE_CONFIG_DIR_ENV_VAR, &[".claude"])
}

pub(crate) fn codex_dir() -> io::Result<PathBuf> {
    config_dir_from_env_or_home(CODEX_HOME_ENV_VAR, &[".codex"])
}

pub(crate) fn kimi_dir() -> io::Result<PathBuf> {
    config_dir_from_env_or_home(KIMI_CODE_HOME_ENV_VAR, &[".kimi-code"])
}

pub(crate) fn copilot_dir() -> io::Result<PathBuf> {
    config_dir_from_env_or_home(COPILOT_HOME_ENV_VAR, &[".copilot"])
}

pub(crate) fn devin_dir() -> io::Result<PathBuf> {
    if let Some(value) = std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        return expand_tilde_path(PathBuf::from(value)).map(|path| path.join("devin"));
    }

    #[cfg(windows)]
    if let Some(value) = std::env::var_os("APPDATA").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(value).join("devin"));
    }

    Ok(home_dir()?.join(".config").join("devin"))
}

pub(crate) fn droid_dir() -> io::Result<PathBuf> {
    Ok(home_dir()?.join(".factory"))
}

pub(crate) fn config_dir_from_env_or_home(
    env_var: &str,
    home_relative_segments: &[&str],
) -> io::Result<PathBuf> {
    if let Some(value) = std::env::var_os(env_var).filter(|value| !value.is_empty()) {
        return expand_tilde_path(PathBuf::from(value));
    }

    let mut path = home_dir()?;
    for segment in home_relative_segments {
        path.push(segment);
    }
    Ok(path)
}

pub(crate) fn expand_tilde_path(path: PathBuf) -> io::Result<PathBuf> {
    let Some(raw) = path.to_str() else {
        return Ok(path);
    };

    if raw == "~" {
        return home_dir();
    }

    if let Some(rest) = raw
        .strip_prefix("~/")
        .or_else(|| raw.strip_prefix("~\\"))
        .or_else(|| raw.strip_prefix('~'))
    {
        return Ok(home_dir()?.join(rest));
    }

    Ok(path)
}

pub(crate) fn opencode_dir() -> io::Result<PathBuf> {
    Ok(home_dir()?.join(".config/opencode"))
}

pub(crate) fn opencode_state_dir() -> io::Result<PathBuf> {
    if let Some(value) = std::env::var_os("XDG_STATE_HOME").filter(|value| !value.is_empty()) {
        return expand_tilde_path(PathBuf::from(value)).map(|path| path.join("opencode"));
    }

    Ok(home_dir()?.join(".local/state/opencode"))
}

pub(crate) fn kilo_dir() -> io::Result<PathBuf> {
    Ok(home_dir()?.join(".config/kilo"))
}

pub(crate) fn hermes_dir() -> io::Result<PathBuf> {
    if let Some(value) = std::env::var_os(HERMES_HOME_ENV_VAR).filter(|value| !value.is_empty()) {
        return expand_tilde_path(PathBuf::from(value));
    }

    #[cfg(windows)]
    {
        let explicit_home = std::env::var_os("HOME").filter(|value| !value.is_empty());
        let profile = std::env::var_os("USERPROFILE").filter(|value| !value.is_empty());
        if let Some(home) = explicit_home.filter(|home| profile.as_ref() != Some(home)) {
            return Ok(PathBuf::from(home).join(".hermes"));
        }
        if let Some(local_app_data) =
            std::env::var_os("LOCALAPPDATA").filter(|value| !value.is_empty())
        {
            return Ok(PathBuf::from(local_app_data).join("hermes"));
        }
    }

    Ok(home_dir()?.join(".hermes"))
}

pub(crate) fn hermes_plugin_dir() -> io::Result<PathBuf> {
    Ok(hermes_dir()?
        .join("plugins")
        .join(super::HERMES_PLUGIN_INSTALL_NAME))
}

pub(crate) fn qodercli_dir() -> io::Result<PathBuf> {
    config_dir_from_env_or_home(QODERCLI_CONFIG_DIR_ENV_VAR, &[".qoder"])
}

pub(crate) fn qwen_dir() -> io::Result<PathBuf> {
    config_dir_from_env_or_home(QWEN_HOME_ENV_VAR, &[".qwen"])
}

pub(crate) fn letta_dir() -> io::Result<PathBuf> {
    Ok(home_dir()?.join(".letta"))
}

pub(crate) fn cursor_dir() -> io::Result<PathBuf> {
    config_dir_from_env_or_home(CURSOR_CONFIG_DIR_ENV_VAR, &[".cursor"])
}

pub(crate) fn mastracode_dir() -> io::Result<PathBuf> {
    Ok(home_dir()?.join(".mastracode"))
}

pub(crate) fn antigravity_cli_dir() -> io::Result<PathBuf> {
    // Antigravity CLI discovers global customizations (hooks.json included)
    // from ~/.gemini/config; ~/.gemini/antigravity-cli holds runtime data and
    // is never read for hooks.
    config_dir_from_env_or_home(ANTIGRAVITY_CLI_CONFIG_DIR_ENV_VAR, &[".gemini", "config"])
}

pub(crate) fn grok_dir() -> io::Result<PathBuf> {
    // GROK_CONFIG_DIR is a herdr-level override only (primarily a test
    // seam); the grok CLI does not honor it, so it stays first and explicit.
    if let Some(value) = std::env::var_os(GROK_CONFIG_DIR_ENV_VAR).filter(|value| !value.is_empty())
    {
        return expand_tilde_path(PathBuf::from(value));
    }
    // The grok CLI honors GROK_HOME as its config home (config.toml,
    // auth.json, hooks/); mirror it so hook installs land where grok looks.
    config_dir_from_env_or_home(GROK_HOME_ENV_VAR, &[".grok"])
}

pub(crate) fn home_dir() -> io::Result<PathBuf> {
    if let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(home));
    }

    #[cfg(windows)]
    {
        if let Some(profile) = std::env::var_os("USERPROFILE").filter(|value| !value.is_empty()) {
            return Ok(PathBuf::from(profile));
        }
        if let (Some(drive), Some(path)) = (
            std::env::var_os("HOMEDRIVE").filter(|value| !value.is_empty()),
            std::env::var_os("HOMEPATH").filter(|value| !value.is_empty()),
        ) {
            let mut home = PathBuf::from(drive);
            home.push(path);
            return Ok(home);
        }
    }

    Err(io::Error::other(
        "home directory is not set; cannot locate home directory",
    ))
}

#[cfg(test)]
pub(crate) struct IntegrationEnvLock {
    _guard: MutexGuard<'static, ()>,
    #[cfg(windows)]
    appdata: Option<std::ffi::OsString>,
}

#[cfg(test)]
impl Drop for IntegrationEnvLock {
    fn drop(&mut self) {
        #[cfg(windows)]
        if let Some(appdata) = self.appdata.take() {
            std::env::set_var("APPDATA", appdata);
        } else {
            std::env::remove_var("APPDATA");
        }
    }
}

#[cfg(test)]
pub(crate) fn integration_env_lock() -> IntegrationEnvLock {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let guard = LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
    IntegrationEnvLock {
        _guard: guard,
        #[cfg(windows)]
        appdata: std::env::var_os("APPDATA"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn omp_profile_names_follow_omp_rule() {
        for name in [
            "work",
            "0",
            "a.b_c-d",
            "a".repeat(64).as_str(),
            "console",
            "com10",
        ] {
            assert!(is_omp_profile_name(name), "{name} should be valid");
        }
        for name in [
            "",
            ".",
            "..",
            "work.",
            "-work",
            "Work",
            "default",
            "a".repeat(65).as_str(),
            "con",
            "nul.txt",
            "com1",
            "lpt9.x",
        ] {
            assert!(!is_omp_profile_name(name), "{name} should be invalid");
        }
    }

    #[test]
    fn opencode_state_dir_defaults_to_local_state() {
        let _lock = integration_env_lock();
        let original = std::env::var_os("XDG_STATE_HOME");
        std::env::remove_var("XDG_STATE_HOME");
        let expected = home_dir().unwrap().join(".local/state/opencode");
        assert_eq!(opencode_state_dir().unwrap(), expected);
        match original {
            Some(value) => std::env::set_var("XDG_STATE_HOME", value),
            None => std::env::remove_var("XDG_STATE_HOME"),
        }
    }

    #[test]
    fn opencode_state_dir_honors_xdg_state_home() {
        let _lock = integration_env_lock();
        let original = std::env::var_os("XDG_STATE_HOME");
        let xdg = std::env::temp_dir().join("herdr-xdg-state");
        std::env::set_var("XDG_STATE_HOME", &xdg);
        assert_eq!(opencode_state_dir().unwrap(), xdg.join("opencode"));
        match original {
            Some(value) => std::env::set_var("XDG_STATE_HOME", value),
            None => std::env::remove_var("XDG_STATE_HOME"),
        }
    }
}
