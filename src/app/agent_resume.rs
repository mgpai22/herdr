use std::time::Instant;

use bytes::Bytes;
use ratatui::layout::Rect;

use super::App;

struct PendingAgentResumeCandidate {
    pane_id: crate::layout::PaneId,
    terminal_id: crate::terminal::TerminalId,
    cwd: std::path::PathBuf,
    plan: crate::agent_resume::AgentResumePlan,
    rows: u16,
    cols: u16,
}

impl App {
    pub(crate) fn has_pending_agent_resumes(&self) -> bool {
        self.state
            .terminals
            .values()
            .any(|terminal| terminal.pending_agent_resume_plan.is_some())
    }

    pub(crate) fn sync_pending_agent_resume_deadline(&mut self, now: Instant) {
        if !self.has_pending_agent_resumes() {
            self.pending_agent_resume_deadline = None;
            self.next_agent_resume_at = None;
            return;
        }
        if self.pending_agent_resume_candidates().is_empty() {
            self.pending_agent_resume_deadline = None;
            return;
        }
        if let Some(next) = self.next_agent_resume_at {
            self.pending_agent_resume_deadline = Some(next);
        } else {
            self.pending_agent_resume_deadline
                .get_or_insert(now + super::PENDING_AGENT_RESUME_THEME_WAIT);
        }
    }

    pub(crate) fn pending_agent_resume_due(&self, now: Instant) -> bool {
        self.pending_agent_resume_deadline
            .is_some_and(|deadline| now >= deadline)
    }

    pub(crate) fn start_pending_agent_resumes(
        &mut self,
        now: Instant,
        allow_empty_theme: bool,
    ) -> bool {
        // Geometry/theme events can also enter here; they must not bypass spacing.
        if self.next_agent_resume_at.is_some_and(|next| now < next) {
            return false;
        }
        let pending = self.pending_agent_resume_candidates();
        let mut changed = false;
        for PendingAgentResumeCandidate {
            pane_id,
            terminal_id,
            cwd,
            plan,
            rows,
            cols,
        } in pending
        {
            if self.terminal_runtimes.get(&terminal_id).is_some() {
                continue;
            }
            changed |= self.start_pending_agent_resume(
                pane_id,
                terminal_id,
                cwd,
                plan,
                rows,
                cols,
                allow_empty_theme,
            );
            if changed && !self.startup_per_agent_delay.is_zero() {
                self.next_agent_resume_at = Some(now + self.startup_per_agent_delay);
                self.pending_agent_resume_deadline = self.next_agent_resume_at;
                break;
            }
        }

        if changed {
            self.schedule_session_save();
        }
        if !self.has_pending_agent_resumes() || self.pending_agent_resume_candidates().is_empty() {
            self.pending_agent_resume_deadline = None;
        }
        if !self.has_pending_agent_resumes() {
            self.next_agent_resume_at = None;
        }
        changed
    }

    fn pending_agent_resume_candidates(&self) -> Vec<PendingAgentResumeCandidate> {
        let terminal_area = self.state.view.terminal_area;
        if terminal_area.width == 0 || terminal_area.height == 0 {
            return Vec::new();
        };

        let mut pending = Vec::new();
        for (ws_idx, ws) in self.state.workspaces.iter().enumerate() {
            for (tab_idx, tab) in ws.tabs.iter().enumerate() {
                for info in
                    self.pending_agent_resume_pane_infos(ws_idx, tab_idx, tab, terminal_area)
                {
                    let Some(pane) = tab.panes.get(&info.id) else {
                        continue;
                    };
                    if self
                        .terminal_runtimes
                        .get(&pane.attached_terminal_id)
                        .is_some()
                    {
                        continue;
                    }
                    let Some(terminal) = self.state.terminals.get(&pane.attached_terminal_id)
                    else {
                        continue;
                    };
                    let Some(plan) = terminal.pending_agent_resume_plan.clone() else {
                        continue;
                    };
                    pending.push(PendingAgentResumeCandidate {
                        pane_id: info.id,
                        terminal_id: pane.attached_terminal_id.clone(),
                        cwd: terminal.cwd.clone(),
                        plan,
                        rows: info.inner_rect.height,
                        cols: info.inner_rect.width,
                    });
                }
            }
        }
        pending
    }

    fn pending_agent_resume_pane_infos(
        &self,
        ws_idx: usize,
        tab_idx: usize,
        tab: &crate::workspace::Tab,
        terminal_area: Rect,
    ) -> Vec<crate::layout::PaneInfo> {
        let mut pane_infos = derived_pending_agent_resume_pane_infos(
            tab,
            terminal_area,
            self.state.pane_borders,
            self.state.pane_gaps,
            self.state.pane_outer_borders,
        );

        if self.state.active == Some(ws_idx)
            && self
                .state
                .workspaces
                .get(ws_idx)
                .is_some_and(|ws| tab_idx == ws.active_tab_index())
        {
            for visible_info in &self.state.view.pane_infos {
                if let Some(info) = pane_infos
                    .iter_mut()
                    .find(|info| info.id == visible_info.id)
                {
                    *info = visible_info.clone();
                } else {
                    pane_infos.push(visible_info.clone());
                }
            }
        }

        pane_infos
    }

