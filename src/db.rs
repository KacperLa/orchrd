use crate::config::Project;
use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::{path::Path, sync::Mutex};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSession {
    pub herdr_session: String,
    pub project_id: String,
    pub workspace_id: String,
    pub pane_id: String,
    pub agent_name: String,
    pub status: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversation_history_is_retained_and_windowed() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("orchrd.sqlite3")).unwrap();
        db.add_conversation_message("user", "first", None, "utterance")
            .unwrap();
        db.add_conversation_message("assistant", "second", None, "reply")
            .unwrap();
        db.add_conversation_message("user", "third", None, "utterance")
            .unwrap();

        let recent = db.recent_conversation(2).unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].content, "second");
        assert_eq!(recent[1].content, "third");

        let all = db.recent_conversation(10).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].content, "first");
    }

    #[test]
    fn old_pending_announcements_are_migrated_to_done() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("orchrd.sqlite3");
        let db = Database::open(&path).unwrap();
        db.create_run("run", "project", "request", "prompt", "", "work")
            .unwrap();
        db.finish_latest_run("project", "work", "pending_announcement", "finished")
            .unwrap();
        drop(db);

        let db = Database::open(&path).unwrap();
        assert_eq!(db.latest_run("project").unwrap().unwrap().status, "done");
    }

    #[test]
    fn agent_bindings_are_isolated_by_herdr_session() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("orchrd.sqlite3")).unwrap();
        for herdr_session in ["work", "personal"] {
            db.upsert_session(&AgentSession {
                herdr_session: herdr_session.into(),
                project_id: "orchrd".into(),
                workspace_id: format!("{herdr_session}-workspace"),
                pane_id: format!("{herdr_session}-pane"),
                agent_name: format!("{herdr_session}-agent"),
                status: "idle".into(),
            })
            .unwrap();
        }
        assert_eq!(db.sessions("work").unwrap().len(), 1);
        assert_eq!(
            db.session("personal", "orchrd")
                .unwrap()
                .unwrap()
                .agent_name,
            "personal-agent"
        );
    }

    #[test]
    fn old_runs_gain_a_herdr_session_column() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("orchrd.sqlite3");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE runs(id TEXT PRIMARY KEY,project_id TEXT NOT NULL,transcript TEXT NOT NULL,prompt TEXT NOT NULL,terminal_baseline TEXT NOT NULL DEFAULT '',status TEXT NOT NULL,result TEXT,created_at TEXT NOT NULL,updated_at TEXT NOT NULL);",
            )
            .unwrap();
        drop(connection);

        let db = Database::open(&path).unwrap();
        db.create_run("new", "project", "request", "prompt", "", "work")
            .unwrap();
        assert_eq!(
            db.latest_run("project").unwrap().unwrap().herdr_session,
            "work"
        );
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    pub id: String,
    pub herdr_session: String,
    pub project_id: String,
    pub transcript: String,
    pub prompt: String,
    pub status: String,
    pub result: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationMessage {
    pub id: i64,
    pub role: String,
    pub content: String,
    pub project_id: Option<String>,
    pub kind: String,
    pub created_at: String,
}

pub struct Database {
    conn: Mutex<Connection>,
}

