use crate::activity::{self, Activity};
use crate::{agent::AgentInfo, config::AnnouncementMode, db::Database, tts::concise_speech};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::{
    sync::{broadcast, mpsc, oneshot, watch},
    time::Instant,
};
use uuid::Uuid;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scope {
    pub session: String,
    pub epoch: String,
}
pub const CONVERSATION_SESSION: &str = "toad";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub revision: u64,
    pub activity: Vec<Activity>,
    pub state: String,
    pub voice_state: String,
    pub active_agent: Option<String>,
    pub agents: Vec<AgentInfo>,
    pub announcement_mode: AnnouncementMode,
    pub message: Option<String>,
    /// Questions bound to current terminal captures, with input capabilities.
    #[serde(default)]
    pub observed_questions: Vec<Value>,
    #[serde(skip)]
    pub scope: Scope,
    #[serde(skip)]
    pub speech_epoch: u64,
}
impl Snapshot {
    pub fn initial(mode: AnnouncementMode) -> Self {
        Self {
            revision: 0,
            activity: vec![],
            state: "no_agents".into(),
            voice_state: "idle".into(),
            active_agent: None,
            agents: vec![],
            announcement_mode: mode,
            message: None,
            observed_questions: vec![],
            scope: Scope {
                session: CONVERSATION_SESSION.into(),
                epoch: Uuid::new_v4().to_string(),
            },
            speech_epoch: 0,
        }
    }
}
#[derive(Debug, Clone)]
pub enum Event {
    Snapshot(Arc<Snapshot>),
    Transcript(String),
    Completion { agent_name: String, speech: String },
    Approval { id: String, message: String },
    Error(String),
}
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    Send,
    Approve,
    Deny,
    Answer,
    Cancel,
}
#[derive(Clone)]
pub struct Speech {
    pub scope: Scope,
    pub epoch: u64,
    pub id: String,
    pub text: String,
    pub automatic: bool,
    pub question_id: Option<String>,
    pub completion_agent: Option<AgentInfo>,
}
impl Speech {
    pub fn current(&self, snapshot: &Snapshot) -> bool {
        self.scope == snapshot.scope
            && self.epoch == snapshot.speech_epoch
            && (!self.automatic || snapshot.announcement_mode == AnnouncementMode::Auto)
            && self.completion_agent.as_ref().is_none_or(|expected| {
                snapshot
                    .agents
                    .iter()
                    .any(|agent| completion_matches(expected, agent))
                    && !snapshot
                        .observed_questions
                        .iter()
                        .any(|q| q["agent_name"] == expected.name)
            })
            && self.question_id.as_ref().is_none_or(|id| {
                snapshot
                    .observed_questions
                    .iter()
                    .any(|q| q["question"]["question_id"].as_str() == Some(id))
            })
    }
}
pub enum Message {
    Activity {
        scope: Scope,
        activity: Activity,
    },
    Context {
        scope: Scope,
        reply: oneshot::Sender<Result<Snapshot>>,
    },
    Addressed {
        scope: Scope,
        agent_name: String,
        instance_id: String,
        reply: oneshot::Sender<Result<()>>,
    },
    Voice {
        scope: Scope,
        token: String,
        state: String,
        reply: oneshot::Sender<Result<()>>,
    },
    VoiceDone {
        token: String,
    },
    Mode {
        mode: AnnouncementMode,
        reply: oneshot::Sender<Result<()>>,
    },
    Record {
        scope: Scope,
        role: String,
        agent: Option<String>,
        kind: String,
        content: String,
        reply: oneshot::Sender<Result<()>>,
    },
    Reply {
        scope: Scope,
        text: String,
        reply: oneshot::Sender<Result<()>>,
    },
    Repeat {
        reply: oneshot::Sender<Result<()>>,
    },
    Polled {
        scope: Scope,
        agents: Vec<AgentInfo>,
    },
    ObservedQuestions {
        scope: Scope,
        questions: Vec<Value>,
    },
    CompletionSummary {
        scope: Scope,
        id: String,
        agent: AgentInfo,
        speech: String,
        reply: oneshot::Sender<Result<()>>,
    },
    ReadObservedQuestion {
        scope: Scope,
        agent_name: String,
        reply: oneshot::Sender<Result<bool>>,
    },
    Begin {
        parent: Option<String>,
        scope: Scope,
        agent_name: String,
        kind: OperationKind,
        request: Value,
        registered: oneshot::Sender<Result<(String, AgentInfo)>>,
        completed: oneshot::Sender<Value>,
    },
    Delivered {
        id: String,
        result: Result<()>,
    },
    Spoken {
        speech: Speech,
        result: Result<()>,
    },
    Error {
        scope: Scope,
        error: String,
    },
}
struct Observation {
    agent: AgentInfo,
}
fn completion_matches(expected: &AgentInfo, live: &AgentInfo) -> bool {
    expected.name == live.name
        && expected.instance_id == live.instance_id
        && expected.state_change_seq == live.state_change_seq
        && expected.status == live.status
        && matches!(live.status.as_str(), "idle" | "done")
}
struct Pending {
    scope: Scope,
    agent: AgentInfo,
    kind: OperationKind,
    accepted: bool,
    evidence: Option<&'static str>,
    reason: Option<&'static str>,
    error: Option<String>,
    deadline: Instant,
    completed: oneshot::Sender<Value>,
}
pub struct Coordinator {
    pub snapshot: Snapshot,
    db: Arc<Database>,
    events: broadcast::Sender<Event>,
    snapshots: watch::Sender<Snapshot>,
    speech: mpsc::Sender<Speech>,
    observations: HashMap<String, Observation>,
    pending: HashMap<String, Pending>,
    voice_token: Option<String>,
}
impl Coordinator {
    pub fn new(
        snapshot: Snapshot,
        db: Arc<Database>,
        events: broadcast::Sender<Event>,
        snapshots: watch::Sender<Snapshot>,
        speech: mpsc::Sender<Speech>,
    ) -> Self {
        Self {
            snapshot,
            db,
            events,
            snapshots,
            speech,
            observations: HashMap::new(),
            pending: HashMap::new(),
            voice_token: None,
        }
    }
    pub async fn run(mut self, mut messages: mpsc::Receiver<Message>) -> Result<()> {
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        loop {
            tokio::select! {
                message = messages.recv() => {
                    self.handle(message.context("coordinator channel closed")?)?;
                    self.publish();
                }
                _ = tick.tick() => { self.resolve_pending()?; self.publish(); }
            }
        }
    }
    fn check_scope(&self, scope: &Scope) -> Result<()> {
        if scope != &self.snapshot.scope {
            bail!("This turn belongs to a previous controller lifetime")
        }
        Ok(())
    }
    fn publish(&mut self) {
        self.snapshot.state = if self.snapshot.voice_state != "idle" {
            self.snapshot.voice_state.clone()
        } else if self.snapshot.agents.is_empty() {
            "no_agents".into()
        } else if self.snapshot.agents.iter().any(|a| a.status == "blocked") {
            "blocked".into()
        } else if self.snapshot.agents.iter().any(|a| a.status == "working") {
            "working".into()
        } else {
            "idle".into()
        };
        if *self.snapshots.borrow() != self.snapshot {
            self.snapshot.revision += 1;
            self.snapshots.send_replace(self.snapshot.clone());
            let _ = self
                .events
                .send(Event::Snapshot(Arc::new(self.snapshot.clone())));
        }
    }
    fn trace(&mut self, activity: Activity) {
        activity::update(&mut self.snapshot.activity, activity);
    }
    fn queue_speech(
        &mut self,
        scope: &Scope,
        id: String,
        text: String,
        automatic: bool,
    ) -> Result<()> {
        self.queue_bound_speech(scope, id, text, automatic, None)
    }
    fn queue_bound_speech(
        &mut self,
        scope: &Scope,
        id: String,
        text: String,
        automatic: bool,
        completion_agent: Option<AgentInfo>,
    ) -> Result<()> {
        if scope != &self.snapshot.scope {
            return Ok(());
        }
        self.trace(
            Activity::new(
                format!("{id}/speech"),
                "speech",
                "queued",
                false,
                json!({"text":text}),
            )
            .linked(
                if self.snapshot.activity.iter().any(|a| a.id == id) {
                    Some(id.clone())
                } else {
                    self.voice_token.clone()
                },
                None,
            ),
        );
        match self.speech.try_send(Speech {
            scope: scope.clone(),
            epoch: self.snapshot.speech_epoch,
            id: id.clone(),
            text,
            automatic,
            completion_agent,
            question_id: self
                .snapshot
                .observed_questions
                .iter()
                .any(|q| q["question"]["question_id"] == id)
                .then(|| id.clone()),
        }) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.trace(Activity::new(
                    format!("{id}/speech"),
                    "speech",
                    "skipped",
                    true,
                    json!({"reason":"speech queue is full"}),
                ));
                let _ = self.events.send(Event::Error(
                    "Speech queue is full; response remains available in history.".into(),
                ));
                Ok(())
            }
            Err(mpsc::error::TrySendError::Closed(_)) => bail!("speech worker stopped"),
        }
    }
    fn handle(&mut self, message: Message) -> Result<()> {
        match message {
            Message::Activity { scope, activity } => {
                if scope == self.snapshot.scope {
                    self.trace(activity);
                }
            }
            Message::Context { scope, reply } => {
                let _ = reply.send(self.check_scope(&scope).map(|_| self.snapshot.clone()));
            }
            Message::Addressed {
                scope,
                agent_name,
                instance_id,
                reply,
            } => {
                let result = (|| {
                    self.check_scope(&scope)?;
                    if !self
                        .snapshot
                        .agents
                        .iter()
                        .any(|a| a.name == agent_name && a.instance_id == instance_id)
                    {
                        bail!("The addressed agent is no longer present")
                    }
                    self.snapshot.active_agent = Some(agent_name);
                    Ok(())
                })();
                let _ = reply.send(result);
            }
            Message::Voice {
                scope,
                token,
                state,
                reply,
            } => {
                let result = (|| {
                    self.check_scope(&scope)?;
                    if self.voice_token.as_ref().is_some_and(|id| id != &token) {
                        bail!("Orchrd is still processing the previous utterance")
                    }
                    if state == "listening" {
                        self.snapshot.speech_epoch += 1;
                    }
                    self.voice_token = Some(token);
                    self.snapshot.voice_state = state;
                    Ok(())
                })();
                let _ = reply.send(result);
            }
            Message::VoiceDone { token } => {
                if self.voice_token.as_ref() == Some(&token) {
                    self.voice_token = None;
                    self.snapshot.voice_state = "idle".into();
                }
            }
            Message::Mode { mode, reply } => {
                let result = self.db.set_setting(
                    "announcement_mode",
                    if mode == AnnouncementMode::Auto {
                        "auto"
                    } else {
                        "muted"
                    },
                );
                if result.is_ok() && self.snapshot.announcement_mode != mode {
                    self.snapshot.announcement_mode = mode;
                    self.snapshot.speech_epoch += 1;
                }
                let _ = reply.send(result);
            }
            Message::Record {
                scope,
                role,
                agent,
                kind,
                content,
                reply,
            } => {
                let result =
                    self.db
                        .append(&scope.session, &role, agent.as_deref(), &kind, &content);
                if result.is_ok() && kind == "utterance" && scope == self.snapshot.scope {
                    let _ = self.events.send(Event::Transcript(content));
                }
                let _ = reply.send(result);
            }
            Message::Reply { scope, text, reply } => {
                let speech = concise_speech(&text);
                let result = (|| {
                    self.db
                        .append(&scope.session, "assistant", None, "reply", &speech)?;
                    if scope == self.snapshot.scope {
                        self.snapshot.message = Some(speech.clone());
                    }
                    self.queue_speech(&scope, Uuid::new_v4().to_string(), speech, false)
                })();
                let _ = reply.send(result);
            }
            Message::Repeat { reply } => {
                let result = (|| {
                    let speech = self
                        .db
                        .setting("last_spoken")?
                        .context("Orchrd has not spoken yet")?;
                    self.queue_speech(
                        &self.snapshot.scope.clone(),
                        Uuid::new_v4().to_string(),
                        speech,
                        false,
                    )
                })();
                let _ = reply.send(result);
            }
            Message::Polled { scope, agents } => {
                if scope != self.snapshot.scope {
                    return Ok(());
                }
                if self.snapshot.agents != agents {
                    self.trace(Activity::new(
                        "monitor",
                        "monitor",
                        "observed",
                        true,
                        json!({"agents":agents.len()}),
                    ));
                }
                self.observe(scope, agents)?;
            }
            Message::ObservedQuestions { scope, questions } => {
                if scope != self.snapshot.scope {
                    return Ok(());
                }
                let old = std::mem::replace(&mut self.snapshot.observed_questions, questions);
                self.snapshot.observed_questions.retain(|q| {
                    self.snapshot.agents.iter().any(|agent| {
                        q["agent_name"] == agent.name
                            && q["instance_id"] == agent.instance_id
                            && q["state_change_seq"] == json!(agent.state_change_seq)
                    })
                });
                for value in self.snapshot.observed_questions.clone() {
                    if old
                        .iter()
                        .any(|q| q["question"]["question_id"] == value["question"]["question_id"])
                    {
                        continue;
                    }
                    let question: crate::question::Question =
                        serde_json::from_value(value["question"].clone())?;
                    let name = value["agent_name"]
                        .as_str()
                        .context("observed question omitted agent")?;
                    let display = self
                        .snapshot
                        .agents
                        .iter()
                        .find(|a| a.name == name)
                        .map(|a| a.display_name.as_str())
                        .unwrap_or(name);
                    let hint = if value["read_only"] == true {
                        "Answer in the agent terminal."
                    } else if question.question_kind == "permission"
                        && value["can_approve"] == true
                        && value["can_deny"] == true
                    {
                        "Say approve to allow this request once, deny, or name an option."
                    } else if question.question_kind == "permission" {
                        "Name the option you want."
                    } else {
                        "You can reply by voice."
                    };
                    let speech = crate::tts::plain_speech(&format!(
                        "{display}. {} {hint}",
                        question.speech()
                    ));
                    self.db.append(
                        &scope.session,
                        "assistant",
                        Some(name),
                        "observed_question",
                        &speech,
                    )?;
                    self.snapshot.message = Some(speech.clone());
                    self.trace(
                        Activity::new(
                            &question.question_id,
                            "dialog",
                            "presented",
                            true,
                            json!({"question":question,"speech":speech,"read_only":value["read_only"]}),
                        )
                        .linked(None, Some(name.into())),
                    );
                    if question.question_kind == "permission"
                        && value["can_approve"] == true
                        && value["can_deny"] == true
                    {
                        let id = json!({"scope":scope,"agent_name":name,"instance_id":value["instance_id"],"question_id":question.question_id}).to_string();
                        let _ = self.events.send(Event::Approval {
                            id,
                            message: speech.clone(),
                        });
                    } else {
                        let _ = self.events.send(Event::Completion {
                            agent_name: name.into(),
                            speech: speech.clone(),
                        });
                    }
                    if self.snapshot.announcement_mode == AnnouncementMode::Auto {
                        self.queue_speech(&scope, question.question_id, speech, true)?;
                    }
                }
            }
            Message::CompletionSummary {
                scope,
                id,
                agent,
                speech,
                reply,
            } => {
                let result = (|| {
                    self.check_scope(&scope)?;
                    if !self
                        .snapshot
                        .agents
                        .iter()
                        .any(|live| completion_matches(&agent, live))
                        || self
                            .snapshot
                            .observed_questions
                            .iter()
                            .any(|q| q["agent_name"] == agent.name)
                    {
                        bail!("The completed response is no longer current")
                    }
                    let speech = crate::tts::plain_speech(&format!(
                        "{}. {}",
                        agent.display_name,
                        concise_speech(&speech)
                    ));
                    self.db.append(
                        &scope.session,
                        "assistant",
                        Some(&agent.name),
                        "completion_summary",
                        &speech,
                    )?;
                    self.snapshot.message = Some(speech.clone());
                    self.trace(
                        Activity::new(&id, "summary", "presented", true, json!({"speech":speech}))
                            .linked(None, Some(agent.name.clone())),
                    );
                    let _ = self.events.send(Event::Completion {
                        agent_name: agent.name.clone(),
                        speech: speech.clone(),
                    });
                    if self.snapshot.announcement_mode == AnnouncementMode::Auto {
                        self.queue_bound_speech(&scope, id, speech, true, Some(agent))?;
                    }
                    Ok(())
                })();
                let _ = reply.send(result);
            }
            Message::ReadObservedQuestion {
                scope,
                agent_name,
                reply,
            } => {
                let result = (|| {
                    self.check_scope(&scope)?;
                    let Some(value) = self
                        .snapshot
                        .observed_questions
                        .iter()
                        .find(|q| q["agent_name"] == agent_name)
                    else {
                        return Ok(false);
                    };
                    let question: crate::question::Question =
                        serde_json::from_value(value["question"].clone())?;
                    let hint = if value["read_only"] == true {
                        "Answer in the agent terminal."
                    } else {
                        "You can reply by voice."
                    };
                    let speech = crate::tts::plain_speech(&format!("{} {hint}", question.speech()));
                    self.db.append(
                        &scope.session,
                        "assistant",
                        Some(&agent_name),
                        "reply",
                        &speech,
                    )?;
                    self.snapshot.message = Some(speech.clone());
                    self.queue_speech(&scope, question.question_id, speech, false)?;
                    Ok(true)
                })();
                let _ = reply.send(result);
            }
            Message::Begin {
                parent,
                scope,
                agent_name,
                kind,
                request,
                registered,
                completed,
            } => {
                let result = (|| {
                    self.check_scope(&scope)?;
                    let agent = self
                        .snapshot
                        .agents
                        .iter()
                        .find(|a| a.name == agent_name)
                        .cloned()
                        .context("Agent is not connected to Orchrd")?;
                    if matches!(
                        kind,
                        OperationKind::Approve | OperationKind::Deny | OperationKind::Answer
                    ) && agent.status != "blocked"
                        && !(matches!(kind, OperationKind::Answer)
                            && matches!(agent.status.as_str(), "idle" | "done")
                            && self.snapshot.observed_questions.iter().any(|q| {
                                q["agent_name"] == agent.name
                                    && q["instance_id"] == agent.instance_id
                                    && q["conversation"] == true
                                    && q["question"]["question_id"] == request["question_id"]
                            }))
                    {
                        bail!("Agent is not waiting for input")
                    }
                    if self
                        .pending
                        .values()
                        .any(|p| p.scope == scope && p.agent.instance_id == agent.instance_id)
                    {
                        bail!("An operation for this agent is still pending")
                    }
                    let id = Uuid::new_v4().to_string();
                    self.db
                        .begin_delivery(&id, &scope.session, &agent.name, &request)?;
                    self.snapshot.active_agent = Some(agent.name.clone());
                    self.trace(
                        Activity::new(
                            id.clone(),
                            "delivery",
                            "sending",
                            false,
                            json!({"request":request,"operation":kind}),
                        )
                        .linked(parent, Some(agent.name.clone())),
                    );
                    self.pending.insert(
                        id.clone(),
                        Pending {
                            scope,
                            agent: agent.clone(),
                            kind,
                            accepted: false,
                            evidence: None,
                            reason: None,
                            error: None,
                            deadline: Instant::now() + Duration::from_secs(25),
                            completed,
                        },
                    );
                    Ok((id, agent))
                })();
                let _ = registered.send(result);
            }
            Message::Delivered { id, result } => {
                if self
                    .pending
                    .get(&id)
                    .is_some_and(|p| p.scope == self.snapshot.scope)
                {
                    self.trace(Activity::new(id.clone(), "delivery", if result.is_ok() { "awaiting_activity" } else { "unconfirmed" }, false,
                        json!({"delivery":if result.is_ok(){"accepted"}else{"unconfirmed"},"error":result.as_ref().err().map(|e|e.to_string())})));
                }
                if let Some(op) = self.pending.get_mut(&id) {
                    match result {
                        Ok(()) => {
                            op.accepted = true;
                            op.deadline = Instant::now() + Duration::from_secs(5);
                        }
                        Err(error) => {
                            tracing::warn!(%id,%error,"agent delivery unconfirmed");
                            op.reason = Some("delivery_unconfirmed");
                            op.error = Some(crate::intent::redact(&format!("{error:#}")));
                            op.deadline = Instant::now();
                        }
                    }
                }
                self.resolve_pending()?;
            }
            Message::Spoken { speech, result } => {
                if speech.scope == self.snapshot.scope {
                    self.trace(Activity::new(format!("{}/speech",speech.id), "speech", if result.is_ok(){"spoken"}else{"interrupted_or_failed"}, true,
                        json!({"text":speech.text,"error":result.as_ref().err().map(|e|e.to_string())})));
                }
                match result {
                    Ok(()) if speech.current(&self.snapshot) => {
                        self.db.set_setting("last_spoken", &speech.text)?
                    }
                    Err(error) => tracing::warn!(%error,"speech did not complete"),
                    _ => {}
                }
            }
            Message::Error { scope, error } => {
                tracing::warn!(session=%scope.session,%error,"Orchrd operation failed");
                if scope == self.snapshot.scope {
                    self.snapshot.message = Some(error.clone());
                    let _ = self.events.send(Event::Error(error));
                }
            }
        }
        Ok(())
    }
    fn observe(&mut self, scope: Scope, agents: Vec<AgentInfo>) -> Result<()> {
        for op in self.pending.values_mut().filter(|p| p.scope == scope) {
            match agents.iter().find(|a| a.name == op.agent.name) {
                None => op.reason = Some("agent_gone"),
                Some(agent) if agent.instance_id != op.agent.instance_id => {
                    op.reason = Some("agent_replaced")
                }
                Some(agent) => {
                    if let Some(evidence) = operation_evidence(op.kind, &op.agent, agent) {
                        op.evidence = Some(evidence);
                    }
                }
            }
        }
        self.resolve_pending()?;
        self.observations
            .retain(|instance, _| agents.iter().any(|a| &a.instance_id == instance));
        let mut status_changes = vec![];
        for agent in &agents {
            let old = self.observations.get(&agent.instance_id);
            if old.is_none_or(|old| {
                old.agent.status != agent.status
                    || old.agent.state_change_seq != agent.state_change_seq
            }) {
                let parent = self
                    .snapshot
                    .activity
                    .iter()
                    .rev()
                    .find(|a| {
                        a.kind == "agent_status" && a.detail["instance_id"] == agent.instance_id
                    })
                    .map(|a| a.id.clone());
                status_changes.push(Activity::new(Uuid::new_v4().to_string(), "agent_status", &agent.status, true, json!({"instance_id":agent.instance_id,"state_change_seq":agent.state_change_seq,"previous":old.map(|o| &o.agent.status)}))
                    .linked(parent, Some(agent.name.clone())));
            }
            self.observations.insert(
                agent.instance_id.clone(),
                Observation {
                    agent: agent.clone(),
                },
            );
        }
        self.snapshot.agents = agents;
        for status in status_changes {
            self.trace(status);
        }
        if !self
            .snapshot
            .agents
            .iter()
            .any(|a| Some(&a.name) == self.snapshot.active_agent.as_ref())
        {
            self.snapshot.active_agent = None;
        }
        Ok(())
    }
    fn resolve_pending(&mut self) -> Result<()> {
        let ready: Vec<_> = self
            .pending
            .iter()
            .filter_map(|(id, p)| {
                let already_working =
                    matches!(p.kind, OperationKind::Send) && p.agent.status == "working";
                let ready = p.accepted
                    && (p.evidence.is_some() || p.reason.is_some() || already_working)
                    || Instant::now() >= p.deadline;
                ready.then_some(id.clone())
            })
            .collect();
        for id in ready {
            let p = &self.pending[&id];
            let outcome = p.reason.unwrap_or_else(|| {
                p.evidence.unwrap_or(if p.accepted {
                    "delivered"
                } else {
                    "unconfirmed"
                })
            });
            let result = json!({"operation_id":id,"agent_name":p.agent.name,"operation":p.kind,"error":p.error,
                "delivery":if p.accepted{"accepted"}else{"unconfirmed"},"outcome":outcome});
            self.db
                .finish_delivery(&id, &p.scope.session, &p.agent.name, &result)?;
            if p.scope == self.snapshot.scope {
                self.trace(Activity::new(
                    id.clone(),
                    "delivery",
                    outcome,
                    true,
                    result.clone(),
                ));
            }
            if let Some(p) = self.pending.remove(&id) {
                let _ = p.completed.send(result);
            }
        }
        Ok(())
    }
}