    pub(crate) fn start_pending_agent_resume_for_terminal(
        &mut self,
        terminal_id: &crate::terminal::TerminalId,
        rows: u16,
        cols: u16,
        allow_empty_theme: bool,
    ) -> bool {
        if self.terminal_runtimes.get(terminal_id).is_some() {
            return false;
        }
        let Some((pane_id, cwd, plan)) = self.state.workspaces.iter().find_map(|ws| {
            ws.tabs.iter().find_map(|tab| {
                tab.layout.pane_ids().into_iter().find_map(|pane_id| {
                    let pane = tab.panes.get(&pane_id)?;
                    if &pane.attached_terminal_id != terminal_id {
                        return None;
                    }
                    let terminal = self.state.terminals.get(terminal_id)?;
                    Some((
                        pane_id,
                        terminal.cwd.clone(),
                        terminal.pending_agent_resume_plan.clone()?,
                    ))
                })
            })
        }) else {
            return false;
        };

        let changed = self.start_pending_agent_resume(
            pane_id,
            terminal_id.clone(),
            cwd,
            plan,
            rows,
            cols,
            allow_empty_theme,
        );
        if changed {
            self.schedule_session_save();
        }
        if !self.has_pending_agent_resumes() {
            self.pending_agent_resume_deadline = None;
        }
        changed
    }
    /// Re-checks a pending OMP resume against the current launchers and returns the saved
    /// profile, which the resume command exports as `OMP_PROFILE`.
    fn refresh_omp_resume_executable(
        plan: &mut crate::agent_resume::AgentResumePlan,
        session: Option<&crate::agent_resume::PersistedAgentSession>,
        launchers: &std::collections::BTreeMap<String, String>,
    ) -> Result<Option<String>, String> {
        if plan.agent != "omp" {
            return Ok(None);
        }
        let session = session.ok_or("saved OMP recovery metadata is missing")?;
        let current_plan =
            crate::agent_resume::plan(&session.source, &session.agent, &session.session_ref)
                .ok_or("saved OMP session cannot resume")?;
        if current_plan.dedupe_key != plan.dedupe_key {
            return Err("pending OMP resume no longer matches the saved session".into());
        }
        let executable = crate::agent_resume::omp_recovery_executable(session, launchers)
            .map_err(|block| block.to_string())?;
        let command = plan
            .argv
            .first_mut()
            .ok_or("saved OMP resume command is empty")?;
        *command = executable;
        if let Some(fresh) = crate::agent_resume::omp_fresh_session_args(&session.session_ref) {
            // The saved file cannot be opened (OMP 18.7 stops at `--resume=<it>`): start a new
            // session in its directory, and never a bare launcher (see `omp_fresh_session_args`).
            plan.argv.truncate(1);
            plan.argv.extend(fresh);
        }
        Ok(session.launch_profile.clone())
    }

    fn start_pending_agent_resume(
        &mut self,
        pane_id: crate::layout::PaneId,
        terminal_id: crate::terminal::TerminalId,
        cwd: std::path::PathBuf,
        mut plan: crate::agent_resume::AgentResumePlan,
        rows: u16,
        cols: u16,
        allow_empty_theme: bool,
    ) -> bool {
        let recovery_session = self
            .state
            .terminals
            .get(&terminal_id)
            .and_then(|terminal| terminal.persisted_agent_session.as_ref());
        let omp_profile = match Self::refresh_omp_resume_executable(
            &mut plan,
            recovery_session,
            &self.omp_launchers,
        ) {
            Ok(omp_profile) => omp_profile,
            Err(reason) => {
                tracing::warn!(pane = pane_id.raw(), terminal = %terminal_id, reason = %reason,
                    "automatic OMP recovery blocked before spawn");
                return self.block_pending_omp_resume(
                    pane_id,
                    &terminal_id,
                    cwd,
                    rows,
                    cols,
                    reason,
                );
            }
        };
        let host_terminal_theme = self.state.host_terminal_theme;
        if host_terminal_theme.is_empty() && !allow_empty_theme {
            return false;
        }

        // Windows panes run PowerShell or cmd, where POSIX quoting breaks launcher paths and
        // quoted arguments; launch exactly like `agent start` does.
        #[cfg(windows)]
        let shell_name = if crate::pane::uses_windows_powershell_pane_shell(
            crate::pane::PaneShellConfig::new(&self.state.default_shell, self.state.shell_mode),
        ) {
            "powershell"
        } else {
            "cmd"
        };
        let resume_command = match omp_profile.as_deref() {
            Some(profile) => {
                // The profile prefix depends on the shell that will read the command.
                #[cfg(not(windows))]
                let shell_name = crate::pane::pane_shell(&self.state.default_shell);
                #[cfg(not(windows))]
                let shell_name = shell_name.as_str();
                let Some(command) = crate::platform::interactive_shell_command_with_env(
                    &plan.argv,
                    crate::agent_resume::OMP_PROFILE_ENV,
                    profile,
                    shell_name,
                ) else {
                    let reason = format!(
                        "the launcher for profile {profile:?} cannot be typed with its profile into this pane shell; use a launcher path without \"=\""
                    );
                    return self.block_pending_omp_resume(
                        pane_id,
                        &terminal_id,
                        cwd,
                        rows,
                        cols,
                        reason,
                    );
                };
                Some(command)
            }
            #[cfg(not(windows))]
            None => shell_command_from_argv(&plan.argv),
            #[cfg(windows)]
            None => crate::platform::interactive_shell_command(&plan.argv, shell_name),
        };
        let Some(resume_command) = resume_command else {
            tracing::warn!(
                pane = pane_id.raw(),
                terminal = %terminal_id,
                agent = %plan.agent,
                "failed to start deferred agent resume with empty argv"
            );
            return false;
        };
        let Some(launch_env) = self
            .find_pane(pane_id)
            .and_then(|(ws_idx, _)| self.pane_launch_env(ws_idx, pane_id, Vec::new()))
        else {
            return false;
        };

        if !cwd.is_dir() {
            if let Some(terminal) = self.state.terminals.get_mut(&terminal_id) {
                terminal.pending_agent_resume_plan = None;
                terminal.restore_error = Some("Saved directory is unavailable. Restore the directory and restart this session.".into());
                terminal.revision = terminal.revision.saturating_add(1);
            }
            return true;
        }

        let mut cleared_omp_owner = None;
        if plan.agent == "omp" {
            match self.persist_owner_unknown_before_resume(&terminal_id) {
                Ok(owner) => cleared_omp_owner = Some(owner),
                Err(err) => {
                    tracing::warn!(
                        pane = pane_id.raw(),
                        terminal = %terminal_id,
                        err = %err,
                        "automatic OMP recovery blocked because the recovery barrier could not be saved"
                    );
                    return self.block_pending_omp_resume(
                        pane_id,
                        &terminal_id,
                        cwd,
                        rows,
                        cols,
                        format!("Herdr could not save the OMP recovery barrier ({err})"),
                    );
                }
            }
        }

        let runtime = match crate::terminal::TerminalRuntime::spawn(
            pane_id,
            rows,
            cols,
            cwd,
            self.state.pane_scrollback_limit_bytes,
            host_terminal_theme,
            self.state.host_terminal_appearance,
            crate::pane::PaneShellConfig::new(&self.state.default_shell, self.state.shell_mode),
            &launch_env,
            self.event_tx.clone(),
            self.render_notify.clone(),
            self.render_dirty.clone(),
        ) {
            Ok(runtime) => runtime,
            Err(err) => {
                tracing::warn!(
                    pane = pane_id.raw(),
                    terminal = %terminal_id,
                    agent = %plan.agent,
                    err = %err,
                    "failed to start shell for deferred agent resume"
                );
                if let Some(owner) = cleared_omp_owner {
                    self.restore_omp_owner_after_failed_resume(&terminal_id, owner);
                }
                if let Some(terminal) = self.state.terminals.get_mut(&terminal_id) {
                    terminal.pending_agent_resume_plan = None;
                    terminal.restore_error = Some(format!("Could not start the saved shell: {err}. Fix the shell configuration and restart this session."));
                    terminal.revision = terminal.revision.saturating_add(1);
                }
                return true;
            }
        };

        let mut input = resume_command;
        input.push('\r');
        if let Err(err) = runtime.try_send_bytes(Bytes::from(input)) {
            tracing::warn!(
                pane = pane_id.raw(),
                terminal = %terminal_id,
                agent = %plan.agent,
                err = %err,
                "failed to send deferred agent resume command to shell"
            );
            runtime.shutdown();
            // The retry re-runs the barrier; it needs the dead owner to pass revalidation.
            if let Some(owner) = cleared_omp_owner {
                self.restore_omp_owner_after_failed_resume(&terminal_id, owner);
            }
            return false;
        }

        self.terminal_runtimes.insert(terminal_id.clone(), runtime);
        if let Some(terminal) = self.state.terminals.get_mut(&terminal_id) {
            terminal.pending_agent_resume_plan = None;
            terminal.respawn_shell_on_exit = false;
        }
        true
    }

