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
    /// The wdttg session id once recorded; None until then.
    pub session_id: Option<i64>,
    /// Original start of the current continuous period (backdated when recorded).
    pub period_start: DateTime<Utc>,
    /// Most recent time this workspace was focused.
    pub last_focused_at: DateTime<Utc>,
    /// When focus left (Some = pending). None = currently focused.
    pub leave_at: Option<DateTime<Utc>>,
    /// Whether the session has been recorded to wdttg.
    pub recorded: bool,
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
    /// Record a session in wdttg with start = period_start (backdated).
    RecordSession {
        workspace_id: String,
        task_name: String,
        start: DateTime<Utc>,
    },
    CloseSession {
        session_id: i64,
        end_time: DateTime<Utc>,
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

    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
    }

    /// Called when the daemon detects a wall-clock jump between poll ticks
    /// (machine suspend/resume). Treats every focused workspace as if the
    /// user left at `at` (the last tick before the gap), so the normal
    /// bridge-or-close hysteresis bounds the session instead of spanning the
    /// suspension.
    pub fn handle_time_jump(&mut self, at: DateTime<Utc>) {
        for s in self.sessions.values_mut() {
            if s.leave_at.is_none() {
                s.leave_at = Some(at);
            }
        }
    }

    pub fn clear_sessions(&mut self) {
        self.sessions.clear();
        self.pending_rename = None;
    }

    /// Sweep all workspaces against the current herdr state.
    /// Returns commands to execute against wdttg.
    pub fn sweep(
        &mut self,
        workspaces: &[WorkspaceInfo],
        focused_id: Option<&str>,
        now: DateTime<Utc>,
        derive_name: impl Fn(&str, Option<&str>) -> String,
    ) -> Vec<Cmd> {
        let mut cmds = Vec::new();

        if self.paused {
            return cmds;
        }

        // 1. Handle rename timeout (auto-resolve to "continue").
        self.process_rename_timeout(now, &mut cmds, &derive_name);

        // 2. Insert new focused workspace (in-memory only — nothing is written
        //    to wdttg until the span reaches the threshold).
        if let Some(f) = focused_id {
            if !self.sessions.contains_key(f) {
                if let Some(ws) = workspaces.iter().find(|w| w.id == f) {
                    let name = derive_name(&ws.label, ws.cwd.as_deref());
                    self.sessions.insert(
                        f.to_string(),
                        WorkspaceSession {
                            task_id: -1,
                            task_name: name,
                            last_seen_label: ws.label.clone(),
                            session_id: None,
                            period_start: now,
                            last_focused_at: now,
                            leave_at: None,
                            recorded: false,
                        },
                    );
                }
            }
        }

        // 3. Process each tracked workspace.
        let known_ids: Vec<String> = self.sessions.keys().cloned().collect();
        for ws_id in known_ids {
            let ws = workspaces.iter().find(|w| w.id == ws_id).cloned();

            // Workspace closed: close any recorded session, discard.
            let Some(ws) = ws else {
                if let Some(s) = self.sessions.remove(&ws_id) {
                    if let Some(sid) = s.session_id {
                        let end = s.leave_at.unwrap_or(now);
                        cmds.push(Cmd::CloseSession {
                            session_id: sid,
                            end_time: end,
                        });
                    }
                }
                continue;
            };

            let is_focused = focused_id.is_some_and(|f| f == ws_id);

            // Rename detection.
            let (last_label, session_id) = {
                let s = &self.sessions[&ws_id];
                (s.last_seen_label.clone(), s.session_id)
            };
            if ws.label != last_label {
                if self.pending_rename.is_none() {
                    self.pending_rename = Some(PendingRename {
                        workspace_id: ws_id.clone(),
                        old_label: last_label.clone(),
                        new_label: ws.label.clone(),
                        prompted_at: now,
                    });
                }
                self.sessions.get_mut(&ws_id).unwrap().last_seen_label = ws.label.clone();
            }

            // Focus transition.
            let leave_at = self.sessions.get(&ws_id).unwrap().leave_at;
            if is_focused {
                if let Some(la) = leave_at {
                    // Re-focused after being blurred.
                    if now - la <= self.threshold {
                        // Bridge: extend the period across the gap.
                        let s = self.sessions.get_mut(&ws_id).unwrap();
                        s.leave_at = None;
                        s.last_focused_at = now;
                        // Recorded session stays open. No command.
                    } else {
                        // Beyond threshold: close recorded session, start new period.
                        if let Some(sid) = session_id {
                            cmds.push(Cmd::CloseSession {
                                session_id: sid,
                                end_time: la,
                            });
                        }
                        let name = derive_name(&ws.label, ws.cwd.as_deref());
                        let s = self.sessions.get_mut(&ws_id).unwrap();
                        s.period_start = now;
                        s.last_focused_at = now;
                        s.leave_at = None;
                        s.session_id = None;
                        s.recorded = false;
                        s.task_id = -1;
                        s.task_name = name;
                    }
                } else {
                    // Already focused: refresh last_focused_at.
                    self.sessions.get_mut(&ws_id).unwrap().last_focused_at = now;
                }
            } else {
                // Blurred: record leave_at if not already recorded.
                let s = self.sessions.get_mut(&ws_id).unwrap();
                if s.leave_at.is_none() {
                    s.leave_at = Some(now);
                }
            }

            // Expired pending session: close recorded session, discard.
            let (leave_at, session_id) = {
                let s = &self.sessions[&ws_id];
                (s.leave_at, s.session_id)
            };
            if let Some(la) = leave_at {
                if now - la > self.threshold {
                    if let Some(sid) = session_id {
                        cmds.push(Cmd::CloseSession {
                            session_id: sid,
                            end_time: la,
                        });
                    }
                    self.sessions.remove(&ws_id);
                    continue;
                }
            }

            // Recording trigger: a workspace that has lasted >= threshold is
            // written to wdttg (backdated to period_start).
            let (period_start, last_focused_at, recorded, leave_at) = {
                let s = &self.sessions[&ws_id];
                (s.period_start, s.last_focused_at, s.recorded, s.leave_at)
            };
            if !recorded {
                let span = match leave_at {
                    Some(_) => last_focused_at - period_start,
                    None => now - period_start,
                };
                if span >= self.threshold {
                    let name = derive_name(&ws.label, ws.cwd.as_deref());
                    cmds.push(Cmd::RecordSession {
                        workspace_id: ws_id.clone(),
                        task_name: name,
                        start: period_start,
                    });
                    self.sessions.get_mut(&ws_id).unwrap().recorded = true;
                }
            }
        }

        cmds
    }

    fn process_rename_timeout(
        &mut self,
        now: DateTime<Utc>,
        cmds: &mut Vec<Cmd>,
        derive_name: &impl Fn(&str, Option<&str>) -> String,
    ) {
        if let Some(pr) = &self.pending_rename {
            if now - pr.prompted_at > self.rename_timeout {
                let ws_id = pr.workspace_id.clone();
                let new_label = pr.new_label.clone();
                self.pending_rename = None;
                let new_name = derive_name(&new_label, None);
                if let Some(s) = self.sessions.get_mut(&ws_id) {
                    if s.task_id > 0 {
                        cmds.push(Cmd::RenameTask {
                            task_id: s.task_id,
                            new_name: new_name.clone(),
                        });
                    }
                    s.task_name = new_name;
                    s.last_seen_label = new_label;
                }
            }
        }
    }

    pub fn resolve_rename(
        &mut self,
        decision: RenameDecision,
        new_name: String,
        now: DateTime<Utc>,
    ) -> Vec<Cmd> {
        let Some(pr) = self.pending_rename.take() else {
            return Vec::new();
        };
        let mut cmds = Vec::new();
        match decision {
            RenameDecision::Continue => {
                if let Some(s) = self.sessions.get_mut(&pr.workspace_id) {
                    if s.task_id > 0 {
                        cmds.push(Cmd::RenameTask {
                            task_id: s.task_id,
                            new_name: new_name.clone(),
                        });
                    }
                    s.task_name = new_name;
                    s.last_seen_label = pr.new_label.clone();
                }
            }
            RenameDecision::NewTask => {
                if let Some(s) = self.sessions.get(&pr.workspace_id) {
                    let session_id = s.session_id;
                    let leave_at = s.leave_at;
                    if let Some(sid) = session_id {
                        let end = leave_at.unwrap_or(now);
                        cmds.push(Cmd::CloseSession {
                            session_id: sid,
                            end_time: end,
                        });
                    }
                    if let Some(s) = self.sessions.get_mut(&pr.workspace_id) {
                        s.task_id = -1;
                        s.task_name = new_name.clone();
                        s.last_seen_label = pr.new_label.clone();
                        s.session_id = None;
                        s.period_start = now;
                        s.last_focused_at = now;
                        s.leave_at = None;
                        s.recorded = false;
                    }
                }
            }
        }
        cmds
    }

    pub fn status_line(&self, now: DateTime<Utc>) -> String {
        if self.paused {
            return "⏸ wdttg paused".to_string();
        }
        // The focused workspace is the one with leave_at = None.
        for s in self.sessions.values() {
            if s.leave_at.is_none() {
                let elapsed = now - s.period_start;
                return format!("⏱ {} {}", s.task_name, format_duration(elapsed));
            }
        }
        "○ wdttg idle".to_string()
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

    fn ts(h: u32, m: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 22, h, m, s).unwrap()
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
    fn focus_does_not_write_to_wdttg_immediately() {
        let mut t = Tracker::new(TimeDelta::minutes(3), TimeDelta::seconds(60));
        let cmds = t.sweep(&[ws("a", "a")], Some("a"), ts(9, 0, 0), derive);
        assert!(cmds.is_empty(), "no commands until span reaches threshold");
        assert!(t.sessions.contains_key("a"));
        assert!(!t.sessions["a"].recorded);
    }

    #[test]
    fn workspace_recorded_after_threshold_with_backdated_start() {
        let mut t = Tracker::new(TimeDelta::minutes(3), TimeDelta::seconds(60));
        // Focus at 9:00.
        t.sweep(&[ws("a", "a")], Some("a"), ts(9, 0, 0), derive);
        // Poll at 9:03 (3 minutes later) — span reaches threshold.
        let cmds = t.sweep(&[ws("a", "a")], Some("a"), ts(9, 3, 0), derive);
        assert!(
            cmds.iter().any(|c| matches!(c, Cmd::RecordSession { task_name, start, .. } if task_name == "herdr:a" && *start == ts(9, 0, 0))),
            "expected RecordSession backdated to 9:00, got {:?}",
            cmds
        );
        assert!(t.sessions["a"].recorded);
    }

    #[test]
    fn brief_visit_never_recorded() {
        let mut t = Tracker::new(TimeDelta::minutes(3), TimeDelta::seconds(60));
        // Focus A at 9:00.
        t.sweep(&[ws("a", "a")], Some("a"), ts(9, 0, 0), derive);
        // Switch to B at 9:01 (A blurred).
        t.sweep(&[ws("a", "a"), ws("b", "b")], Some("b"), ts(9, 1, 0), derive);
        // Poll at 9:05 (A's bridge expired at 9:04, span was 1 min < 3 min).
        let cmds = t.sweep(&[ws("a", "a"), ws("b", "b")], Some("b"), ts(9, 5, 0), derive);
        // A was never recorded; no RecordSession for A.
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::RecordSession { task_name, .. } if task_name == "herdr:a")),
            "briefly-visited workspace should not be recorded, got {:?}",
            cmds
        );
        assert!(!t.sessions.contains_key("a"), "A should be discarded after expiry");
    }

    #[test]
    fn bridge_across_gap_then_recorded_with_original_start() {
        let mut t = Tracker::new(TimeDelta::minutes(3), TimeDelta::seconds(60));
        // A at 9:00.
        t.sweep(&[ws("a", "a")], Some("a"), ts(9, 0, 0), derive);
        // B at 9:01.
        t.sweep(&[ws("a", "a"), ws("b", "b")], Some("b"), ts(9, 1, 0), derive);
        // Back to A at 9:02 (within threshold).
        t.sweep(&[ws("a", "a"), ws("b", "b")], Some("a"), ts(9, 2, 0), derive);
        // Poll at 9:03: A is focused, span = 3 min >= threshold.
        let cmds = t.sweep(&[ws("a", "a"), ws("b", "b")], Some("a"), ts(9, 3, 0), derive);
        assert!(
            cmds.iter().any(|c| matches!(c, Cmd::RecordSession { task_name, start, .. } if task_name == "herdr:a" && *start == ts(9, 0, 0))),
            "A should be recorded backdated to 9:00, got {:?}",
            cmds
        );
        // B should never be recorded (span 1 min).
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::RecordSession { task_name, .. } if task_name == "herdr:b")),
            "briefly-visited B should not be recorded, got {:?}",
            cmds
        );
    }

    #[test]
    fn recorded_session_closed_at_leave_time_when_bridge_expires() {
        let mut t = Tracker::new(TimeDelta::minutes(3), TimeDelta::seconds(60));
        // A at 9:00, stays focused.
        t.sweep(&[ws("a", "a")], Some("a"), ts(9, 0, 0), derive);
        // Poll at 9:03: recorded.
        t.sweep(&[ws("a", "a")], Some("a"), ts(9, 3, 0), derive);
        // Simulate the executor: record the session.
        t.sessions.get_mut("a").unwrap().session_id = Some(10);
        t.sessions.get_mut("a").unwrap().task_id = 1;
        // Switch to B at 9:04 (A blurred).
        t.sweep(&[ws("a", "a"), ws("b", "b")], Some("b"), ts(9, 4, 0), derive);
        // Poll at 9:08 (A's bridge expired at 9:07).
        let cmds = t.sweep(&[ws("a", "a"), ws("b", "b")], Some("b"), ts(9, 8, 0), derive);
        assert!(
            cmds.iter().any(|c| matches!(c, Cmd::CloseSession { session_id, end_time } if *session_id == 10 && *end_time == ts(9, 4, 0))),
            "expected CloseSession at leave time, got {:?}",
            cmds
        );
        assert!(!t.sessions.contains_key("a"));
    }

    #[test]
    fn recorded_session_stays_open_when_bridged() {
        let mut t = Tracker::new(TimeDelta::minutes(3), TimeDelta::seconds(60));
        t.sweep(&[ws("a", "a")], Some("a"), ts(9, 0, 0), derive);
        t.sweep(&[ws("a", "a")], Some("a"), ts(9, 3, 0), derive);
        t.sessions.get_mut("a").unwrap().session_id = Some(10);
        // Blur at 9:04, re-focus at 9:05 (within threshold).
        t.sweep(&[ws("a", "a"), ws("b", "b")], Some("b"), ts(9, 4, 0), derive);
        let cmds = t.sweep(&[ws("a", "a"), ws("b", "b")], Some("a"), ts(9, 5, 0), derive);
        // No CloseSession; the recorded session stays open.
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::CloseSession { .. })),
            "expected no CloseSession when bridged, got {:?}",
            cmds
        );
        assert!(t.sessions.contains_key("a"));
        assert!(t.sessions["a"].recorded);
    }

    #[test]
    fn workspace_closed_closes_recorded_session() {
        let mut t = Tracker::new(TimeDelta::minutes(3), TimeDelta::seconds(60));
        t.sweep(&[ws("a", "a")], Some("a"), ts(9, 0, 0), derive);
        t.sweep(&[ws("a", "a")], Some("a"), ts(9, 3, 0), derive);
        t.sessions.get_mut("a").unwrap().session_id = Some(10);
        // Workspace a disappears.
        let cmds = t.sweep(&[], Some("a"), ts(9, 4, 0), derive);
        assert!(
            cmds.iter().any(|c| matches!(c, Cmd::CloseSession { session_id, end_time } if *session_id == 10 && *end_time == ts(9, 4, 0))),
            "expected CloseSession at now, got {:?}",
            cmds
        );
    }

    #[test]
    fn parallel_workspaces_both_tracked() {
        let mut t = Tracker::new(TimeDelta::minutes(3), TimeDelta::seconds(60));
        t.sweep(&[ws("a", "a")], Some("a"), ts(9, 0, 0), derive);
        t.sweep(&[ws("a", "a"), ws("b", "b")], Some("b"), ts(9, 1, 0), derive);
        assert!(t.sessions.contains_key("a"));
        assert!(t.sessions.contains_key("b"));
        assert!(t.sessions["a"].leave_at.is_some());
        assert!(t.sessions["b"].leave_at.is_none());
    }

    #[test]
    fn rename_detected_when_label_changes() {
        let mut t = Tracker::new(TimeDelta::minutes(3), TimeDelta::seconds(60));
        t.sweep(&[ws("a", "old")], Some("a"), ts(9, 0, 0), derive);
        t.sweep(&[ws("a", "new")], Some("a"), ts(9, 1, 0), derive);
        assert!(t.pending_rename.is_some());
        let pr = t.pending_rename.as_ref().unwrap();
        assert_eq!(pr.old_label, "old");
        assert_eq!(pr.new_label, "new");
    }

    #[test]
    fn rename_continue_renames_task() {
        let mut t = Tracker::new(TimeDelta::minutes(3), TimeDelta::seconds(60));
        t.sweep(&[ws("a", "old")], Some("a"), ts(9, 0, 0), derive);
        // Simulate executor recording the session.
        t.sessions.get_mut("a").unwrap().task_id = 1;
        t.sessions.get_mut("a").unwrap().session_id = Some(10);
        t.sessions.get_mut("a").unwrap().recorded = true;
        // Label changes.
        t.sweep(&[ws("a", "new")], Some("a"), ts(9, 1, 0), derive);
        let cmds = t.resolve_rename(RenameDecision::Continue, "herdr:new".to_string(), ts(9, 1, 0));
        assert!(
            cmds.iter().any(|c| matches!(c, Cmd::RenameTask { task_id, new_name } if *task_id == 1 && new_name == "herdr:new")),
            "expected RenameTask, got {:?}",
            cmds
        );
        assert_eq!(t.sessions["a"].task_name, "herdr:new");
        assert!(t.pending_rename.is_none());
    }

    #[test]
    fn rename_new_task_closes_and_starts_fresh() {
        let mut t = Tracker::new(TimeDelta::minutes(3), TimeDelta::seconds(60));
        t.sweep(&[ws("a", "old")], Some("a"), ts(9, 0, 0), derive);
        t.sessions.get_mut("a").unwrap().task_id = 1;
        t.sessions.get_mut("a").unwrap().session_id = Some(10);
        t.sessions.get_mut("a").unwrap().recorded = true;
        t.sweep(&[ws("a", "new")], Some("a"), ts(9, 1, 0), derive);
        let cmds = t.resolve_rename(RenameDecision::NewTask, "herdr:new".to_string(), ts(9, 1, 0));
        assert!(
            cmds.iter().any(|c| matches!(c, Cmd::CloseSession { session_id, .. } if *session_id == 10)),
            "expected CloseSession, got {:?}",
            cmds
        );
        assert!(!t.sessions["a"].recorded);
        assert_eq!(t.sessions["a"].task_name, "herdr:new");
    }

    #[test]
    fn status_line_shows_focused_task() {
        let mut t = Tracker::new(TimeDelta::minutes(3), TimeDelta::seconds(60));
        t.sweep(&[ws("a", "a")], Some("a"), ts(9, 0, 0), derive);
        let line = t.status_line(ts(10, 23, 0));
        assert!(line.contains("herdr:a"), "got: {}", line);
        assert!(line.contains("1h 23m"), "got: {}", line);
    }

    #[test]
    fn status_line_shows_paused() {
        let mut t = Tracker::new(TimeDelta::minutes(3), TimeDelta::seconds(60));
        t.set_paused(true);
        let line = t.status_line(ts(9, 0, 0));
        assert!(line.contains("paused"), "got: {}", line);
    }

    #[test]
    fn time_jump_causes_close_at_last_tick_not_resume_time() {
        let mut t = Tracker::new(TimeDelta::minutes(3), TimeDelta::seconds(60));
        // User works on A 9:00-9:03 (recorded).
        t.sweep(&[ws("a", "a")], Some("a"), ts(9, 0, 0), derive);
        t.sweep(&[ws("a", "a")], Some("a"), ts(9, 3, 0), derive);
        t.sessions.get_mut("a").unwrap().session_id = Some(10);
        // Machine suspends at 9:05 (last tick), resumes at 17:00.
        t.sweep(&[ws("a", "a")], Some("a"), ts(9, 5, 0), derive);
        t.handle_time_jump(ts(9, 5, 0));
        // After resume, the same workspace is still focused but the gap far
        // exceeds the threshold: session closes at 9:05, new period at 17:00.
        let cmds = t.sweep(&[ws("a", "a")], Some("a"), ts(17, 0, 0), derive);
        assert!(
            cmds.iter().any(|c| matches!(c, Cmd::CloseSession { session_id, end_time } if *session_id == 10 && *end_time == ts(9, 5, 0))),
            "expected CloseSession at suspend time, got {:?}",
            cmds
        );
        // The new period is unrecorded until it lasts the threshold.
        assert!(!t.sessions["a"].recorded);
        assert_eq!(t.sessions["a"].period_start, ts(17, 0, 0));
    }

    #[test]
    fn time_jump_within_threshold_after_resume_bridges() {
        let mut t = Tracker::new(TimeDelta::minutes(3), TimeDelta::seconds(60));
        t.sweep(&[ws("a", "a")], Some("a"), ts(9, 0, 0), derive);
        t.sweep(&[ws("a", "a")], Some("a"), ts(9, 3, 0), derive);
        t.sessions.get_mut("a").unwrap().session_id = Some(10);
        // Short suspend: last tick 9:05, resume 9:06 (within threshold).
        t.sweep(&[ws("a", "a")], Some("a"), ts(9, 5, 0), derive);
        t.handle_time_jump(ts(9, 5, 0));
        let cmds = t.sweep(&[ws("a", "a")], Some("a"), ts(9, 6, 0), derive);
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::CloseSession { .. })),
            "short suspend should bridge, got {:?}",
            cmds
        );
        assert!(t.sessions["a"].recorded);
    }

    #[test]
    fn time_jump_discards_unrecorded_brief_period() {
        let mut t = Tracker::new(TimeDelta::minutes(3), TimeDelta::seconds(60));
        // User focuses A at 9:00, suspends at 9:01 (below threshold, unrecorded).
        t.sweep(&[ws("a", "a")], Some("a"), ts(9, 0, 0), derive);
        t.handle_time_jump(ts(9, 1, 0));
        // Resumes at 15:00 on a different workspace.
        let cmds = t.sweep(&[ws("a", "a"), ws("b", "b")], Some("b"), ts(15, 0, 0), derive);
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::RecordSession { task_name, .. } if task_name == "herdr:a")),
            "brief pre-suspend period must not be recorded, got {:?}",
            cmds
        );
        assert!(!t.sessions.contains_key("a"), "A discarded after expiry");
    }
}
