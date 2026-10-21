use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{path::Path, sync::Mutex};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationMessage {
    pub role: String,
    pub content: String,
    pub agent_name: Option<String>,
    pub kind: String,
}

pub struct Database {
    conn: Mutex<Connection>,
}

impl Database {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut conn =
            Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
            CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY,value TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS conversation_messages(
                id INTEGER PRIMARY KEY AUTOINCREMENT,role TEXT NOT NULL,content TEXT NOT NULL,
                agent_name TEXT,kind TEXT NOT NULL,created_at TEXT NOT NULL,
                session TEXT NOT NULL DEFAULT '');
            CREATE TABLE IF NOT EXISTS deliveries(
                id TEXT PRIMARY KEY,session TEXT NOT NULL,agent_name TEXT NOT NULL,
                request TEXT NOT NULL,result TEXT,created_at TEXT NOT NULL);",
        )?;
        // Preserve history from existing installations. Former run/binding tables
        // are archival data only; live status is never loaded from SQLite.
        let columns = {
            let mut stmt = conn.prepare("PRAGMA table_info(conversation_messages)")?;
            let rows = stmt
                .query_map([], |r| r.get::<_, String>(1))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        let tx = conn.transaction()?;
        if columns.iter().any(|c| c == "herdr_session") {
            tx.execute(
                "ALTER TABLE conversation_messages RENAME COLUMN herdr_session TO session",
                [],
            )?;
        }
        if columns.iter().any(|c| c == "project_id") {
            tx.execute(
                "ALTER TABLE conversation_messages RENAME COLUMN project_id TO agent_name",
                [],
            )?;
        }
        tx.execute_batch(
            "UPDATE conversation_messages SET session='toad' WHERE session LIKE 'toad:%';
            UPDATE deliveries SET session='toad' WHERE session LIKE 'toad:%';
            DELETE FROM settings WHERE key IN ('selected_session','selected_herdr_session');",
        )?;
        tx.execute_batch(
            "CREATE INDEX IF NOT EXISTS history_by_session ON conversation_messages(session,id);",
        )?;
        // An interrupted delivery is uncertain, never automatically resent.
        let interrupted = {
            let mut stmt =
                tx.prepare("SELECT id,session,agent_name FROM deliveries WHERE result IS NULL")?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        for (id, session, agent) in interrupted {
            let result = json!({"operation_id":id,"agent_name":agent,"delivery":"unconfirmed","outcome":"controller_restarted"}).to_string();
            tx.execute(
                "UPDATE deliveries SET result=?2 WHERE id=?1",
                params![id, result],
            )?;
            insert_message(&tx, &session, "tool", Some(&agent), "tool_result", &result)?;
        }
        tx.commit()?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
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
        self.conn.lock().unwrap().execute("INSERT INTO settings VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",params![key,value])?;
        Ok(())
    }
    pub fn append(
        &self,
        session: &str,
        role: &str,
        agent: Option<&str>,
        kind: &str,
        content: &str,
    ) -> Result<()> {
        insert_message(
            &self.conn.lock().unwrap(),
            session,
            role,
            agent,
            kind,
            content,
        )
    }
    pub fn history(&self, session: &str, limit: usize) -> Result<Vec<ConversationMessage>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT role,content,agent_name,kind FROM (
            SELECT id,role,content,agent_name,kind FROM conversation_messages
            WHERE session=?1 ORDER BY id DESC LIMIT ?2) ORDER BY id",
        )?;
        let rows = stmt
            .query_map(params![session, limit as i64], |r| {
                Ok(ConversationMessage {
                    role: r.get(0)?,
                    content: r.get(1)?,
                    agent_name: r.get(2)?,
                    kind: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
    pub fn begin_delivery(
        &self,
        id: &str,
        session: &str,
        agent: &str,
        request: &Value,
    ) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO deliveries VALUES(?1,?2,?3,?4,NULL,?5)",
            params![
                id,
                session,
                agent,
                request.to_string(),
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok(())
    }
    pub fn finish_delivery(
        &self,
        id: &str,
        session: &str,
        agent: &str,
        result: &Value,
    ) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let text = result.to_string();
        if tx.execute(
            "UPDATE deliveries SET result=?2 WHERE id=?1 AND result IS NULL",
            params![id, text],
        )? > 0
        {
            insert_message(&tx, session, "tool", Some(agent), "tool_result", &text)?;
        }
        tx.commit()?;
        Ok(())
    }
}
fn insert_message(
    conn: &Connection,
    session: &str,
    role: &str,
    agent: Option<&str>,
    kind: &str,
    content: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO conversation_messages(role,content,agent_name,kind,created_at,session)
        VALUES(?1,?2,?3,?4,?5,?6)",
        params![role, content, agent, kind, Utc::now().to_rfc3339(), session],
    )?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn existing_herdr_columns_migrate_without_losing_history() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.sqlite");
        let old = Connection::open(&path).unwrap();
        old.execute_batch("CREATE TABLE conversation_messages(id INTEGER PRIMARY KEY, role TEXT NOT NULL, content TEXT NOT NULL, agent_name TEXT, kind TEXT NOT NULL, created_at TEXT NOT NULL, herdr_session TEXT NOT NULL);
            INSERT INTO conversation_messages VALUES(1,'user','Keep this','agent','utterance','now','toad:one');
            INSERT INTO conversation_messages VALUES(2,'assistant','Other agent','agent-two','agent_response','now','toad:two');
            CREATE TABLE response_jobs(id TEXT PRIMARY KEY, session TEXT NOT NULL, epoch TEXT NOT NULL, agent_name TEXT NOT NULL, pane_id TEXT NOT NULL, kind TEXT NOT NULL, output TEXT NOT NULL, speech TEXT, created_at TEXT NOT NULL);
            INSERT INTO response_jobs VALUES('job','toad:one','epoch','agent','instance','agent_response','Done',NULL,'now');
            CREATE TABLE settings(key TEXT PRIMARY KEY,value TEXT NOT NULL);
            INSERT INTO settings VALUES('selected_herdr_session','toad:one');").unwrap();
        drop(old);
        let db = Database::open(&path).unwrap();
        let history = db.history("toad", 10).unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].content, "Keep this");
        assert_eq!(history[1].content, "Other agent");
        // Legacy bridge jobs are archival only and cannot feed the controller.
        let pending: i64 = db
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM response_jobs WHERE speech IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pending, 1);
        assert!(db.setting("selected_session").unwrap().is_none());
        assert!(db.setting("selected_herdr_session").unwrap().is_none());
    }
    #[test]
    fn interrupted_delivery_is_recorded_once_as_uncertain() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.sqlite");
        let db = Database::open(&path).unwrap();
        db.begin_delivery("op", "work", "agent", &json!({"request":"test"}))
            .unwrap();
        drop(db);
        let db = Database::open(&path).unwrap();
        let history = db.history("work", 20).unwrap();
        assert_eq!(history.len(), 1);
        assert!(history[0].content.contains("controller_restarted"));
        assert!(history[0].content.contains("unconfirmed"));
        drop(db);
        assert_eq!(
            Database::open(&path)
                .unwrap()
                .history("work", 20)
                .unwrap()
                .len(),
            1
        );
    }
}