    /// Drops a pending OMP resume that must not run and gives the pane a plain shell that shows
    /// why, like a pane whose recovery was blocked at restore time. Without a usable directory or
    /// shell the pane shows only the notice.
    fn block_pending_omp_resume(
        &mut self,
        pane_id: crate::layout::PaneId,
        terminal_id: &crate::terminal::TerminalId,
        cwd: std::path::PathBuf,
        rows: u16,
        cols: u16,
        reason: impl std::fmt::Display,
    ) -> bool {
        let notice = crate::agent_resume::omp_recovery_blocked_notice(reason);
        let Some(terminal) = self.state.terminals.get_mut(terminal_id) else {
            return false;
        };
        terminal.pending_agent_resume_plan = None;
        terminal.restore_error = Some(notice.clone());
        terminal.revision = terminal.revision.saturating_add(1);
        let launch_env = self
            .find_pane(pane_id)
            .and_then(|(ws_idx, _)| self.pane_launch_env(ws_idx, pane_id, Vec::new()));
        let (Some(launch_env), true) = (launch_env, cwd.is_dir()) else {
            return true;
        };
        let history = crate::agent_resume::screen_history_with_omp_recovery_notice(None, &notice);
        match crate::terminal::TerminalRuntime::spawn_with_initial_history(
            pane_id,
            rows,
            cols,
            cwd,
            self.state.pane_scrollback_limit_bytes,
            self.state.host_terminal_theme,
            self.state.host_terminal_appearance,
            crate::pane::PaneShellConfig::new(&self.state.default_shell, self.state.shell_mode),
            &launch_env,
            Some(&history),
            self.event_tx.clone(),
            self.render_notify.clone(),
            self.render_dirty.clone(),
        ) {
            Ok(runtime) => {
                self.terminal_runtimes.insert(terminal_id.clone(), runtime);
            }
            Err(err) => tracing::warn!(
                pane = pane_id.raw(),
                terminal = %terminal_id,
                err = %err,
                "failed to start a shell for a pane with blocked OMP recovery"
            ),
        }
        true
    }
}

fn derived_pending_agent_resume_pane_infos(
    tab: &crate::workspace::Tab,
    terminal_area: Rect,
    pane_borders: crate::config::PaneBordersConfig,
    pane_gaps: bool,
    pane_outer_borders: bool,
) -> Vec<crate::layout::PaneInfo> {
    crate::ui::apply_pane_chrome(
        tab.layout.panes(terminal_area),
        pane_borders,
        pane_gaps,
        pane_outer_borders,
    )
    .into_iter()
    .map(|mut info| {
        let pane_inner = crate::ui::pane_inner_rect(info.rect, info.borders);
        info.inner_rect = stable_terminal_inner_rect(pane_inner);
        info
    })
    .collect()
}

fn stable_terminal_inner_rect(pane_inner: Rect) -> Rect {
    if pane_inner.width <= 4 {
        return pane_inner;
    }

    Rect::new(
        pane_inner.x,
        pane_inner.y,
        pane_inner.width.saturating_sub(1),
        pane_inner.height,
    )
}

#[cfg(not(windows))]
fn shell_command_from_argv(argv: &[String]) -> Option<String> {
    let mut parts = argv.iter();
    let first = shell_quote(parts.next()?);
    let mut command = first;
    for part in parts {
        command.push(' ');
        command.push_str(&shell_quote(part));
    }
    Some(command)
}

