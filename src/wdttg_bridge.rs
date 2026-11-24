use chrono::{DateTime, Utc};
use std::sync::Arc;
use wdttg::{Database, SessionService, TaskService};

pub struct WdttgBridge {
    db: Arc<Database>,
    pub sessions: SessionService,
    pub tasks: TaskService,
}

impl WdttgBridge {
    pub fn open() -> Result<Self, wdttg::DbError> {
        let db = Arc::new(Database::open()?);
        Ok(Self {
            sessions: SessionService::new(Arc::clone(&db)),
            tasks: TaskService::new(Arc::clone(&db)),
            db,
        })
    }

    pub fn get_or_create_task(&self, name: &str) -> Result<i64, wdttg::TaskServiceError> {
        Ok(self.tasks.get_or_create(name)?.id)
    }

    pub fn rename_task(&self, task_id: i64, new_name: &str) -> Result<(), wdttg::TaskServiceError> {
        self.tasks.rename_task(task_id, new_name)
    }

    /// Record a session with a backdated start (used when a workspace is
    /// recorded after it has been active for >= threshold).
    pub fn record_session(
        &self,
        task_id: i64,
        start: DateTime<Utc>,
    ) -> Result<i64, wdttg::SessionServiceError> {
        Ok(self.sessions.log_session(task_id, start, None, None)?.id)
    }

    pub fn close_session(
        &self,
        session_id: i64,
        end_time: DateTime<Utc>,
    ) -> Result<(), wdttg::SessionServiceError> {
        let session = self
            .sessions
            .get_by_id(session_id)?
            .ok_or(wdttg::SessionServiceError::NotFound(session_id))?;
        self.sessions
            .update_full(
                session_id,
                session.start_time,
                Some(end_time),
                None,
                None,
                None,
                None,
            )?;
        Ok(())
    }

    /// Find an open (end_time IS NULL) session on a task by exact name.
    /// Returns (session_id, start_time, task_id).
    pub fn find_open_session_by_task(
        &self,
        task_name: &str,
    ) -> Result<Option<(i64, DateTime<Utc>, i64)>, wdttg::SessionServiceError> {
        let ids: Vec<i64> = {
            let conn = self.db.connection();
            let mut stmt = conn.prepare(
                "SELECT s.id FROM sessions s
                 JOIN tasks t ON s.task_id = t.id
                 WHERE s.end_time IS NULL AND t.name = ?1
                 ORDER BY s.start_time DESC",
            )?;
            let ids = stmt
                .query_map([&task_name], |row| row.get(0))?
                .collect::<Result<Vec<_>, _>>()?;
            ids
        };

        let Some(id) = ids.first() else {
            return Ok(None);
        };
        let session = self
            .sessions
            .get_by_id(*id)?
            .ok_or(wdttg::SessionServiceError::NotFound(*id))?;
        Ok(Some((*id, session.start_time, session.task_id)))
    }

    /// Reconcile on startup: find open sessions (end_time IS NULL) on
    /// herdr-prefixed tasks and close them at `close_at` (the last heartbeat
    /// time, bounding sessions at when tracking actually stopped). Skips
    /// `except_id` (an adopted session). Returns the count closed.
    pub fn reconcile_orphans(
        &self,
        close_at: DateTime<Utc>,
        except_id: Option<i64>,
    ) -> Result<usize, wdttg::SessionServiceError> {
        let ids: Vec<i64> = {
            let conn = self.db.connection();
            let mut stmt = conn.prepare(
                "SELECT s.id FROM sessions s
                 JOIN tasks t ON s.task_id = t.id
                 WHERE s.end_time IS NULL AND t.name LIKE 'herdr:%'",
            )?;
            let ids = stmt
                .query_map([], |row| row.get(0))?
                .collect::<Result<Vec<_>, _>>()?;
            ids
        };

        let mut closed = 0;
        for id in &ids {
            if Some(*id) == except_id {
                continue;
            }
            self.close_session(*id, close_at)?;
            closed += 1;
        }
        Ok(closed)
    }
}
