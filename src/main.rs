mod config;
mod herdr_client;
mod tracker;
mod wdttg_bridge;

use chrono::Utc;
use herdr_client::HerdrClient;
use std::path::PathBuf;
use std::time::Duration as StdDuration;
use tracker::{Cmd, RenameDecision, Tracker, WorkspaceInfo};
use wdttg_bridge::WdttgBridge;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG")
                .unwrap_or_else(|_| "wdttg_herdr=info".to_string()),
        )
        .init();

    let config_dir = std::env::var("HERDR_PLUGIN_CONFIG_DIR")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let config = config::Config::load(&config_dir);

    let state_dir = std::env::var("HERDR_PLUGIN_STATE_DIR")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| config_dir.clone());
    std::fs::create_dir_all(&state_dir).ok();

    let status_file = state_dir.join("status.txt");
    let paused_file = state_dir.join("paused");

    let bridge = match WdttgBridge::open() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("Failed to open wdttg database: {e}");
            std::process::exit(1);
        }
    };

    let now = Utc::now();
    match bridge.reconcile_orphans(now) {
        Ok(n) if n > 0 => tracing::info!("reconciled {n} orphaned sessions on startup"),
        Ok(_) => {}
        Err(e) => tracing::warn!("startup reconciliation failed: {e}"),
    }

    let mut tracker = Tracker::new(
        chrono::TimeDelta::seconds(config.bridge_threshold_seconds as i64),
        chrono::TimeDelta::seconds(config.rename_timeout_seconds as i64),
    );
    // Seed pause state from the flag file.
    tracker.set_paused(paused_file.exists());

    let client = HerdrClient::new();
    let poll_interval = StdDuration::from_secs(config.poll_interval_seconds.max(1));

    tracing::info!(
        "wdttg-herdr daemon started (bridge={}s, poll={}s, template={})",
        config.bridge_threshold_seconds,
        config.poll_interval_seconds,
        config.task_name_template
    );

    loop {
        // Check for external pause/resume.
        let want_paused = paused_file.exists();
        if want_paused != tracker.paused {
            tracker.set_paused(want_paused);
            tracing::info!("tracking {}ed", if want_paused { "paused" } else { "resumed" });
        }

        match client.poll() {
            Ok(state) => {
                let workspaces: Vec<WorkspaceInfo> = state
                    .workspaces
                    .iter()
                    .map(|w| WorkspaceInfo {
                        id: w.id.clone(),
                        label: w.label.clone().unwrap_or_default(),
                        cwd: None,
                    })
                    .collect();
                let focused = state.focused_workspace_id.as_deref();
                let now = Utc::now();

                // Resolve any pending rename decision before sweeping.
                check_rename_decision(&bridge, &state_dir, &mut tracker);

                let mut cmds = tracker.sweep(
                    &workspaces,
                    focused,
                    now,
                    |label, cwd| config.derive_task_name(label, cwd),
                );
                cmds.extend(tracker.close_expired(now));

                // Execute commands against wdttg.
                for cmd in &cmds {
                    execute_cmd(&bridge, &mut tracker, cmd, &config);
                }
            }
            Err(e) => {
                tracing::debug!("herdr poll failed: {e}");
            }
        }

        // Write status file for the tab bar.
        let line = tracker.status_line(Utc::now());
        let tmp = state_dir.join("status.txt.tmp");
        if std::fs::write(&tmp, &line).is_ok() {
            let _ = std::fs::rename(&tmp, &status_file);
        }

        std::thread::sleep(poll_interval);
    }
}

fn execute_cmd(bridge: &WdttgBridge, tracker: &mut Tracker, cmd: &Cmd, _config: &config::Config) {
    match cmd {
        Cmd::StartSession { task_name, .. } => {
            match bridge.get_or_create_task(task_name) {
                Ok(task_id) => {
                    if let Some(ws_id) = find_workspace_by_name(tracker, task_name) {
                        if let Ok(session_id) = bridge.start_session(task_id) {
                            if let Some(s) = tracker.sessions.get_mut(&ws_id) {
                                s.task_id = task_id;
                                s.session_id = session_id;
                            }
                        } else {
                            tracing::error!("start_session(task={task_id}) failed");
                        }
                    }
                }
                Err(e) => tracing::error!("get_or_create_task({task_name}) failed: {e}"),
            }
        }
        Cmd::BridgeSession { session_id, new_end } => {
            if let Err(e) = bridge.extend_session(*session_id, *new_end) {
                tracing::error!("extend_session({session_id}) failed: {e}");
            }
        }
        Cmd::CloseSession { session_id, end_time } => {
            if let Err(e) = bridge.close_session(*session_id, *end_time) {
                tracing::error!("close_session({session_id}) failed: {e}");
            }
        }
        Cmd::CloseAndRestart {
            old_session_id,
            close_at,
            task_id,
        } => {
            if let Err(e) = bridge.close_session(*old_session_id, *close_at) {
                tracing::error!("close_session({old_session_id}) failed: {e}");
            }
            if *task_id <= 0 {
                tracing::warn!("CloseAndRestart with sentinel task_id; skipping restart");
                return;
            }
            if let Ok(session_id) = bridge.start_session(*task_id) {
                for s in tracker.sessions.values_mut() {
                    if s.session_id == *old_session_id {
                        s.session_id = session_id;
                    }
                }
            }
        }
        Cmd::RenameTask { task_id, new_name } => {
            if let Err(e) = bridge.rename_task(*task_id, new_name) {
                tracing::error!("rename_task({task_id}, {new_name}) failed: {e}");
            }
        }
    }
}

fn find_workspace_by_name(tracker: &Tracker, task_name: &str) -> Option<String> {
    tracker
        .sessions
        .iter()
        .find(|(_, s)| s.task_name == task_name)
        .map(|(id, _)| id.clone())
}

/// Resolve a pending rename prompt by reading a decision file written by a
/// herdr action. Defaults to Continue if the file is absent or malformed.
fn check_rename_decision(
    bridge: &WdttgBridge,
    state_dir: &std::path::Path,
    tracker: &mut Tracker,
) {
    let decision_file = state_dir.join("rename_decision.txt");
    if let Ok(content) = std::fs::read_to_string(&decision_file) {
        let decision = match content.trim() {
            "new" => RenameDecision::NewTask,
            _ => RenameDecision::Continue,
        };
        if let Some(pr) = tracker.pending_rename.as_ref() {
            let new_name = format!("herdr:{}", pr.new_label);
            let cmds = tracker.resolve_rename(decision, new_name);
            for cmd in &cmds {
                execute_cmd(bridge, tracker, cmd, &config::Config::default());
            }
        }
        let _ = std::fs::remove_file(&decision_file);
    }
}
