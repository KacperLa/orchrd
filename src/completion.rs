//! Completion observation is derived only from live Herdr work cycles.
use crate::{agent::AgentInfo, dialog::Capture};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};
use uuid::Uuid;

#[derive(Clone)]
pub(crate) struct Completion {
    pub id: String,
    pub capture: Capture,
}
struct Work {
    agent: AgentInfo,
    id: String,
    pending: bool,
    running: bool,
    failures: u8,
    retry_at: Instant,
}
#[derive(Default)]
pub(crate) struct Completions(HashMap<String, Work>);
impl Completions {
    pub fn observe(&mut self, agents: &[AgentInfo]) {
        self.0
            .retain(|name, _| agents.iter().any(|a| a.name == *name));
        for agent in agents {
            let start = agent.status == "working"
                && self.0.get(&agent.name).is_none_or(|old| {
                    old.agent.instance_id != agent.instance_id || old.agent.status != "working"
                });
            let entry = self.0.entry(agent.name.clone()).or_insert_with(|| Work {
                agent: agent.clone(),
                id: Uuid::new_v4().to_string(),
                pending: false,
                running: false,
                failures: 0,
                retry_at: Instant::now(),
            });
            if entry.agent.instance_id != agent.instance_id || start {
                entry.id = Uuid::new_v4().to_string();
                entry.pending = start;
                entry.running = false;
                entry.failures = 0;
                entry.retry_at = Instant::now();
            }
            entry.agent = agent.clone();
        }
    }
    pub fn claim(&mut self, capture: &Capture) -> Option<Completion> {
        let work = self.0.get_mut(&capture.agent.name)?;
        if work.agent != capture.agent
            || !work.pending
            || work.running
            || work.failures >= 3
            || work.retry_at > Instant::now()
            || !matches!(work.agent.status.as_str(), "idle" | "done")
        {
            return None;
        }
        work.running = true;
        Some(Completion {
            id: work.id.clone(),
            capture: capture.clone(),
        })
    }
    pub fn current(&self, completion: &Completion) -> bool {
        self.0
            .get(&completion.capture.agent.name)
            .is_some_and(|work| {
                work.id == completion.id && work.pending && work.agent == completion.capture.agent
            })
    }
    pub fn finish(&mut self, completion: &Completion, result: Result<(), bool>) {
        if let Some(work) = self
            .0
            .get_mut(&completion.capture.agent.name)
            .filter(|w| w.id == completion.id)
        {
            work.running = false;
            match result {
                Ok(()) => work.pending = false,
                // Stale captures can be retried with a fresh screen. Provider failures are bounded.
                Err(failed) => {
                    work.failures += u8::from(failed);
                    work.retry_at = Instant::now() + Duration::from_secs(5);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn capture(name: &str, instance: &str, status: &str, seq: u64) -> Capture {
        Capture {
            agent: AgentInfo {
                name: name.into(),
                display_name: name.into(),
                kind: None,
                instance_id: instance.into(),
                cwd: None,
                status: status.into(),
                state_change_seq: Some(seq),
            },
            screen: "Implemented the change. Tests passed.".into(),
        }
    }
    #[test]
    fn only_observed_work_is_summarized_once_and_new_work_invalidates_it() {
        let mut tracker = Completions::default();
        let idle = capture("local", "one", "idle", 1);
        tracker.observe(std::slice::from_ref(&idle.agent));
        assert!(tracker.claim(&idle).is_none());
        let working = capture("local", "one", "working", 2);
        tracker.observe(std::slice::from_ref(&working.agent));
        assert!(tracker.claim(&working).is_none());
        let done = capture("local", "one", "done", 3);
        tracker.observe(std::slice::from_ref(&done.agent));
        let first = tracker.claim(&done).unwrap();
        assert!(tracker.claim(&done).is_none());
        tracker.finish(&first, Ok(()));
        tracker.observe(std::slice::from_ref(&done.agent));
        assert!(tracker.claim(&done).is_none());
        tracker.observe(std::slice::from_ref(&working.agent));
        tracker.observe(std::slice::from_ref(&done.agent));
        let second = tracker.claim(&done).unwrap();
        assert_ne!(first.id, second.id);
        tracker.observe(std::slice::from_ref(&working.agent));
        assert!(!tracker.current(&second));
        tracker.finish(&second, Ok(())); // Cannot consume a newer work cycle.
        tracker.observe(std::slice::from_ref(&done.agent));
        assert!(tracker.claim(&done).is_some());
    }
    #[test]
    fn machines_instances_questions_and_retries_are_isolated() {
        let mut tracker = Completions::default();
        let local = capture("local", "same-pane", "working", 1);
        let remote = capture("remote:one:local", "same-pane", "working", 1);
        tracker.observe(&[local.agent.clone(), remote.agent.clone()]);
        let mut done = local.clone();
        done.agent.status = "idle".into();
        let mut blocked = remote.clone();
        blocked.agent.status = "blocked".into();
        tracker.observe(&[done.agent.clone(), blocked.agent.clone()]);
        assert!(tracker.claim(&blocked).is_none());
        for _ in 0..3 {
            let item = tracker.claim(&done).unwrap();
            tracker.finish(&item, Err(true));
            tracker.0.get_mut("local").unwrap().retry_at = Instant::now();
        }
        assert!(tracker.claim(&done).is_none());
        let mut replacement = done.clone();
        replacement.agent.instance_id = "replacement".into();
        tracker.observe(std::slice::from_ref(&replacement.agent));
        assert!(tracker.claim(&replacement).is_none());
        tracker.observe(&[]);
        tracker.observe(std::slice::from_ref(&done.agent));
        assert!(tracker.claim(&done).is_none());
    }
}
