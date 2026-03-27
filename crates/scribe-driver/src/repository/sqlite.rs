//! `SQLite`-backed implementation of [`TaskRepository`].

use std::sync::Mutex;

use rusqlite::Connection;

use super::{DriverStats, ProjectRecord, TaskMetrics, TaskRecord, TaskRepository};

/// `SQLite` implementation of [`TaskRepository`].
pub struct SqliteTaskRepository {
    conn: Mutex<Connection>,
}

impl SqliteTaskRepository {
    /// Open (or create) the driver database and initialise the schema.
    pub fn open() -> Result<Self, String> {
        let db_path = db_path()?;

        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("failed to create db dir: {e}"))?;
        }

        let conn = Connection::open(&db_path)
            .map_err(|e| format!("failed to open SQLite database: {e}"))?;

        create_schema(&conn)?;

        Ok(Self { conn: Mutex::new(conn) })
    }
}

/// Resolve the database path: `$XDG_DATA_HOME/scribe/driver.db`.
fn db_path() -> Result<std::path::PathBuf, String> {
    dirs::data_dir()
        .map(|d| d.join("scribe").join("driver.db"))
        .ok_or_else(|| String::from("no XDG data directory available"))
}

/// Create the database schema if it does not already exist.
fn create_schema(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        "
		CREATE TABLE IF NOT EXISTS tasks (
			id            TEXT    PRIMARY KEY,
			project_path  TEXT    NOT NULL,
			description   TEXT    NOT NULL,
			state         TEXT    NOT NULL,
			worktree_path TEXT,
			created_at    INTEGER NOT NULL,
			completed_at  INTEGER,
			exit_code     INTEGER
		);
		CREATE TABLE IF NOT EXISTS task_output (
			task_id   TEXT    NOT NULL,
			seq       INTEGER NOT NULL,
			chunk     TEXT    NOT NULL,
			timestamp INTEGER NOT NULL,
			PRIMARY KEY (task_id, seq)
		);
		CREATE TABLE IF NOT EXISTS task_metrics (
			task_id         TEXT    PRIMARY KEY,
			tokens_used     INTEGER NOT NULL DEFAULT 0,
			files_changed   INTEGER NOT NULL DEFAULT 0,
			cost_usd        REAL    NOT NULL DEFAULT 0.0,
			waves_completed INTEGER NOT NULL DEFAULT 0
		);
		CREATE TABLE IF NOT EXISTS projects (
			path       TEXT    PRIMARY KEY,
			name       TEXT    NOT NULL,
			created_at INTEGER NOT NULL
		);
		",
    )
    .map_err(|e| format!("failed to create schema: {e}"))
}

/// Convert a `rusqlite::Error` to a `String`.
fn db_err(context: &str, e: &rusqlite::Error) -> String {
    format!("{context}: {e}")
}

/// Get current Unix timestamp in seconds as `i64`.
fn unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().cast_signed())
        .unwrap_or(0_i64)
}

/// Get current Unix timestamp in milliseconds as `i64`.
fn unix_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_millis()).ok())
        .unwrap_or(0_i64)
}

