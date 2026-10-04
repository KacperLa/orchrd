use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Coordinator-owned activity, transported verbatim in the D-Bus snapshot.
/// `finished` describes the operation, never the coding agent's status.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Activity {
    pub id: String,
    pub parent: Option<String>,
    pub agent_name: Option<String>,
    pub kind: String,
    pub state: String,
    pub finished: bool,
    pub detail: Value,
    pub started_ms: i64,
    pub updated_ms: i64,
    pub transitions: Vec<(i64, String)>,
}
impl Activity {
    pub fn new(
        id: impl Into<String>,
        kind: &str,
        state: &str,
        finished: bool,
        detail: Value,
    ) -> Self {
        let now = chrono::Utc::now().timestamp_millis();
        Self {
            id: id.into(),
            parent: None,
            agent_name: None,
            kind: kind.into(),
            state: state.into(),
            finished,
            detail,
            started_ms: now,
            updated_ms: now,
            transitions: vec![(now, state.into())],
        }
    }
    pub fn linked(mut self, parent: Option<String>, agent: Option<String>) -> Self {
        self.parent = parent;
        self.agent_name = agent;
        self
    }
}

pub fn update(records: &mut Vec<Activity>, mut next: Activity) {
    if let Some(old) = records.iter_mut().find(|a| a.id == next.id) {
        next.started_ms = old.started_ms;
        next.transitions = old.transitions.clone();
        if old.state != next.state {
            next.transitions.push((next.updated_ms, next.state.clone()));
            if next.transitions.len() > 32 {
                next.transitions.remove(0);
            }
        }
        if next.parent.is_none() {
            next.parent = old.parent.clone();
        }
        if next.agent_name.is_none() {
            next.agent_name = old.agent_name.clone();
        }
        if let (Some(old_detail), Some(detail)) =
            (old.detail.as_object(), next.detail.as_object_mut())
        {
            for (key, value) in old_detail {
                detail.entry(key.clone()).or_insert_with(|| value.clone());
            }
        }
        bound_detail(&mut next);
        *old = next;
    } else {
        bound_detail(&mut next);
        records.push(next);
    }
    // Never evict an operation that is still pending.
    while records.len() > 200 {
        if let Some(index) = records.iter().position(|a| a.finished) {
            records.remove(index);
        } else {
            break;
        }
    }
}

fn bound_detail(record: &mut Activity) {
    let encoded = record.detail.to_string();
    if encoded.len() > 16_000 {
        record.detail = serde_json::json!({"truncated":true,"preview":encoded.chars().take(4_000).collect::<String>()});
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn transitions_keep_exact_arguments_and_parent() {
        let mut records = vec![];
        update(
            &mut records,
            Activity::new(
                "op",
                "delivery",
                "sending",
                false,
                json!({"request":"run tests"}),
            )
            .linked(Some("turn".into()), Some("agent".into())),
        );
        update(
            &mut records,
            Activity::new(
                "op",
                "delivery",
                "awaiting_activity",
                false,
                json!({"delivery":"accepted"}),
            ),
        );
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].detail["request"], "run tests");
        assert_eq!(records[0].parent.as_deref(), Some("turn"));
        assert_eq!(records[0].transitions.len(), 2);
        assert!(!records[0].finished);
    }
    #[test]
    fn retention_never_discards_pending_operations() {
        let mut records = vec![Activity::new(
            "pending",
            "delivery",
            "sending",
            false,
            json!({}),
        )];
        for i in 0..240 {
            update(
                &mut records,
                Activity::new(i.to_string(), "summary", "recorded", true, json!({})),
            );
        }
        assert_eq!(records.len(), 200);
        assert_eq!(records[0].id, "pending");
    }
}
