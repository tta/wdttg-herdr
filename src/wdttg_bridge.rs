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

    pub fn start_session(&self, task_id: i64) -> Result<i64, wdttg::SessionServiceError> {
        Ok(self.sessions.start(task_id, None)?.id)
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

    pub fn extend_session(
        &self,
        session_id: i64,
        new_end: DateTime<Utc>,
    ) -> Result<(), wdttg::SessionServiceError> {
        self.sessions.extend_session(session_id, new_end)?;
        Ok(())
    }

    /// Reconcile on startup: find open sessions (end_time IS NULL) on
    /// herdr-prefixed tasks and close them at `now`. Returns the count closed.
    pub fn reconcile_orphans(&self, now: DateTime<Utc>) -> Result<usize, wdttg::SessionServiceError> {
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

        for id in &ids {
            self.close_session(*id, now)?;
        }
        Ok(ids.len())
    }
}