impl TaskRepository for SqliteTaskRepository {
    fn create_task(&self, record: &TaskRecord) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| format!("db lock poisoned: {e}"))?;
        conn.execute(
            "INSERT INTO tasks (id, project_path, description, state, worktree_path, \
			 created_at, completed_at, exit_code) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            rusqlite::params![
                record.id,
                record.project_path,
                record.description,
                record.state,
                record.worktree_path,
                record.created_at,
                record.completed_at,
                record.exit_code,
            ],
        )
        .map_err(|e| db_err("create_task failed", &e))?;
        Ok(())
    }

    fn update_task_state(&self, task_id: &str, state: &str) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| format!("db lock poisoned: {e}"))?;
        conn.execute(
            "UPDATE tasks SET state = ?1 WHERE id = ?2",
            rusqlite::params![state, task_id],
        )
        .map_err(|e| db_err("update_task_state failed", &e))?;
        Ok(())
    }

    fn complete_task(&self, task_id: &str, exit_code: Option<i32>) -> Result<(), String> {
        let now = unix_secs();
        let state = if exit_code == Some(0) { "Completed" } else { "Failed" };
        let conn = self.conn.lock().map_err(|e| format!("db lock poisoned: {e}"))?;
        conn.execute(
            "UPDATE tasks SET state=?1, completed_at=?2, exit_code=?3 WHERE id=?4",
            rusqlite::params![state, now, exit_code, task_id],
        )
        .map_err(|e| db_err("complete_task failed", &e))?;
        Ok(())
    }

    fn list_tasks(&self) -> Result<Vec<TaskRecord>, String> {
        let conn = self.conn.lock().map_err(|e| format!("db lock poisoned: {e}"))?;
        let mut stmt = conn
            .prepare(
                "SELECT id, project_path, description, state, worktree_path, created_at, \
				 completed_at, exit_code FROM tasks ORDER BY created_at DESC",
            )
            .map_err(|e| db_err("list_tasks prepare failed", &e))?;

        stmt.query_map([], row_to_task_record)
            .map_err(|e| db_err("list_tasks query failed", &e))?
            .map(|r| r.map_err(|e| db_err("list_tasks row error", &e)))
            .collect::<Result<Vec<_>, _>>()
    }

    fn get_task(&self, task_id: &str) -> Result<Option<TaskRecord>, String> {
        let conn = self.conn.lock().map_err(|e| format!("db lock poisoned: {e}"))?;
        let mut stmt = conn
            .prepare(
                "SELECT id, project_path, description, state, worktree_path, created_at, \
				 completed_at, exit_code FROM tasks WHERE id = ?1",
            )
            .map_err(|e| db_err("get_task prepare failed", &e))?;

        stmt.query_map(rusqlite::params![task_id], row_to_task_record)
            .map_err(|e| db_err("get_task query failed", &e))?
            .next()
            .transpose()
            .map_err(|e| db_err("get_task row error", &e))
    }

    fn append_output(&self, task_id: &str, chunk: &str) -> Result<(), String> {
        let now_ms = unix_millis();
        let conn = self.conn.lock().map_err(|e| format!("db lock poisoned: {e}"))?;

        let seq: i64 = conn
            .query_row(
                "SELECT COALESCE(MAX(seq), -1) + 1 FROM task_output WHERE task_id = ?1",
                rusqlite::params![task_id],
                get_i64,
            )
            .map_err(|e| db_err("append_output seq query failed", &e))?;

        conn.execute(
            "INSERT INTO task_output (task_id, seq, chunk, timestamp) VALUES (?1,?2,?3,?4)",
            rusqlite::params![task_id, seq, chunk, now_ms],
        )
        .map_err(|e| db_err("append_output insert failed", &e))?;
        Ok(())
    }

    fn get_output(&self, task_id: &str) -> Result<String, String> {
        let conn = self.conn.lock().map_err(|e| format!("db lock poisoned: {e}"))?;
        let mut stmt = conn
            .prepare("SELECT chunk FROM task_output WHERE task_id = ?1 ORDER BY seq ASC")
            .map_err(|e| db_err("get_output prepare failed", &e))?;

        let chunks: Vec<String> = stmt
            .query_map(rusqlite::params![task_id], get_string)
            .map_err(|e| db_err("get_output query failed", &e))?
            .map(|r| r.map_err(|e| db_err("get_output row error", &e)))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(chunks.concat())
    }

    fn update_metrics(&self, task_id: &str, metrics: &TaskMetrics) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| format!("db lock poisoned: {e}"))?;
        conn.execute(
            "INSERT OR REPLACE INTO task_metrics \
			 (task_id, tokens_used, files_changed, cost_usd, waves_completed) \
			 VALUES (?1,?2,?3,?4,?5)",
            rusqlite::params![
                task_id,
                metrics.tokens_used.cast_signed(),
                metrics.files_changed.cast_signed(),
                metrics.cost_usd,
                metrics.waves_completed.cast_signed(),
            ],
        )
        .map_err(|e| db_err("update_metrics failed", &e))?;
        Ok(())
    }

    fn get_metrics(&self, task_id: &str) -> Result<Option<TaskMetrics>, String> {
        let conn = self.conn.lock().map_err(|e| format!("db lock poisoned: {e}"))?;
        let mut stmt = conn
            .prepare(
                "SELECT task_id, tokens_used, files_changed, cost_usd, waves_completed \
				 FROM task_metrics WHERE task_id = ?1",
            )
            .map_err(|e| db_err("get_metrics prepare failed", &e))?;

        stmt.query_map(rusqlite::params![task_id], row_to_task_metrics)
            .map_err(|e| db_err("get_metrics query failed", &e))?
            .next()
            .transpose()
            .map_err(|e| db_err("get_metrics row error", &e))
    }

    #[allow(
        clippy::too_many_lines,
        reason = "four sequential COUNT queries cannot reasonably be split further"
    )]
    fn get_stats(&self) -> Result<DriverStats, String> {
        let conn = self.conn.lock().map_err(|e| format!("db lock poisoned: {e}"))?;

        let running: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tasks \
				 WHERE state IN ('Starting','Running','WaitingForInput','PermissionPrompt')",
                [],
                get_i64,
            )
            .map_err(|e| db_err("get_stats running count failed", &e))?;

        let completed: i64 = conn
            .query_row("SELECT COUNT(*) FROM tasks WHERE state = 'Completed'", [], get_i64)
            .map_err(|e| db_err("get_stats completed count failed", &e))?;

        let failed: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tasks WHERE state IN ('Failed','Stopped')",
                [],
                get_i64,
            )
            .map_err(|e| db_err("get_stats failed count failed", &e))?;

        let total_tokens: i64 = conn
            .query_row("SELECT COALESCE(SUM(tokens_used), 0) FROM task_metrics", [], get_i64)
            .map_err(|e| db_err("get_stats total_tokens failed", &e))?;

        Ok(DriverStats {
            running: usize::try_from(running).unwrap_or(0),
            completed: usize::try_from(completed).unwrap_or(0),
            failed: usize::try_from(failed).unwrap_or(0),
            total_tokens: u64::try_from(total_tokens).unwrap_or(0),
        })
    }

    fn add_project(&self, path: &str, name: &str) -> Result<(), String> {
        let now = unix_secs();
        let conn = self.conn.lock().map_err(|e| format!("db lock poisoned: {e}"))?;
        conn.execute(
            "INSERT OR IGNORE INTO projects (path, name, created_at) VALUES (?1,?2,?3)",
            rusqlite::params![path, name, now],
        )
        .map_err(|e| db_err("add_project failed", &e))?;
        Ok(())
    }

    fn remove_project(&self, path: &str) -> Result<(), String> {
        let conn = self.conn.lock().map_err(|e| format!("db lock poisoned: {e}"))?;
        conn.execute("DELETE FROM projects WHERE path = ?1", rusqlite::params![path])
            .map_err(|e| db_err("remove_project failed", &e))?;
        Ok(())
    }

    fn list_projects(&self) -> Result<Vec<ProjectRecord>, String> {
        let conn = self.conn.lock().map_err(|e| format!("db lock poisoned: {e}"))?;
        let mut stmt = conn
            .prepare("SELECT path, name FROM projects ORDER BY name ASC")
            .map_err(|e| db_err("list_projects prepare failed", &e))?;

        stmt.query_map([], row_to_project_record)
            .map_err(|e| db_err("list_projects query failed", &e))?
            .map(|r| r.map_err(|e| db_err("list_projects row error", &e)))
            .collect::<Result<Vec<_>, _>>()
    }
}