#[cfg(not(windows))]
fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    if value.bytes().all(|byte| {
        byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'_' | b'-' | b'.' | b'/' | b':' | b'@' | b'%' | b'+' | b'='
            )
    }) {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn test_app() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        )
    }

    #[tokio::test]
    async fn pending_agent_resume_spacing_survives_events_and_failed_restores() {
        for delay_ms in [100, 250, 0] {
            let config: crate::config::Config = toml::from_str(&format!(
                "[session]\nstartup_per_agent_delay_ms = {delay_ms}"
            ))
            .unwrap();
            let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
            let mut app = App::new(
                &config,
                crate::app::AppPolicy::TEST,
                None,
                api_rx,
                crate::api::EventHub::default(),
            );
            app.state.workspaces = (0..4)
                .map(|_| crate::workspace::Workspace::test_new("restore"))
                .collect();
            app.state.active = Some(0);
            app.state.view.terminal_area = Rect::new(0, 0, 100, 30);
            app.state.ensure_test_terminals();
            let missing = std::env::current_dir()
                .unwrap()
                .join("__missing_resume_cwd__");
            assert!(!missing.exists());
            for terminal in app.state.terminals.values_mut() {
                terminal.cwd = missing.clone();
                terminal.pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
                    agent: "codex".into(),
                    argv: vec!["codex".into()],
                    dedupe_key: terminal.id.to_string(),
                });
            }
            let now = Instant::now();
            app.sync_pending_agent_resume_deadline(now);
            assert!(!app.start_pending_agent_resumes(now, false));
            assert!(app.start_pending_agent_resumes(now, true));
            if delay_ms != 0 {
                let next = now + std::time::Duration::from_millis(delay_ms);
                assert_eq!(
                    app.state
                        .terminals
                        .values()
                        .filter(|t| t.restore_error.is_some())
                        .count(),
                    1
                );
                // Geometry changes clear the wakeup, but must preserve the launch gap.
                app.pending_agent_resume_deadline = None;
                app.sync_pending_agent_resume_deadline(now);
                assert_eq!(app.pending_agent_resume_deadline, Some(next));
                assert!(!app
                    .start_pending_agent_resumes(next - std::time::Duration::from_millis(1), true));
                // A late wakeup must not release every overdue agent in a burst.
                for processed in 2..=4 {
                    let late = now + std::time::Duration::from_secs(processed * 10);
                    assert!(app.start_pending_agent_resumes(late, true));
                    assert_eq!(
                        app.state
                            .terminals
                            .values()
                            .filter(|t| t.restore_error.is_some())
                            .count(),
                        processed as usize
                    );
                }
            }
            assert!(!app.has_pending_agent_resumes());
            assert!(app.pending_agent_resume_deadline.is_none());
            assert!(app.next_agent_resume_at.is_none());
            assert_eq!(
                app.state
                    .terminals
                    .values()
                    .filter(|t| t.restore_error.is_some())
                    .count(),
                4
            );
        }
    }

    #[cfg(unix)]
    fn long_running_test_argv() -> Vec<String> {
        vec!["/bin/sh".into(), "-c".into(), "sleep 5".into()]
    }

    #[cfg(unix)]
    fn marker_resume_test_argv() -> Vec<String> {
        vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf '%s' 'restored agent: shell quoted | marker'; sleep 5".into(),
        ]
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pending_omp_resume_revalidates_mapping_and_session_identity() {
        let first_ref = crate::agent_resume::AgentSessionRef::path("/tmp/first.jsonl").unwrap();
        let mut plan = crate::agent_resume::plan("herdr:omp", "omp", &first_ref).unwrap();
        let session = crate::agent_resume::PersistedAgentSession {
            source: "herdr:omp".into(),
            agent: "omp".into(),
            session_ref: first_ref,
            launch_profile: Some("default".into()),
            owner_process: Some(crate::platform::OwnerProcessIncarnation {
                pid: u32::MAX,
                boot_id: "old-boot".into(),
                start_time_ticks: 1,
            }),
        };
        let launchers =
            std::collections::BTreeMap::from([("default".into(), "/opt/new-wrapper".into())]);
        App::refresh_omp_resume_executable(&mut plan, Some(&session), &launchers).unwrap();
        assert_eq!(plan.argv[0], "/opt/new-wrapper");

        assert!(App::refresh_omp_resume_executable(
            &mut plan,
            Some(&session),
            &std::collections::BTreeMap::new(),
        )
        .is_err());
        let mut different = session.clone();
        different.session_ref =
            crate::agent_resume::AgentSessionRef::path("/tmp/second.jsonl").unwrap();
        assert!(
            App::refresh_omp_resume_executable(&mut plan, Some(&different), &launchers)
                .unwrap_err()
                .contains("no longer matches")
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pending_omp_resume_never_restarts_a_session_it_cannot_open_as_a_bare_launcher() {
        let dir = std::env::temp_dir().join(format!(
            "herdr-omp-resume-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let written = dir.join("written.jsonl");
        std::fs::write(&written, "{}\n").unwrap();
        let never_written = dir.join("never-written.jsonl");
        let empty = dir.join("empty.jsonl");
        std::fs::write(&empty, "").unwrap();
        let launchers =
            std::collections::BTreeMap::from([("default".into(), "/opt/omp-wrapper".into())]);
        let launcher = || "/opt/omp-wrapper".to_string();
        let in_dir = || {
            vec![
                launcher(),
                "--session-dir".to_string(),
                dir.to_str().unwrap().to_string(),
            ]
        };
        let path = |file: &std::path::Path| {
            crate::agent_resume::AgentSessionRef::path(file.to_str().unwrap()).unwrap()
        };
        let id = crate::agent_resume::AgentSessionRef::id("01a113e4-bc1f-77e4-b4ad-5f232f8a2a5f")
            .unwrap();
        for (session_ref, argv) in [
            (
                path(&written),
                vec![launcher(), format!("--resume={}", written.display())],
            ),
            (path(&never_written), in_dir()),
            (path(&empty), in_dir()),
            (id, vec![launcher(), "--no-session".to_string()]),
        ] {
            let mut plan = crate::agent_resume::plan("herdr:omp", "omp", &session_ref).unwrap();
            let session = crate::agent_resume::PersistedAgentSession {
                source: "herdr:omp".into(),
                agent: "omp".into(),
                session_ref,
                launch_profile: Some("default".into()),
                owner_process: Some(crate::platform::OwnerProcessIncarnation {
                    pid: u32::MAX,
                    boot_id: "old-boot".into(),
                    start_time_ticks: 1,
                }),
            };
            App::refresh_omp_resume_executable(&mut plan, Some(&session), &launchers).unwrap();
            assert_eq!(plan.argv, argv, "{:?}", session.session_ref);
            assert!(
                plan.argv.len() > 1,
                "a bare launcher can pick another pane's session"
            );
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_deferred_restore_keeps_session_reference_without_retrying_elsewhere() {
        for missing_shell in [false, true] {
            let mut app = test_app();
            let workspace = crate::workspace::Workspace::test_new("unavailable");
            let pane_id = workspace.tabs[0].root_pane;
            let terminal_id = workspace.terminal_id(pane_id).unwrap().clone();
            app.state.workspaces = vec![workspace];
            app.state.active = Some(0);
            app.state.ensure_test_terminals();
            if missing_shell {
                app.state.default_shell = "__herdr_missing_resume_shell__".into();
            }
            let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
            if !missing_shell {
                terminal.cwd = std::env::current_dir()
                    .unwrap()
                    .join("__herdr_missing_resume_cwd__");
                assert!(!terminal.cwd.exists());
            }
            let session = crate::agent_resume::PersistedAgentSession {
                source: "herdr:codex".into(),
                agent: "codex".into(),
                session_ref: crate::agent_resume::AgentSessionRef::id("resume-test").unwrap(),
                launch_profile: None,
                owner_process: None,
            };
            terminal.persisted_agent_session = Some(session.clone());
            terminal.pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
                agent: "codex".into(),
                argv: long_running_test_argv(),
                dedupe_key: "resume-test".into(),
            });
            app.start_pending_agent_resume_for_terminal(&terminal_id, 24, 80, true);
            assert!(app.terminal_runtimes.get(&terminal_id).is_none());
            let terminal = &app.state.terminals[&terminal_id];
            assert!(terminal.pending_agent_resume_plan.is_none());
            assert_eq!(terminal.persisted_agent_session.as_ref(), Some(&session));
            assert!(terminal.restore_error.is_some());
            assert!(!app.has_pending_agent_resumes());
            assert!(!app.start_pending_agent_resume_for_terminal(&terminal_id, 24, 80, true));
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pending_agent_resume_waits_for_host_theme_before_launch() {
        let mut app = test_app();
        let workspace = crate::workspace::Workspace::test_new("restored");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace.terminal_id(pane_id).cloned().unwrap();
        let pane_infos = workspace.tabs[0]
            .layout
            .panes(ratatui::layout::Rect::new(0, 0, 100, 30));
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 30);
        app.state.view.pane_infos = pane_infos;
        let terminal = app
            .state
            .terminals
            .get_mut(&terminal_id)
            .expect("test terminal should exist");
        terminal.pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "codex".into(),
            argv: marker_resume_test_argv(),
            dedupe_key: "herdr:codex\0codex\0Id\0codex-session".into(),
        });

        assert!(!app.start_pending_agent_resumes(Instant::now(), false));
        assert!(app.terminal_runtimes.get(&terminal_id).is_none());

        app.state.host_terminal_theme = crate::terminal_theme::TerminalTheme {
            foreground: Some(crate::terminal_theme::RgbColor {
                r: 220,
                g: 220,
                b: 220,
            }),
            background: Some(crate::terminal_theme::RgbColor {
                r: 20,
                g: 20,
                b: 20,
            }),
            ..Default::default()
        };

        assert!(app.start_pending_agent_resumes(Instant::now(), false));
        assert!(app.terminal_runtimes.get(&terminal_id).is_some());
        let terminal = app
            .state
            .terminals
            .get(&terminal_id)
            .expect("terminal should survive launch");
        assert!(terminal.pending_agent_resume_plan.is_none());
        assert!(!terminal.respawn_shell_on_exit);

        let runtime = app
            .terminal_runtimes
            .get(&terminal_id)
            .expect("pending resume should leave a shell runtime");
        let marker = "restored agent: shell quoted | marker";
        for _ in 0..20 {
            if runtime
                .snapshot_history()
                .is_some_and(|text| text.contains(marker))
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(
            runtime
                .snapshot_history()
                .expect("runtime should expose terminal history")
                .contains(marker),
            "deferred restore should inject the resume argv into the restored shell"
        );

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }
    // Owner observation exists only on Linux (and Windows); other platforms block before the barrier.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn omp_resume_restores_owner_when_no_omp_process_starts() {
        let _guard = crate::config::test_config_env_lock().lock().unwrap();
        let config_home = std::env::temp_dir().join(format!(
            "herdr-omp-owner-barrier-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::env::remove_var(crate::session::SESSION_ENV_VAR);
        let owner = crate::platform::OwnerProcessIncarnation {
            pid: 42,
            boot_id: "old-boot".into(),
            start_time_ticks: 1,
        };
        // (persist_session, config home is a regular file, missing shell)
        for (persist, unwritable, missing_shell) in [
            (false, false, false),
            (true, true, false),
            (true, false, true),
        ] {
            let _ = std::fs::remove_dir_all(&config_home);
            let _ = std::fs::remove_file(&config_home);
            if unwritable {
                std::fs::write(&config_home, b"not a directory").unwrap();
            } else {
                std::fs::create_dir_all(&config_home).unwrap();
            }
            std::env::set_var("XDG_CONFIG_HOME", &config_home);
            let mut app = test_app();
            app.policy.persist_session = persist;
            if missing_shell {
                app.state.default_shell = "__herdr_missing_resume_shell__".into();
            }
            let workspace = crate::workspace::Workspace::test_new("restored");
            let pane_id = workspace.tabs[0].root_pane;
            let terminal_id = workspace.terminal_id(pane_id).cloned().unwrap();
            app.state.workspaces = vec![workspace];
            app.state.active = Some(0);
            app.state.ensure_test_terminals();
            app.state.host_terminal_theme = crate::terminal_theme::TerminalTheme {
                foreground: Some(crate::terminal_theme::RgbColor {
                    r: 220,
                    g: 220,
                    b: 220,
                }),
                background: Some(crate::terminal_theme::RgbColor {
                    r: 20,
                    g: 20,
                    b: 20,
                }),
                ..Default::default()
            };
            app.omp_launchers
                .insert("default".into(), "/bin/true".into());
            let session_ref =
                crate::agent_resume::AgentSessionRef::path("/tmp/omp-session.jsonl").unwrap();
            let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
            terminal.pending_agent_resume_plan =
                crate::agent_resume::plan("herdr:omp", "omp", &session_ref);
            terminal.persisted_agent_session = Some(crate::agent_resume::PersistedAgentSession {
                source: "herdr:omp".into(),
                agent: "omp".into(),
                session_ref,
                launch_profile: Some("default".into()),
                owner_process: Some(owner.clone()),
            });

            assert!(app.start_pending_agent_resume_for_terminal(&terminal_id, 24, 80, true));
            let terminal = &app.state.terminals[&terminal_id];
            assert!(terminal.pending_agent_resume_plan.is_none());
            let restore_error = terminal.restore_error.as_deref().unwrap();
            assert_eq!(
                restore_error.contains("recovery barrier"),
                !missing_shell,
                "{restore_error}"
            );
            // A failed barrier leaves a usable shell that shows why OMP did not start.
            assert_eq!(
                app.terminal_runtimes
                    .get(&terminal_id)
                    .map(|runtime| runtime.recent_unwrapped_text(20).contains(restore_error)),
                (!missing_shell).then_some(true),
                "{restore_error}"
            );
            assert_eq!(
                terminal
                    .persisted_agent_session
                    .as_ref()
                    .unwrap()
                    .owner_process
                    .as_ref(),
                Some(&owner)
            );
            for (_, runtime) in app.terminal_runtimes.drain() {
                runtime.shutdown();
            }
        }

        std::env::remove_var("XDG_CONFIG_HOME");
        let _ = std::fs::remove_dir_all(&config_home);
        let _ = std::fs::remove_file(&config_home);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn omp_resume_runs_the_launcher_under_its_saved_profile() {
        use std::os::unix::fs::PermissionsExt;

        let env_guard = crate::config::test_config_env_lock().lock().unwrap();
        let root = std::env::temp_dir().join(format!(
            "herdr-omp-resume-profile-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let config_home = root.join("config");
        std::fs::create_dir_all(&config_home).unwrap();
        std::env::remove_var(crate::session::SESSION_ENV_VAR);
        std::env::set_var("XDG_CONFIG_HOME", &config_home);
        // A launcher that does not select a profile itself, like a bare OMP binary.
        let launcher = root.join("omp-plain");
        std::fs::write(
            &launcher,
            "#!/bin/sh\nprintf 'profile[%s]\\n' \"$OMP_PROFILE\"\nsleep 5\n",
        )
        .unwrap();
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut app = test_app();
        app.policy.persist_session = true;
        let workspace = crate::workspace::Workspace::test_new("restored");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace.terminal_id(pane_id).cloned().unwrap();
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        app.omp_launchers = std::collections::BTreeMap::from([
            ("default".into(), "/bin/true".into()),
            ("neurable".into(), launcher.display().to_string()),
        ]);
        let session_ref =
            crate::agent_resume::AgentSessionRef::path("/tmp/omp-profile-session.jsonl").unwrap();
        let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
        terminal.pending_agent_resume_plan =
            crate::agent_resume::plan("herdr:omp", "omp", &session_ref);
        terminal.persisted_agent_session = Some(crate::agent_resume::PersistedAgentSession {
            source: "herdr:omp".into(),
            agent: "omp".into(),
            session_ref,
            launch_profile: Some("neurable".into()),
            owner_process: Some(crate::platform::OwnerProcessIncarnation {
                pid: 42,
                boot_id: "old-boot".into(),
                start_time_ticks: 1,
            }),
        });

        assert!(app.start_pending_agent_resume_for_terminal(&terminal_id, 24, 80, true));
        // The barrier save is done; release the shared config environment before waiting.
        std::env::remove_var("XDG_CONFIG_HOME");
        drop(env_guard);
        let runtime = app
            .terminal_runtimes
            .get(&terminal_id)
            .expect("the resume should start a shell");
        let mut history = String::new();
        for _ in 0..200 {
            history = runtime.snapshot_history().unwrap_or_default();
            if history.contains("profile[") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(history.contains("profile[neurable]"), "{history}");

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A launcher that behaves like OMP 18.7 with `autoResume: true` for the start arguments Herdr
    /// can give it, and says what it did. A bare start continues another pane's session, which
    /// a restored pane must never do.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn omp_restore_of_a_session_omp_cannot_open_never_takes_another_panes_session() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!(
            "herdr-omp-resume-auto-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let config_home = root.join("config");
        let sessions = root.join("sessions");
        std::fs::create_dir_all(&config_home).unwrap();
        std::fs::create_dir_all(&sessions).unwrap();
        let launcher = root.join("omp-auto-resume");
        std::fs::write(
            &launcher,
            "#!/bin/sh\n\
             case \"$1\" in\n\
             --resume=*) f=${1#--resume=}\n\
               if [ -s \"$f\" ]; then echo \"outcome[resumed $f]\"; else echo \"outcome[session not found]\"; fi ;;\n\
             --session-dir) echo \"outcome[new session in $2]\" ;;\n\
             --no-session) echo \"outcome[in memory]\" ;;\n\
             *) echo \"outcome[autoResume took another pane's session]\" ;;\n\
             esac\n\
             sleep 5\n",
        )
        .unwrap();
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o755)).unwrap();
        let written = sessions.join("written.jsonl");
        std::fs::write(&written, "{}\n").unwrap();
        let empty = sessions.join("empty.jsonl");
        std::fs::write(&empty, "").unwrap();
        let path = |file: &std::path::Path| {
            crate::agent_resume::AgentSessionRef::path(file.to_str().unwrap()).unwrap()
        };
        let new_in_sessions = format!("outcome[new session in {}]", sessions.display());
        let cases = [
            (
                path(&written),
                format!("outcome[resumed {}]", written.display()),
            ),
            (
                path(&sessions.join("never-written.jsonl")),
                new_in_sessions.clone(),
            ),
            (path(&empty), new_in_sessions),
            (
                crate::agent_resume::AgentSessionRef::id("01a113e4-bc1f-77e4-b4ad-5f232f8a2a5f")
                    .unwrap(),
                "outcome[in memory]".to_string(),
            ),
        ];

        for (session_ref, expected) in cases {
            let (mut app, terminal_id) = {
                // The barrier save reads the config home: hold the shared environment only
                // while the restore starts, never across an await.
                let _env = crate::config::test_config_env_lock().lock().unwrap();
                std::env::remove_var(crate::session::SESSION_ENV_VAR);
                std::env::set_var("XDG_CONFIG_HOME", &config_home);
                let mut app = test_app();
                app.policy.persist_session = true;
                let workspace = crate::workspace::Workspace::test_new("restored");
                let pane_id = workspace.tabs[0].root_pane;
                let terminal_id = workspace.terminal_id(pane_id).cloned().unwrap();
                app.state.workspaces = vec![workspace];
                app.state.active = Some(0);
                app.state.ensure_test_terminals();
                app.omp_launchers = std::collections::BTreeMap::from([(
                    "default".into(),
                    launcher.display().to_string(),
                )]);
                let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
                terminal.pending_agent_resume_plan =
                    crate::agent_resume::plan("herdr:omp", "omp", &session_ref);
                terminal.persisted_agent_session =
                    Some(crate::agent_resume::PersistedAgentSession {
                        source: "herdr:omp".into(),
                        agent: "omp".into(),
                        session_ref: session_ref.clone(),
                        launch_profile: Some("default".into()),
                        owner_process: Some(crate::platform::OwnerProcessIncarnation {
                            pid: u32::MAX,
                            boot_id: "old-boot".into(),
                            start_time_ticks: 1,
                        }),
                    });
                assert!(app.start_pending_agent_resume_for_terminal(&terminal_id, 24, 400, true));
                std::env::remove_var("XDG_CONFIG_HOME");
                (app, terminal_id)
            };
            let runtime = app
                .terminal_runtimes
                .get(&terminal_id)
                .expect("the resume should start a shell");
            let mut text = String::new();
            for _ in 0..200 {
                text = runtime.recent_unwrapped_text(40);
                if text.contains("outcome[") {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            assert!(text.contains(&expected), "{session_ref:?}: {text}");
            assert!(!text.contains("autoResume took"), "{session_ref:?}: {text}");
            for (_, runtime) in app.terminal_runtimes.drain() {
                runtime.shutdown();
            }
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pending_agent_resume_can_launch_after_theme_wait_expires() {
        let mut app = test_app();
        let workspace = crate::workspace::Workspace::test_new("restored");
        let pane_id = workspace.tabs[0].root_pane;
        let terminal_id = workspace.terminal_id(pane_id).cloned().unwrap();
        app.state.view.pane_infos = workspace.tabs[0]
            .layout
            .panes(ratatui::layout::Rect::new(0, 0, 100, 30));
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 30);
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        app.state
            .terminals
            .get_mut(&terminal_id)
            .expect("test terminal should exist")
            .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "codex".into(),
            argv: long_running_test_argv(),
            dedupe_key: "herdr:codex\0codex\0Id\0codex-session".into(),
        });

        app.sync_pending_agent_resume_deadline(std::time::Instant::now());
        assert!(!app.start_pending_agent_resumes(Instant::now(), false));
        assert!(app.start_pending_agent_resumes(Instant::now(), true));
        assert!(app.terminal_runtimes.get(&terminal_id).is_some());

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn pending_agent_resume_launches_hidden_panes_with_current_terminal_area() {
        let mut app = test_app();
        let active_workspace = crate::workspace::Workspace::test_new("active");
        let active_pane = active_workspace.tabs[0].root_pane;
        let active_terminal = active_workspace.terminal_id(active_pane).cloned().unwrap();
        let hidden_workspace = crate::workspace::Workspace::test_new("hidden");
        let hidden_pane = hidden_workspace.tabs[0].root_pane;
        let hidden_terminal = hidden_workspace.terminal_id(hidden_pane).cloned().unwrap();
        app.state.view.pane_infos = active_workspace.tabs[0]
            .layout
            .panes(ratatui::layout::Rect::new(0, 0, 100, 30));
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 30);
        app.state.workspaces = vec![active_workspace, hidden_workspace];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        app.state.host_terminal_theme = crate::terminal_theme::TerminalTheme {
            foreground: Some(crate::terminal_theme::RgbColor {
                r: 220,
                g: 220,
                b: 220,
            }),
            background: Some(crate::terminal_theme::RgbColor {
                r: 20,
                g: 20,
                b: 20,
            }),
            ..Default::default()
        };
        for terminal_id in [&active_terminal, &hidden_terminal] {
            app.state
                .terminals
                .get_mut(terminal_id)
                .expect("test terminal should exist")
                .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
                agent: "codex".into(),
                argv: long_running_test_argv(),
                dedupe_key: format!("herdr:codex\0codex\0Id\0{terminal_id}"),
            });
        }
        app.pending_agent_resume_deadline =
            Some(std::time::Instant::now() - std::time::Duration::from_millis(1));

        let now = Instant::now();
        assert!(app.start_pending_agent_resumes(now, false));
        assert!(app.terminal_runtimes.get(&active_terminal).is_some());
        assert!(app.terminal_runtimes.get(&hidden_terminal).is_none());
        assert!(!app.start_pending_agent_resumes(now, true));
        assert!(
            app.start_pending_agent_resumes(now + std::time::Duration::from_millis(100), false,)
        );
        assert!(app.terminal_runtimes.get(&hidden_terminal).is_some());
        assert!(
            app.pending_agent_resume_deadline.is_none(),
            "launched pending resumes should clear the wakeup deadline"
        );

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn pending_agent_resume_launches_inactive_tab_panes_with_current_terminal_area() {
        let mut app = test_app();
        let mut workspace = crate::workspace::Workspace::test_new("tabs");
        let active_pane = workspace.tabs[0].root_pane;
        let inactive_tab = workspace.test_add_tab(Some("agents"));
        let inactive_pane = workspace.tabs[inactive_tab].root_pane;
        let inactive_terminal = workspace.tabs[inactive_tab]
            .terminal_id(inactive_pane)
            .cloned()
            .unwrap();
        app.state.view.pane_infos = workspace.tabs[0]
            .layout
            .panes(ratatui::layout::Rect::new(0, 0, 100, 30));
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 30);
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        assert!(app
            .state
            .workspaces
            .first()
            .and_then(|ws| ws.tabs[0].terminal_id(active_pane))
            .is_some());
        app.state.host_terminal_theme = crate::terminal_theme::TerminalTheme {
            foreground: Some(crate::terminal_theme::RgbColor {
                r: 220,
                g: 220,
                b: 220,
            }),
            background: Some(crate::terminal_theme::RgbColor {
                r: 20,
                g: 20,
                b: 20,
            }),
            ..Default::default()
        };
        app.state
            .terminals
            .get_mut(&inactive_terminal)
            .expect("inactive tab terminal should exist")
            .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "codex".into(),
            argv: long_running_test_argv(),
            dedupe_key: "herdr:codex\0codex\0Id\0inactive-tab-session".into(),
        });

        assert!(app.start_pending_agent_resumes(Instant::now(), false));
        assert!(app.terminal_runtimes.get(&inactive_terminal).is_some());
        assert!(
            app.state
                .terminals
                .get(&inactive_terminal)
                .expect("inactive tab terminal should still exist")
                .pending_agent_resume_plan
                .is_none(),
            "inactive tab restored panes should not wait for tab focus"
        );

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn pending_agent_resume_launches_zoom_hidden_active_tab_panes() {
        let mut app = test_app();
        let mut workspace = crate::workspace::Workspace::test_new("zoomed");
        let hidden_pane = workspace.tabs[0].root_pane;
        let visible_pane = workspace.test_split(ratatui::layout::Direction::Horizontal);
        workspace.tabs[0].zoomed = true;
        let hidden_terminal = workspace.terminal_id(hidden_pane).cloned().unwrap();
        app.state.view.pane_infos = vec![crate::layout::PaneInfo {
            id: visible_pane,
            rect: ratatui::layout::Rect::new(0, 0, 100, 30),
            inner_rect: ratatui::layout::Rect::new(1, 1, 98, 28),
            scrollbar_rect: None,
            borders: ratatui::widgets::Borders::ALL,
            is_focused: true,
        }];
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 30);
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        app.state.host_terminal_theme = crate::terminal_theme::TerminalTheme {
            foreground: Some(crate::terminal_theme::RgbColor {
                r: 220,
                g: 220,
                b: 220,
            }),
            background: Some(crate::terminal_theme::RgbColor {
                r: 20,
                g: 20,
                b: 20,
            }),
            ..Default::default()
        };
        app.state
            .terminals
            .get_mut(&hidden_terminal)
            .expect("hidden zoom pane terminal should exist")
            .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "codex".into(),
            argv: long_running_test_argv(),
            dedupe_key: "herdr:codex\0codex\0Id\0zoom-hidden-session".into(),
        });

        assert!(app.start_pending_agent_resumes(Instant::now(), false));
        assert!(app.terminal_runtimes.get(&hidden_terminal).is_some());
        assert!(
            app.state
                .terminals
                .get(&hidden_terminal)
                .expect("hidden zoom pane terminal should still exist")
                .pending_agent_resume_plan
                .is_none(),
            "zoom-hidden restored panes should not wait for pane focus"
        );

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn pending_agent_resume_uses_current_terminal_area_for_background_panes() {
        let mut app = test_app();
        let previous_workspace = crate::workspace::Workspace::test_new("previous");
        let previous_pane = previous_workspace.tabs[0].root_pane;
        let previous_terminal = previous_workspace
            .terminal_id(previous_pane)
            .cloned()
            .unwrap();
        let current_workspace = crate::workspace::Workspace::test_new("current");
        app.state.view.pane_infos = previous_workspace.tabs[0]
            .layout
            .panes(ratatui::layout::Rect::new(0, 0, 100, 30));
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 80, 24);
        app.state.workspaces = vec![previous_workspace, current_workspace];
        app.state.active = Some(1);
        app.state.ensure_test_terminals();
        app.state.host_terminal_theme = crate::terminal_theme::TerminalTheme {
            foreground: Some(crate::terminal_theme::RgbColor {
                r: 220,
                g: 220,
                b: 220,
            }),
            background: Some(crate::terminal_theme::RgbColor {
                r: 20,
                g: 20,
                b: 20,
            }),
            ..Default::default()
        };
        app.state
            .terminals
            .get_mut(&previous_terminal)
            .expect("test terminal should exist")
            .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "codex".into(),
            argv: long_running_test_argv(),
            dedupe_key: "herdr:codex\0codex\0Id\0codex-session".into(),
        });

        app.sync_pending_agent_resume_deadline(std::time::Instant::now());
        assert!(app.pending_agent_resume_deadline.is_some());
        assert!(app.start_pending_agent_resumes(Instant::now(), false));
        assert!(app.terminal_runtimes.get(&previous_terminal).is_some());
        assert!(
            app.state
                .terminals
                .get(&previous_terminal)
                .expect("previous terminal should still exist")
                .pending_agent_resume_plan
                .is_none(),
            "background restored panes should not wait for focus once terminal area is known"
        );

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pending_agent_resume_launches_with_inner_rect_size() {
        let mut app = test_app();
        let mut workspace = crate::workspace::Workspace::test_new("split");
        let pane_id = workspace.test_split(ratatui::layout::Direction::Horizontal);
        let terminal_id = workspace.terminal_id(pane_id).cloned().unwrap();
        app.state.view.pane_infos = vec![crate::layout::PaneInfo {
            id: pane_id,
            rect: ratatui::layout::Rect::new(0, 0, 100, 30),
            inner_rect: ratatui::layout::Rect::new(1, 1, 98, 28),
            scrollbar_rect: None,
            borders: ratatui::widgets::Borders::ALL,
            is_focused: true,
        }];
        app.state.view.terminal_area = ratatui::layout::Rect::new(0, 0, 100, 30);
        app.state.workspaces = vec![workspace];
        app.state.active = Some(0);
        app.state.ensure_test_terminals();
        app.state.host_terminal_theme = crate::terminal_theme::TerminalTheme {
            foreground: Some(crate::terminal_theme::RgbColor {
                r: 220,
                g: 220,
                b: 220,
            }),
            background: Some(crate::terminal_theme::RgbColor {
                r: 20,
                g: 20,
                b: 20,
            }),
            ..Default::default()
        };
        app.state
            .terminals
            .get_mut(&terminal_id)
            .expect("test terminal should exist")
            .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "codex".into(),
            argv: long_running_test_argv(),
            dedupe_key: "herdr:codex\0codex\0Id\0codex-session".into(),
        });

        assert!(app.start_pending_agent_resumes(Instant::now(), false));
        assert_eq!(
            app.terminal_runtimes
                .get(&terminal_id)
                .expect("pending resume should launch")
                .current_size(),
            (28, 98)
        );

        for (_, runtime) in app.terminal_runtimes.drain() {
            runtime.shutdown();
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn shell_command_from_argv_quotes_resume_arguments() {
        let argv = vec![
            "claude".to_string(),
            "--resume".to_string(),
            "session with ' quote".to_string(),
        ];

        assert_eq!(
            shell_command_from_argv(&argv).as_deref(),
            Some("claude --resume 'session with '\\'' quote'")
        );
        assert_eq!(shell_command_from_argv(&[]), None);
    }
}
