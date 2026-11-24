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
    let heartbeat_file = state_dir.join("heartbeat");

    let bridge = match WdttgBridge::open() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("Failed to open wdttg database: {e}");
            std::process::exit(1);
        }
    };

    let threshold = chrono::TimeDelta::seconds(config.bridge_threshold_seconds as i64);

    let mut tracker = Tracker::new(
        threshold,
        chrono::TimeDelta::seconds(config.rename_timeout_seconds as i64),
    );

    // Startup: if the previous daemon left a fresh heartbeat and the focused
    // workspace has an open session, adopt it so the restart doesn't split
    // the session. Otherwise close orphans at the last heartbeat time (or
    // now, on first run), bounding them at when tracking actually stopped.
    let now = Utc::now();
    let last_run = std::fs::read_to_string(&heartbeat_file)
        .ok()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s.trim()).ok())
        .map(|dt| dt.with_timezone(&Utc));
    let mut adopted_id: Option<i64> = None;
    if let (Some(lr), Some(w)) = (
        last_run,
        client_peek_focused(&HerdrClient::new(), &config),
    ) {
        if now - lr <= threshold {
            if let Ok(Some((sid, start, tid))) =
                bridge.find_open_session_by_task(&w.task_name)
            {
                tracing::info!(
                    "adopting open session {sid} on {} (restart within threshold)",
                    w.task_name
                );
                tracker.adopt(
                    &WorkspaceInfo {
                        id: w.id.clone(),
                        label: w.label.clone(),
                        cwd: None,
                    },
                    tid,
                    w.task_name.clone(),
                    sid,
                    start,
                    now,
                );
                adopted_id = Some(sid);
            }
        }
    }
    let close_at = last_run.unwrap_or(now);
    match bridge.reconcile_orphans(close_at, adopted_id) {
        Ok(n) if n > 0 => tracing::info!("reconciled {n} orphaned sessions at {close_at}"),
        Ok(_) => {}
        Err(e) => tracing::warn!("startup reconciliation failed: {e}"),
    }

    // Seed pause state from the flag file.
    tracker.set_paused(paused_file.exists());

    let client = HerdrClient::new();
    let poll_interval = StdDuration::from_secs(config.poll_interval_seconds.max(1));
    // A wall-clock gap between ticks larger than this means the machine
    // suspended (or the daemon was frozen); sessions must not span it.
    let jump_threshold =
        chrono::TimeDelta::seconds((config.poll_interval_seconds * 10).max(60) as i64);
    let mut last_tick = Utc::now();

    tracing::info!(
        "wdttg-herdr daemon started (bridge={}s, poll={}s, template={})",
        config.bridge_threshold_seconds,
        config.poll_interval_seconds,
        config.task_name_template
    );

    loop {
        // Detect suspend/resume: if the wall clock jumped far past the sleep
        // interval, treat the user as having left at the last tick.
        let tick_now = Utc::now();
        if tick_now - last_tick > jump_threshold {
            tracing::info!(
                "time jump of {}s detected; treating workspaces as left at last tick",
                (tick_now - last_tick).num_seconds()
            );
            tracker.handle_time_jump(last_tick);
        }
        last_tick = tick_now;

        // Check for external pause/resume.
        let want_paused = paused_file.exists();
        if want_paused != tracker.paused {
            tracker.set_paused(want_paused);
            if want_paused {
                // Close all recorded sessions at the pause time.
                let now = Utc::now();
                for s in tracker.sessions.values() {
                    if let Some(sid) = s.session_id {
                        if let Err(e) = bridge.close_session(sid, now) {
                            tracing::error!("close_session({sid}) on pause failed: {e}");
                        }
                    }
                }
            }
            tracker.clear_sessions();
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

                let cmds = tracker.sweep(
                    &workspaces,
                    focused,
                    now,
                    |label, cwd| config.derive_task_name(label, cwd),
                );

                // Execute commands against wdttg.
                for cmd in &cmds {
                    execute_cmd(&bridge, &mut tracker, cmd, &config);
                }
            }
            Err(e) => {
                tracing::debug!("herdr poll failed: {e}");
            }
        }

        // Write status file for the tab bar, and the heartbeat used to bound
        // orphaned sessions on the next startup.
        let line = tracker.status_line(Utc::now());
        let tmp = state_dir.join("status.txt.tmp");
        if std::fs::write(&tmp, &line).is_ok() {
            let _ = std::fs::rename(&tmp, &status_file);
        }
        let _ = std::fs::write(&heartbeat_file, Utc::now().to_rfc3339());

        std::thread::sleep(poll_interval);
    }
}

/// One-shot poll for the focused workspace, used at startup for adoption.
struct FocusedWorkspace {
    id: String,
    label: String,
    task_name: String,
}

fn client_peek_focused(client: &HerdrClient, config: &config::Config) -> Option<FocusedWorkspace> {
    let snap = client.poll().ok()?;
    let fw = snap.focused_workspace_id?;
    let w = snap.workspaces.iter().find(|w| w.id == fw)?;
    let label = w.label.clone().unwrap_or_default();
    Some(FocusedWorkspace {
        id: w.id.clone(),
        task_name: config.derive_task_name(&label, None),
        label,
    })
}

fn execute_cmd(bridge: &WdttgBridge, tracker: &mut Tracker, cmd: &Cmd, _config: &config::Config) {    match cmd {
        Cmd::RecordSession {
            workspace_id,
            task_name,
            start,
        } => {
            match bridge.get_or_create_task(task_name) {
                Ok(task_id) => match bridge.record_session(task_id, *start) {
                    Ok(session_id) => {
                        if let Some(s) = tracker.sessions.get_mut(workspace_id) {
                            s.task_id = task_id;
                            s.session_id = Some(session_id);
                        }
                    }
                    Err(e) => {
                        tracing::error!("record_session failed: {e}");
                        if let Some(s) = tracker.sessions.get_mut(workspace_id) {
                            s.recorded = false; // retry next sweep
                        }
                    }
                },
                Err(e) => {
                    tracing::error!("get_or_create_task({task_name}) failed: {e}");
                    if let Some(s) = tracker.sessions.get_mut(workspace_id) {
                        s.recorded = false; // retry next sweep
                    }
                }
            }
        }
        Cmd::CloseSession {
            session_id,
            end_time,
        } => {
            if let Err(e) = bridge.close_session(*session_id, *end_time) {
                tracing::error!("close_session({session_id}) failed: {e}");
            }
        }
        Cmd::RenameTask { task_id, new_name } => {
            if let Err(e) = bridge.rename_task(*task_id, new_name) {
                tracing::error!("rename_task({task_id}, {new_name}) failed: {e}");
            }
        }
    }
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
            let cmds = tracker.resolve_rename(decision, new_name, Utc::now());
            for cmd in &cmds {
                execute_cmd(bridge, tracker, cmd, &config::Config::default());
            }
        }
        let _ = std::fs::remove_file(&decision_file);
    }
}