impl Database {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn =
            Connection::open(path).with_context(|| format!("open database {}", path.display()))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;
          CREATE TABLE IF NOT EXISTS projects(id TEXT PRIMARY KEY,name TEXT NOT NULL,path TEXT NOT NULL,goal TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS agent_sessions(herdr_session TEXT NOT NULL,project_id TEXT NOT NULL,workspace_id TEXT NOT NULL,pane_id TEXT NOT NULL,agent_name TEXT NOT NULL,status TEXT NOT NULL,updated_at TEXT NOT NULL,PRIMARY KEY(herdr_session,project_id));
          CREATE TABLE IF NOT EXISTS runs(id TEXT PRIMARY KEY,project_id TEXT NOT NULL,transcript TEXT NOT NULL,prompt TEXT NOT NULL,terminal_baseline TEXT NOT NULL DEFAULT '',status TEXT NOT NULL,result TEXT,created_at TEXT NOT NULL,updated_at TEXT NOT NULL,herdr_session TEXT NOT NULL DEFAULT '');
          CREATE TABLE IF NOT EXISTS conversation_messages(id INTEGER PRIMARY KEY AUTOINCREMENT,role TEXT NOT NULL,content TEXT NOT NULL,project_id TEXT,kind TEXT NOT NULL,created_at TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY,value TEXT NOT NULL);")?;
        let has_terminal_baseline = {
            let mut statement = conn.prepare("PRAGMA table_info(runs)")?;
            let columns = statement
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            columns.iter().any(|column| column == "terminal_baseline")
        };
        if !has_terminal_baseline {
            conn.execute(
                "ALTER TABLE runs ADD COLUMN terminal_baseline TEXT NOT NULL DEFAULT ''",
                [],
            )?;
        }
        let has_herdr_session = {
            let mut statement = conn.prepare("PRAGMA table_info(runs)")?;
            let columns = statement
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            columns.iter().any(|column| column == "herdr_session")
        };
        if !has_herdr_session {
            conn.execute(
                "ALTER TABLE runs ADD COLUMN herdr_session TEXT NOT NULL DEFAULT ''",
                [],
            )?;
        }
        conn.execute(
            "UPDATE runs SET status='done' WHERE status='pending_announcement'",
            [],
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }
    pub fn sync_projects(&self, projects: &[Project]) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        for p in projects {
            tx.execute("INSERT INTO projects(id,name,path,goal) VALUES(?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET name=excluded.name,path=excluded.path,goal=excluded.goal", params![p.id,p.name,p.path.to_string_lossy(),p.goal])?;
        }
        tx.commit()?;
        Ok(())
    }
    pub fn session(&self, herdr_session: &str, project_id: &str) -> Result<Option<AgentSession>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT herdr_session,project_id,workspace_id,pane_id,agent_name,status FROM agent_sessions WHERE herdr_session=?1 AND project_id=?2", params![herdr_session, project_id], |r| Ok(AgentSession { herdr_session:r.get(0)?,project_id:r.get(1)?,workspace_id:r.get(2)?,pane_id:r.get(3)?,agent_name:r.get(4)?,status:r.get(5)? })).optional().map_err(Into::into)
    }
    pub fn sessions(&self, herdr_session: &str) -> Result<Vec<AgentSession>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt=conn.prepare("SELECT herdr_session,project_id,workspace_id,pane_id,agent_name,status FROM agent_sessions WHERE herdr_session=?1 ORDER BY project_id")?;
        let rows = stmt
            .query_map([herdr_session], |r| {
                Ok(AgentSession {
                    herdr_session: r.get(0)?,
                    project_id: r.get(1)?,
                    workspace_id: r.get(2)?,
                    pane_id: r.get(3)?,
                    agent_name: r.get(4)?,
                    status: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
    pub fn upsert_session(&self, s: &AgentSession) -> Result<()> {
        self.conn.lock().unwrap().execute("INSERT INTO agent_sessions(herdr_session,project_id,workspace_id,pane_id,agent_name,status,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(herdr_session,project_id) DO UPDATE SET workspace_id=excluded.workspace_id,pane_id=excluded.pane_id,agent_name=excluded.agent_name,status=excluded.status,updated_at=excluded.updated_at",params![s.herdr_session,s.project_id,s.workspace_id,s.pane_id,s.agent_name,s.status,Utc::now().to_rfc3339()])?;
        Ok(())
    }
    pub fn set_session_status(
        &self,
        herdr_session: &str,
        project_id: &str,
        status: &str,
    ) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE agent_sessions SET status=?3,updated_at=?4 WHERE herdr_session=?1 AND project_id=?2",
            params![herdr_session, project_id, status, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }
    pub fn delete_session(&self, herdr_session: &str, project_id: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "DELETE FROM agent_sessions WHERE herdr_session=?1 AND project_id=?2",
            params![herdr_session, project_id],
        )?;
        Ok(())
    }
    pub fn create_run(
        &self,
        id: &str,
        project: &str,
        transcript: &str,
        prompt: &str,
        terminal_baseline: &str,
        herdr_session: &str,
    ) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.conn.lock().unwrap().execute("INSERT INTO runs(id,project_id,transcript,prompt,terminal_baseline,status,created_at,updated_at,herdr_session) VALUES(?1,?2,?3,?4,?5,'working',?6,?6,?7)",params![id,project,transcript,prompt,terminal_baseline,now,herdr_session])?;
        Ok(())
    }
    pub fn run_terminal_baseline(&self, id: &str) -> Result<String> {
        self.conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT terminal_baseline FROM runs WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }
    pub fn finish_latest_run(
        &self,
        project: &str,
        herdr_session: &str,
        status: &str,
        result: &str,
    ) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let id:Option<String>=conn.query_row("SELECT id FROM runs WHERE project_id=?1 AND herdr_session=?2 AND status='working' ORDER BY created_at DESC LIMIT 1",params![project,herdr_session],|r|r.get(0)).optional()?;
        if let Some(id) = &id {
            conn.execute(
                "UPDATE runs SET status=?2,result=?3,updated_at=?4 WHERE id=?1",
                params![id, status, result, Utc::now().to_rfc3339()],
            )?;
        }
        Ok(id)
    }
    pub fn latest_run(&self, project: &str) -> Result<Option<RunRecord>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT id,herdr_session,project_id,transcript,prompt,status,result,created_at FROM runs WHERE project_id=?1 ORDER BY created_at DESC LIMIT 1",[project],|r|Ok(RunRecord{id:r.get(0)?,herdr_session:r.get(1)?,project_id:r.get(2)?,transcript:r.get(3)?,prompt:r.get(4)?,status:r.get(5)?,result:r.get(6)?,created_at:r.get(7)?})).optional().map_err(Into::into)
    }
    pub fn latest_working_run(
        &self,
        project: &str,
        herdr_session: &str,
    ) -> Result<Option<RunRecord>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT id,herdr_session,project_id,transcript,prompt,status,result,created_at FROM runs WHERE project_id=?1 AND herdr_session=?2 AND status='working' ORDER BY created_at DESC LIMIT 1",params![project,herdr_session],|r|Ok(RunRecord{id:r.get(0)?,herdr_session:r.get(1)?,project_id:r.get(2)?,transcript:r.get(3)?,prompt:r.get(4)?,status:r.get(5)?,result:r.get(6)?,created_at:r.get(7)?})).optional().map_err(Into::into)
    }
    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT value FROM settings WHERE key=?1", [key], |r| {
                r.get(0)
            })
            .optional()?)
    }
    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn.lock().unwrap().execute("INSERT INTO settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",params![key,value])?;
        Ok(())
    }
    pub fn add_conversation_message(
        &self,
        role: &str,
        content: &str,
        project_id: Option<&str>,
        kind: &str,
    ) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO conversation_messages(role,content,project_id,kind,created_at) VALUES(?1,?2,?3,?4,?5)",
            params![role, content, project_id, kind, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }
    pub fn recent_conversation(&self, limit: usize) -> Result<Vec<ConversationMessage>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id,role,content,project_id,kind,created_at FROM (
                SELECT id,role,content,project_id,kind,created_at
                FROM conversation_messages ORDER BY id DESC LIMIT ?1
             ) ORDER BY id",
        )?;
        let rows = stmt
            .query_map([limit as i64], |r| {
                Ok(ConversationMessage {
                    id: r.get(0)?,
                    role: r.get(1)?,
                    content: r.get(2)?,
                    project_id: r.get(3)?,
                    kind: r.get(4)?,
                    created_at: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}