fn operation_evidence(
    kind: OperationKind,
    before: &AgentInfo,
    after: &AgentInfo,
) -> Option<&'static str> {
    let changed = before.status != after.status
        || before
            .state_change_seq
            .zip(after.state_change_seq)
            .is_some_and(|(a, b)| b > a);
    match kind {
        OperationKind::Send
            if before.status != "working" && after.status == "working" && changed =>
        {
            Some("started")
        }
        OperationKind::Send
            if changed && matches!(after.status.as_str(), "blocked" | "idle" | "done") =>
        {
            Some("activity_observed")
        }
        OperationKind::Approve | OperationKind::Deny | OperationKind::Answer
            if before.status == "blocked"
                && matches!(after.status.as_str(), "working" | "idle" | "done")
                && changed =>
        {
            Some("response_observed")
        }
        OperationKind::Answer
            if matches!(before.status.as_str(), "idle" | "done")
                && changed
                && matches!(after.status.as_str(), "working" | "blocked") =>
        {
            Some("response_observed")
        }
        OperationKind::Answer
            if before.status == "blocked" && after.status == "blocked" && changed =>
        {
            Some("answer_recorded")
        }
        OperationKind::Cancel if matches!(after.status.as_str(), "idle" | "done") && changed => {
            Some("stopped")
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_summaries_use_auto_policy_and_expire_when_work_resumes() {
        for mode in [AnnouncementMode::Auto, AnnouncementMode::Muted] {
            let (mut c, mut audio) = fixture();
            c.snapshot.announcement_mode = mode;
            let scope = c.snapshot.scope.clone();
            let done = agent("idle", 3);
            c.observe(scope.clone(), vec![done.clone()]).unwrap();
            let (reply, mut receipt) = oneshot::channel();
            c.handle(Message::CompletionSummary {
                scope: scope.clone(),
                id: "finished".into(),
                agent: done.clone(),
                speech: "The fix passed its tests.".into(),
                reply,
            })
            .unwrap();
            receipt.try_recv().unwrap().unwrap();
            assert!(c
                .snapshot
                .message
                .as_ref()
                .unwrap()
                .contains("passed its tests"));
            if mode == AnnouncementMode::Auto {
                let speech = audio.try_recv().unwrap();
                assert!(speech.current(&c.snapshot));
                c.snapshot
                    .observed_questions
                    .push(json!({"agent_name":"coder"}));
                assert!(!speech.current(&c.snapshot));
                c.snapshot.observed_questions.clear();
                c.observe(scope.clone(), vec![agent("working", 4)]).unwrap();
                assert!(!speech.current(&c.snapshot));
            } else {
                assert!(audio.try_recv().is_err());
            }
            c.observe(scope.clone(), vec![agent("working", 4)]).unwrap();
            let (reply, mut receipt) = oneshot::channel();
            c.handle(Message::CompletionSummary {
                scope,
                id: "late".into(),
                agent: done,
                speech: "Stale result".into(),
                reply,
            })
            .unwrap();
            assert!(receipt.try_recv().unwrap().is_err());
            assert!(audio.try_recv().is_err());
        }
    }

    #[test]
    fn inferred_permissions_are_read_only_deduplicated_and_speech_tracks_the_screen() {
        let (mut c, mut audio) = fixture();
        let scope = c.snapshot.scope.clone();
        c.observe(scope.clone(), vec![agent("blocked", 1)]).unwrap();
        let mut events = c.events.subscribe();
        let value = json!({"agent_name":"coder","instance_id":"pane","state_change_seq":1,"read_only":true,
            "question":{"question_id":"observed","question_kind":"permission","title":"Approval", "body":"Run command?",
                "options":[{"id":"0","label":"Allow once","description":"allow_once"},
                    {"id":"1","label":"Always allow","description":"allow_persistent"},
                    {"id":"2","label":"Deny","description":"deny"}],"multiple":false,"allow_custom":false}});
        for _ in 0..2 {
            c.handle(Message::ObservedQuestions {
                scope: scope.clone(),
                questions: vec![value.clone()],
            })
            .unwrap();
        }
        let speech = audio.try_recv().unwrap();
        assert!(speech.text.contains("Option 3: Deny"));
        assert!(speech.text.contains("Answer in the agent terminal"));
        assert!(speech.current(&c.snapshot));
        assert!(audio.try_recv().is_err());
        assert!(matches!(
            events.try_recv().unwrap(),
            Event::Completion { .. }
        ));
        assert!(events.try_recv().is_err()); // No actionable approval signal.
        let (reply, mut received) = oneshot::channel();
        c.handle(Message::ReadObservedQuestion {
            scope: scope.clone(),
            agent_name: "coder".into(),
            reply,
        })
        .unwrap();
        assert!(received.try_recv().unwrap().unwrap());
        let read = audio.try_recv().unwrap();
        assert!(!read.automatic);
        assert!(read.text.contains("Option 3: Deny"));
        c.handle(Message::ObservedQuestions {
            scope: scope.clone(),
            questions: vec![],
        })
        .unwrap();
        assert!(!speech.current(&c.snapshot));
        assert!(!read.current(&c.snapshot));
        c.observe(scope.clone(), vec![]).unwrap();
        c.handle(Message::ObservedQuestions {
            scope,
            questions: vec![value],
        })
        .unwrap();
        assert!(c.snapshot.observed_questions.is_empty());
        assert!(audio.try_recv().is_err());
    }

    #[test]
    fn actionable_permissions_emit_bound_approval_controls() {
        let (mut c, _audio) = fixture();
        let scope = c.snapshot.scope.clone();
        c.observe(scope.clone(), vec![agent("blocked", 1)]).unwrap();
        let mut events = c.events.subscribe();
        c.handle(Message::ObservedQuestions { scope, questions:vec![json!({"agent_name":"coder","instance_id":"pane",
            "state_change_seq":1,"read_only":false,"can_approve":true,"can_deny":true,
            "question":{"question_id":"bound","question_kind":"permission","title":"Permission","body":"Run?",
                "options":[],"multiple":false,"allow_custom":false}})] }).unwrap();
        let Event::Approval { id, message } = events.try_recv().unwrap() else {
            panic!("expected approval")
        };
        let reference: Value = serde_json::from_str(&id).unwrap();
        assert_eq!(reference["question_id"], "bound");
        assert_eq!(reference["instance_id"], "pane");
        assert!(message.contains("once"));
    }
    #[test]
    fn chat_answers_require_the_bound_question_and_observe_resumed_work() {
        let (mut c, _audio) = fixture();
        let scope = c.snapshot.scope.clone();
        c.observe(scope.clone(), vec![agent("idle", 1)]).unwrap();
        c.snapshot.observed_questions = vec![
            json!({"agent_name":"coder","instance_id":"pane","conversation":true,
            "question":{"question_id":"chat","question_kind":"question"}}),
        ];
        for id in ["wrong", "chat"] {
            let (registered, mut registration) = oneshot::channel();
            let (completed, mut completion) = oneshot::channel();
            c.handle(Message::Begin {
                parent: None,
                scope: scope.clone(),
                agent_name: "coder".into(),
                kind: OperationKind::Answer,
                request: json!({"question_id":id}),
                registered,
                completed,
            })
            .unwrap();
            let result = registration.try_recv().unwrap();
            if id == "wrong" {
                assert!(result.is_err());
                continue;
            }
            let (operation, _) = result.unwrap();
            c.handle(Message::Delivered {
                id: operation,
                result: Ok(()),
            })
            .unwrap();
            c.observe(scope.clone(), vec![agent("working", 2)]).unwrap();
            assert_eq!(
                completion.try_recv().unwrap()["outcome"],
                "response_observed"
            );
        }
    }

    #[test]
    fn full_speech_queue_marks_skipped_and_keeps_coordinator_available() {
        let (mut c, _audio) = fixture();
        let (speech, _queued_audio) = mpsc::channel(2);
        c.speech = speech;
        let scope = c.snapshot.scope.clone();
        let capacity = c.speech.max_capacity();
        for n in 0..=capacity {
            c.queue_speech(&scope, n.to_string(), "Response".into(), true)
                .unwrap();
        }
        assert_eq!(c.speech.capacity(), 0);
        let skipped = c
            .snapshot
            .activity
            .iter()
            .find(|a| a.id == format!("{capacity}/speech"))
            .unwrap();
        assert_eq!(skipped.state, "skipped");
        assert_eq!(skipped.detail["reason"], "speech queue is full");
        c.publish();
    }
    #[test]
    fn another_question_records_answer_without_claiming_work_resumed() {
        assert_eq!(
            operation_evidence(
                OperationKind::Answer,
                &agent("blocked", 1),
                &agent("blocked", 2)
            ),
            Some("answer_recorded")
        );
        assert_eq!(
            operation_evidence(
                OperationKind::Answer,
                &agent("blocked", 1),
                &agent("blocked", 1)
            ),
            None
        );
        assert_eq!(
            operation_evidence(
                OperationKind::Approve,
                &agent("blocked", 1),
                &agent("blocked", 2)
            ),
            None
        );
    }

    fn agent(status: &str, seq: u64) -> AgentInfo {
        AgentInfo {
            display_name: "coder".into(),
            name: "coder".into(),
            kind: Some("generic".into()),
            instance_id: "pane".into(),
            cwd: None,
            status: status.into(),
            state_change_seq: Some(seq),
        }
    }
    fn fixture() -> (Coordinator, mpsc::Receiver<Speech>) {
        let db = Arc::new(Database::open(std::path::Path::new(":memory:")).unwrap());
        let snapshot = Snapshot::initial(AnnouncementMode::Auto);
        let (events, _) = broadcast::channel(32);
        let (snapshots, _) = watch::channel(snapshot.clone());
        let (speech, rx) = mpsc::channel(256);
        (
            Coordinator::new(snapshot, db, events, snapshots, speech),
            rx,
        )
    }
    fn begin(c: &mut Coordinator, kind: OperationKind) -> (String, oneshot::Receiver<Value>) {
        let (registered, mut registration) = oneshot::channel();
        let (completed, completion) = oneshot::channel();
        c.handle(Message::Begin {
            parent: None,
            scope: c.snapshot.scope.clone(),
            agent_name: "coder".into(),
            kind,
            request: json!({"request":"test"}),
            registered,
            completed,
        })
        .unwrap();
        (registration.try_recv().unwrap().unwrap().0, completion)
    }
    #[test]
    fn approval_requires_transition_and_receipt_in_either_order() {
        for transition_first in [false, true] {
            let (mut c, _) = fixture();
            let scope = c.snapshot.scope.clone();
            c.observe(scope.clone(), vec![agent("blocked", 1)]).unwrap();
            let (id, mut completion) = begin(&mut c, OperationKind::Approve);
            if transition_first {
                c.observe(scope.clone(), vec![agent("working", 2)]).unwrap();
            } else {
                c.handle(Message::Delivered {
                    id: id.clone(),
                    result: Ok(()),
                })
                .unwrap();
            }
            assert!(completion.try_recv().is_err());
            if transition_first {
                c.handle(Message::Delivered { id, result: Ok(()) }).unwrap();
            } else {
                c.observe(scope, vec![agent("working", 2)]).unwrap();
            }
            assert_eq!(
                completion.try_recv().unwrap()["outcome"],
                "response_observed"
            );
            assert_eq!(c.db.history(CONVERSATION_SESSION, 20).unwrap().len(), 1);
        }
    }
    #[test]
    fn timeout_and_disappearance_do_not_claim_approval() {
        for gone in [false, true] {
            let (mut c, _) = fixture();
            let scope = c.snapshot.scope.clone();
            c.observe(scope.clone(), vec![agent("blocked", 1)]).unwrap();
            let (id, mut completion) = begin(&mut c, OperationKind::Approve);
            c.handle(Message::Delivered {
                id: id.clone(),
                result: Ok(()),
            })
            .unwrap();
            if gone {
                c.observe(scope, vec![]).unwrap();
            } else {
                c.pending.get_mut(&id).unwrap().deadline = Instant::now();
                c.resolve_pending().unwrap();
            }
            let result = completion.try_recv().unwrap();
            assert_eq!(result["delivery"], "accepted");
            assert_eq!(
                result["outcome"],
                if gone { "agent_gone" } else { "delivered" }
            );
        }
    }
    #[test]
    fn busy_agent_delivery_does_not_claim_new_task_started() {
        let (mut c, _) = fixture();
        c.observe(c.snapshot.scope.clone(), vec![agent("working", 1)])
            .unwrap();
        let (id, mut completion) = begin(&mut c, OperationKind::Send);
        c.handle(Message::Delivered { id, result: Ok(()) }).unwrap();
        assert_eq!(completion.try_recv().unwrap()["outcome"], "delivered");
        assert!(
            operation_evidence(OperationKind::Send, &agent("idle", 1), &agent("unknown", 2))
                .is_none()
        );
    }
    #[test]
    fn addressed_agent_updates_context_without_changing_status() {
        let (mut c, _) = fixture();
        let scope = c.snapshot.scope.clone();
        let mut reader = agent("idle", 1);
        reader.name = "reader".into();
        reader.instance_id = "reader-pane".into();
        c.observe(scope.clone(), vec![agent("working", 1), reader])
            .unwrap();
        let (id, _) = begin(&mut c, OperationKind::Send);
        c.handle(Message::Delivered { id, result: Ok(()) }).unwrap();
        assert_eq!(c.snapshot.active_agent.as_deref(), Some("coder"));
        let (reply, mut received) = oneshot::channel();
        c.handle(Message::Addressed {
            scope: scope.clone(),
            agent_name: "reader".into(),
            instance_id: "reader-pane".into(),
            reply,
        })
        .unwrap();
        received.try_recv().unwrap().unwrap();
        let (reply, mut received) = oneshot::channel();
        c.handle(Message::Context { scope, reply }).unwrap();
        let snapshot = received.try_recv().unwrap().unwrap();
        assert_eq!(snapshot.active_agent.as_deref(), Some("reader"));
        assert_eq!(snapshot.agents[0].status, "working");
    }

    #[test]
    fn stale_reads_cannot_change_last_addressed_agent() {
        let (mut c, _) = fixture();
        let scope = c.snapshot.scope.clone();
        c.observe(scope.clone(), vec![agent("working", 1)]).unwrap();
        for stale_session in [false, true] {
            let mut read_scope = scope.clone();
            if stale_session {
                read_scope.epoch = "previous-selection".into();
            }
            let (reply, mut received) = oneshot::channel();
            c.handle(Message::Addressed {
                scope: read_scope,
                agent_name: "coder".into(),
                instance_id: if stale_session {
                    "pane"
                } else {
                    "replaced-pane"
                }
                .into(),
                reply,
            })
            .unwrap();
            assert!(received.try_recv().unwrap().is_err());
            assert!(c.snapshot.active_agent.is_none());
        }
    }

    #[test]
    fn activity_is_scoped_and_delivery_does_not_change_agent_status() {
        let (mut c, _) = fixture();
        let scope = c.snapshot.scope.clone();
        c.observe(scope.clone(), vec![agent("idle", 1)]).unwrap();
        let (id, _) = begin(&mut c, OperationKind::Send);
        assert_eq!(
            c.snapshot
                .activity
                .iter()
                .find(|a| a.id == id)
                .unwrap()
                .state,
            "sending"
        );
        c.handle(Message::Delivered {
            id: id.clone(),
            result: Ok(()),
        })
        .unwrap();
        assert_eq!(
            c.snapshot
                .activity
                .iter()
                .find(|a| a.id == id)
                .unwrap()
                .state,
            "awaiting_activity"
        );
        assert_eq!(c.snapshot.agents[0].status, "idle");
        let mut stale = scope;
        stale.epoch = "old selection".into();
        c.handle(Message::Activity {
            scope: stale,
            activity: Activity::new("stale", "speech", "speaking", false, json!({})),
        })
        .unwrap();
        assert!(!c.snapshot.activity.iter().any(|a| a.id == "stale"));
        c.publish();
        let revision = c.snapshot.revision;
        c.publish();
        assert_eq!(c.snapshot.revision, revision);
    }

    #[test]
    fn conversational_reply_cannot_clear_agent_state() {
        let (mut c, _audio) = fixture();
        c.observe(c.snapshot.scope.clone(), vec![agent("blocked", 1)])
            .unwrap();
        let (reply, _) = oneshot::channel();
        c.handle(Message::Reply {
            scope: c.snapshot.scope.clone(),
            text: "Hello".into(),
            reply,
        })
        .unwrap();
        c.publish();
        assert_eq!(c.snapshot.state, "blocked");
        assert_eq!(c.snapshot.agents[0].status, "blocked");
    }
}
