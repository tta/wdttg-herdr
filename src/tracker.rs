use chrono::{DateTime, TimeDelta, Utc};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct WorkspaceInfo {
    pub id: String,
    pub label: String,
    pub cwd: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WorkspaceSession {
    pub task_id: i64,
    pub task_name: String,
    pub last_seen_label: String,
    pub session_id: i64,
    pub session_start: DateTime<Utc>,
    pub state: SessionState,
}

#[derive(Debug, Clone)]
pub enum SessionState {
    Active,
    Pending { left_at: DateTime<Utc> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct PendingRename {
    pub workspace_id: String,
    pub old_label: String,
    pub new_label: String,
    pub prompted_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RenameDecision {
    Continue,
    NewTask,
}

#[derive(Debug, Clone)]
pub enum Cmd {
    StartSession {
        task_name: String,
    },
    BridgeSession {
        session_id: i64,
        new_end: DateTime<Utc>,
    },
    CloseSession {
        session_id: i64,
        end_time: DateTime<Utc>,
    },
    CloseAndRestart {
        old_session_id: i64,
        close_at: DateTime<Utc>,
        task_id: i64,
    },
    RenameTask {
        task_id: i64,
        new_name: String,
    },
}

pub struct Tracker {
    pub sessions: HashMap<String, WorkspaceSession>,
    pub threshold: TimeDelta,
    pub paused: bool,
    pub pending_rename: Option<PendingRename>,
    pub rename_timeout: TimeDelta,
}

impl Tracker {
    pub fn new(threshold: TimeDelta, rename_timeout: TimeDelta) -> Self {
        Self {
            sessions: HashMap::new(),
            threshold,
            paused: false,
            pending_rename: None,
            rename_timeout,
        }
    }

    /// Sweep all workspaces against the current herdr state.
    /// Returns the commands to execute against wdttg.
    pub fn sweep(
        &mut self,
        workspaces: &[WorkspaceInfo],
        focused_id: Option<&str>,
        now: DateTime<Utc>,
        derive_name: impl Fn(&str, Option<&str>) -> String,
    ) -> Vec<Cmd> {
        let mut cmds = Vec::new();

        // 1. Handle rename detection and timeout for existing sessions.
        self.process_rename_timeout(now, &mut cmds);

        // 2. Update state for each known workspace.
        let known_ids: Vec<String> = self.sessions.keys().cloned().collect();
        for ws_id in known_ids {
            let ws = workspaces.iter().find(|w| w.id == ws_id);
            let still_exists = ws.is_some();
            let is_focused = focused_id.is_some_and(|f| f == ws_id);

            // Borrow out what we need, then mutate.
            let (task_id, last_label, session_id, state) = {
                let s = &self.sessions[&ws_id];
                (
                    s.task_id,
                    s.last_seen_label.clone(),
                    s.session_id,
                    s.state.clone(),
                )
            };

            if !still_exists {
                // Workspace closed: end the session.
                let end = match &state {
                    SessionState::Active => now,
                    SessionState::Pending { left_at } => *left_at,
                };
                cmds.push(Cmd::CloseSession {
                    session_id,
                    end_time: end,
                });
                self.sessions.remove(&ws_id);
                continue;
            }

            let ws = ws.unwrap();

            // Rename detection.
            if ws.label != last_label {
                if self.pending_rename.is_none() {
                    self.pending_rename = Some(PendingRename {
                        workspace_id: ws_id.clone(),
                        old_label: last_label.clone(),
                        new_label: ws.label.clone(),
                        prompted_at: now,
                    });
                }
                // Update last_seen_label so we don't re-trigger every tick.
                self.sessions.get_mut(&ws_id).unwrap().last_seen_label = ws.label.clone();
            }

            // Focus state transition.
            match (&state, is_focused) {
                (SessionState::Active, true) => {}
                (SessionState::Active, false) => {
                    self.sessions.get_mut(&ws_id).unwrap().state =
                        SessionState::Pending { left_at: now };
                }
                (SessionState::Pending { .. }, true) => {
                    let left_at = match &state {
                        SessionState::Pending { left_at } => *left_at,
                        _ => now,
                    };
                    if now - left_at <= self.threshold {
                        cmds.push(Cmd::BridgeSession {
                            session_id,
                            new_end: now,
                        });
                    } else {
                        cmds.push(Cmd::CloseAndRestart {
                            old_session_id: session_id,
                            close_at: left_at,
                            task_id,
                        });
                    }
                    self.sessions.get_mut(&ws_id).unwrap().state = SessionState::Active;
                }
                (SessionState::Pending { .. }, false) => {}
            }
        }

        // 3. Start sessions for newly-focused workspaces we don't track yet.
        if let Some(f) = focused_id {
            if !self.sessions.contains_key(f) {
                if let Some(ws) = workspaces.iter().find(|w| w.id == f) {
                    let name = derive_name(&ws.label, ws.cwd.as_deref());
                    cmds.push(Cmd::StartSession {
                        task_name: name.clone(),
                    });
                    self.sessions.insert(
                        f.to_string(),
                        WorkspaceSession {
                            task_id: -1,
                            task_name: name,
                            last_seen_label: ws.label.clone(),
                            session_id: -1,
                            session_start: now,
                            state: SessionState::Active,
                        },
                    );
                }
            }
        }

        cmds
    }

    /// Close any pending sessions whose bridge threshold has expired.
    pub fn close_expired(&mut self, now: DateTime<Utc>) -> Vec<Cmd> {
        let mut cmds = Vec::new();
        let expired: Vec<(String, i64, DateTime<Utc>)> = self
            .sessions
            .iter()
            .filter_map(|(id, s)| match &s.state {
                SessionState::Pending { left_at }
                    if now - *left_at > self.threshold =>
                {
                    Some((id.clone(), s.session_id, *left_at))
                }
                _ => None,
            })
            .collect();

        for (id, session_id, left_at) in expired {
            cmds.push(Cmd::CloseSession {
                session_id,
                end_time: left_at,
            });
            self.sessions.remove(&id);
        }
        cmds
    }

    fn process_rename_timeout(&mut self, now: DateTime<Utc>, cmds: &mut Vec<Cmd>) {
        if let Some(pr) = &self.pending_rename {
            if now - pr.prompted_at > self.rename_timeout {
                let new_label = pr.new_label.clone();
                let ws_id = pr.workspace_id.clone();
                self.pending_rename = None;
                if let Some(s) = self.sessions.get_mut(&ws_id) {
                    // Default: continue with renamed task.
                    let new_name = format!("herdr:{}", new_label);
                    cmds.push(Cmd::RenameTask {
                        task_id: s.task_id,
                        new_name,
                    });
                    s.task_name = format!("herdr:{}", new_label);
                    s.last_seen_label = new_label;
                }
            }
        }
    }

    pub fn resolve_rename(
        &mut self,
        decision: RenameDecision,
        new_name: String,
    ) -> Vec<Cmd> {
        let Some(pr) = self.pending_rename.take() else {
            return Vec::new();
        };
        let mut cmds = Vec::new();
        match decision {
            RenameDecision::Continue => {
                if let Some(s) = self.sessions.get_mut(&pr.workspace_id) {
                    cmds.push(Cmd::RenameTask {
                        task_id: s.task_id,
                        new_name: new_name.clone(),
                    });
                    s.task_name = new_name;
                    s.last_seen_label = pr.new_label.clone();
                }
            }
            RenameDecision::NewTask => {
                if let Some(s) = self.sessions.get(&pr.workspace_id) {
                    let now = Utc::now();
                    let close_at = match &s.state {
                        SessionState::Active => now,
                        SessionState::Pending { left_at } => *left_at,
                    };
                    cmds.push(Cmd::CloseSession {
                        session_id: s.session_id,
                        end_time: close_at,
                    });
                    // Start a fresh session on the new task; the executor
                    // resolves the new task_id via get_or_create.
                    cmds.push(Cmd::StartSession {
                        task_name: new_name,
                    });
                    if let Some(s) = self.sessions.get_mut(&pr.workspace_id) {
                        s.task_id = -1;
                        s.session_id = -1;
                        s.session_start = now;
                        s.state = SessionState::Active;
                    }
                }
            }
        }
        cmds
    }

    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
    }

    pub fn status_line(&self, now: DateTime<Utc>) -> String {
        if self.paused {
            return "⏸ wdttg paused".to_string();
        }
        // Find the active session (if any).
        let mut best: Option<(&String, &WorkspaceSession)> = None;
        for (id, s) in &self.sessions {
            if let SessionState::Active = &s.state {
                best = Some((id, s));
                break;
            }
        }
        match best {
            Some((_id, s)) => {
                let elapsed = now - s.session_start;
                format!("⏱ {} {}", s.task_name, format_duration(elapsed))
            }
            None => "○ wdttg idle".to_string(),
        }
    }
}

fn format_duration(d: TimeDelta) -> String {
    let total_minutes = d.num_minutes().max(0);
    let hours = total_minutes / 60;
    let minutes = total_minutes % 60;
    if hours > 0 {
        format!("{}h {}m", hours, minutes)
    } else {
        format!("{}m", minutes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ts(h: u32, m: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 22, h, m, 0).unwrap()
    }

    fn ws(id: &str, label: &str) -> WorkspaceInfo {
        WorkspaceInfo {
            id: id.to_string(),
            label: label.to_string(),
            cwd: None,
        }
    }

    fn derive(label: &str, _cwd: Option<&str>) -> String {
        format!("herdr:{}", label)
    }

    #[test]
    fn focus_new_workspace_starts_session() {
        let mut t = Tracker::new(TimeDelta::minutes(3), TimeDelta::seconds(60));
        let cmds = t.sweep(&[ws("a", "a")], Some("a"), ts(9, 0), derive);
        assert_eq!(cmds.len(), 1);
        match &cmds[0] {
            Cmd::StartSession { task_name, .. } => assert_eq!(task_name, "herdr:a"),
            other => panic!("expected StartSession, got {:?}", other),
        }
        assert!(t.sessions.contains_key("a"));
    }

    #[test]
    fn switch_workspace_pauses_old_starts_new() {
        let mut t = Tracker::new(TimeDelta::minutes(3), TimeDelta::seconds(60));
        t.sweep(&[ws("a", "a")], Some("a"), ts(9, 0), derive);
        // Resolve the sentinel task_id as the executor would.
        t.sessions.get_mut("a").unwrap().task_id = 1;
        t.sessions.get_mut("a").unwrap().session_id = 10;

        let cmds = t.sweep(&[ws("a", "a"), ws("b", "b")], Some("b"), ts(9, 5), derive);
        // b starts; a becomes pending (no cmd for a on this tick).
        assert!(matches!(&cmds[0], Cmd::StartSession { task_name, .. } if task_name == "herdr:b"));
        assert!(matches!(
            t.sessions["a"].state,
            SessionState::Pending { .. }
        ));
    }

    #[test]
    fn return_within_threshold_bridges() {
        let mut t = Tracker::new(TimeDelta::minutes(3), TimeDelta::seconds(60));
        t.sweep(&[ws("a", "a")], Some("a"), ts(9, 0), derive);
        t.sessions.get_mut("a").unwrap().task_id = 1;
        t.sessions.get_mut("a").unwrap().session_id = 10;

        // Move focus away.
        t.sweep(&[ws("a", "a"), ws("b", "b")], Some("b"), ts(9, 5), derive);
        // Return to a after 2 minutes (within 3-min threshold).
        let cmds = t.sweep(&[ws("a", "a"), ws("b", "b")], Some("a"), ts(9, 7), derive);
        assert!(
            cmds.iter().any(|c| matches!(c, Cmd::BridgeSession { session_id, .. } if *session_id == 10)),
            "expected BridgeSession, got {:?}",
            cmds
        );
        assert!(matches!(t.sessions["a"].state, SessionState::Active));
    }

    #[test]
    fn return_after_threshold_closes_and_restarts() {
        let mut t = Tracker::new(TimeDelta::minutes(3), TimeDelta::seconds(60));
        t.sweep(&[ws("a", "a")], Some("a"), ts(9, 0), derive);
        t.sessions.get_mut("a").unwrap().task_id = 1;
        t.sessions.get_mut("a").unwrap().session_id = 10;

        t.sweep(&[ws("a", "a"), ws("b", "b")], Some("b"), ts(9, 5), derive);
        // Return after 5 minutes (exceeds 3-min threshold).
        let cmds = t.sweep(&[ws("a", "a"), ws("b", "b")], Some("a"), ts(9, 10), derive);
        assert!(
            cmds.iter().any(|c| matches!(c, Cmd::CloseAndRestart { old_session_id, .. } if *old_session_id == 10)),
            "expected CloseAndRestart, got {:?}",
            cmds
        );
    }

    #[test]
    fn pending_session_expires_after_threshold() {
        let mut t = Tracker::new(TimeDelta::minutes(3), TimeDelta::seconds(60));
        t.sweep(&[ws("a", "a")], Some("a"), ts(9, 0), derive);
        t.sessions.get_mut("a").unwrap().task_id = 1;
        t.sessions.get_mut("a").unwrap().session_id = 10;

        t.sweep(&[ws("a", "a"), ws("b", "b")], Some("b"), ts(9, 5), derive);
        // 4 minutes later, a is still not focused.
        let cmds = t.close_expired(ts(9, 9));
        assert!(
            cmds.iter().any(|c| matches!(c, Cmd::CloseSession { session_id, end_time } if *session_id == 10 && *end_time == ts(9, 5))),
            "expected CloseSession at leave time, got {:?}",
            cmds
        );
        assert!(!t.sessions.contains_key("a"));
    }

    #[test]
    fn workspace_closed_while_active_closes_at_now() {
        let mut t = Tracker::new(TimeDelta::minutes(3), TimeDelta::seconds(60));
        t.sweep(&[ws("a", "a")], Some("a"), ts(9, 0), derive);
        t.sessions.get_mut("a").unwrap().session_id = 10;

        // Workspace a no longer exists in herdr.
        let cmds = t.sweep(&[], Some("a"), ts(9, 2), derive);
        assert!(
            cmds.iter().any(|c| matches!(c, Cmd::CloseSession { session_id, end_time } if *session_id == 10 && *end_time == ts(9, 2))),
            "expected CloseSession at now, got {:?}",
            cmds
        );
    }

    #[test]
    fn paused_tracker_still_tracks_focus_but_no_new_sessions() {
        // Pause is checked by the executor, not the tracker's sweep;
        // the tracker records focus state so resume can act on it.
        let mut t = Tracker::new(TimeDelta::minutes(3), TimeDelta::seconds(60));
        t.set_paused(true);
        let cmds = t.sweep(&[ws("a", "a")], Some("a"), ts(9, 0), derive);
        // Sweep still records the session (executor will skip StartSession when paused).
        assert!(t.sessions.contains_key("a"));
        assert!(!cmds.is_empty());
    }

    #[test]
    fn rename_detected_when_label_changes() {
        let mut t = Tracker::new(TimeDelta::minutes(3), TimeDelta::seconds(60));
        t.sweep(&[ws("a", "old")], Some("a"), ts(9, 0), derive);
        t.sessions.get_mut("a").unwrap().task_id = 1;
        t.sessions.get_mut("a").unwrap().session_id = 10;

        // Label changes to "new".
        t.sweep(&[ws("a", "new")], Some("a"), ts(9, 1), derive);
        assert!(t.pending_rename.is_some());
        let pr = t.pending_rename.as_ref().unwrap();
        assert_eq!(pr.old_label, "old");
        assert_eq!(pr.new_label, "new");
    }

    #[test]
    fn rename_continue_renames_task() {
        let mut t = Tracker::new(TimeDelta::minutes(3), TimeDelta::seconds(60));
        t.sweep(&[ws("a", "old")], Some("a"), ts(9, 0), derive);
        t.sessions.get_mut("a").unwrap().task_id = 1;
        t.sessions.get_mut("a").unwrap().session_id = 10;
        t.sweep(&[ws("a", "new")], Some("a"), ts(9, 1), derive);

        let cmds = t.resolve_rename(RenameDecision::Continue, "herdr:new".to_string());
        assert!(
            cmds.iter().any(|c| matches!(c, Cmd::RenameTask { task_id, new_name } if *task_id == 1 && new_name == "herdr:new")),
            "expected RenameTask, got {:?}",
            cmds
        );
        assert_eq!(t.sessions["a"].task_name, "herdr:new");
        assert!(t.pending_rename.is_none());
    }

    #[test]
    fn rename_new_task_closes_and_starts() {
        let mut t = Tracker::new(TimeDelta::minutes(3), TimeDelta::seconds(60));
        t.sweep(&[ws("a", "old")], Some("a"), ts(9, 0), derive);
        t.sessions.get_mut("a").unwrap().task_id = 1;
        t.sessions.get_mut("a").unwrap().session_id = 10;
        t.sweep(&[ws("a", "new")], Some("a"), ts(9, 1), derive);

        let cmds = t.resolve_rename(RenameDecision::NewTask, "herdr:new".to_string());
        assert!(
            cmds.iter().any(|c| matches!(c, Cmd::CloseSession { session_id, .. } if *session_id == 10)),
            "expected CloseSession, got {:?}",
            cmds
        );
        assert!(
            cmds.iter().any(|c| matches!(c, Cmd::StartSession { task_name, .. } if task_name == "herdr:new")),
            "expected StartSession, got {:?}",
            cmds
        );
    }

    #[test]
    fn parallel_workspaces_both_tracked() {
        let mut t = Tracker::new(TimeDelta::minutes(3), TimeDelta::seconds(60));
        t.sweep(&[ws("a", "a")], Some("a"), ts(9, 0), derive);
        t.sessions.get_mut("a").unwrap().task_id = 1;
        t.sessions.get_mut("a").unwrap().session_id = 10;

        // Switch to b: a goes pending, b starts.
        t.sweep(&[ws("a", "a"), ws("b", "b")], Some("b"), ts(9, 1), derive);
        t.sessions.get_mut("b").unwrap().task_id = 2;
        t.sessions.get_mut("b").unwrap().session_id = 20;

        // Both a (pending) and b (active) are tracked.
        assert!(t.sessions.contains_key("a"));
        assert!(t.sessions.contains_key("b"));
        assert!(matches!(t.sessions["a"].state, SessionState::Pending { .. }));
        assert!(matches!(t.sessions["b"].state, SessionState::Active));
    }

    #[test]
    fn status_line_shows_active_task() {
        let mut t = Tracker::new(TimeDelta::minutes(3), TimeDelta::seconds(60));
        t.sweep(&[ws("a", "a")], Some("a"), ts(9, 0), derive);
        t.sessions.get_mut("a").unwrap().session_start = ts(9, 0);
        let line = t.status_line(ts(10, 23));
        assert!(line.contains("herdr:a"), "got: {}", line);
        assert!(line.contains("1h 23m"), "got: {}", line);
    }
}