/// Row mapper for a single `i64` column.
#[allow(
    clippy::result_large_err,
    reason = "rusqlite::Error is inherently large; required by the rusqlite query_row API"
)]
fn get_i64(row: &rusqlite::Row<'_>) -> rusqlite::Result<i64> {
    row.get(0)
}

/// Row mapper for a single `String` column.
#[allow(
    clippy::result_large_err,
    reason = "rusqlite::Error is inherently large; required by the rusqlite query_map API"
)]
fn get_string(row: &rusqlite::Row<'_>) -> rusqlite::Result<String> {
    row.get(0)
}

/// Map a database row to a [`TaskRecord`].
#[allow(
    clippy::result_large_err,
    reason = "rusqlite::Error is inherently large; required by the rusqlite query_map API"
)]
fn row_to_task_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskRecord> {
    Ok(TaskRecord {
        id: row.get(0)?,
        project_path: row.get(1)?,
        description: row.get(2)?,
        state: row.get(3)?,
        worktree_path: row.get(4)?,
        created_at: row.get(5)?,
        completed_at: row.get(6)?,
        exit_code: row.get(7)?,
    })
}

/// Map a database row to a [`TaskMetrics`].
#[allow(
    clippy::result_large_err,
    reason = "rusqlite::Error is inherently large; required by the rusqlite query_map API"
)]
fn row_to_task_metrics(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskMetrics> {
    Ok(TaskMetrics {
        task_id: row.get(0)?,
        tokens_used: u64::try_from(row.get::<_, i64>(1)?).unwrap_or(0),
        files_changed: u64::try_from(row.get::<_, i64>(2)?).unwrap_or(0),
        cost_usd: row.get(3)?,
        waves_completed: u64::try_from(row.get::<_, i64>(4)?).unwrap_or(0),
    })
}

/// Map a database row to a [`ProjectRecord`].
#[allow(
    clippy::result_large_err,
    reason = "rusqlite::Error is inherently large; required by the rusqlite query_map API"
)]
fn row_to_project_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<ProjectRecord> {
    Ok(ProjectRecord { path: row.get(0)?, name: row.get(1)? })
}
