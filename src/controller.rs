use crate::activity::Activity;
pub use crate::coordinator::{Event, Scope, Snapshot};
use crate::{
    config::{AnnouncementMode, Config},
    coordinator::{Coordinator, Message, OperationKind, Speech},
    db::Database,
    herdr::Herdr,
    intent::{redact, IntentClient, ToolCall},
    routing::{Operation, Router},
    stt::SttClient,
    tts::TtsClient,
};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};
use tokio::{
    sync::{broadcast, mpsc, oneshot, watch, Mutex},
    task::JoinSet,
    time::{sleep, timeout},
};
use uuid::Uuid;

struct Recording {
    permissions: HashMap<String, String>,
    scope: Scope,
    token: String,
}
struct Turn {
    permissions: HashMap<String, String>,
    scope: Scope,
    token: String,
    text: Option<String>,
    completed: Option<oneshot::Sender<Result<()>>>,
}
struct Runtime {
    coordinator: Coordinator,
    messages: mpsc::Receiver<Message>,
    turns: mpsc::Receiver<Turn>,
    speech: mpsc::Receiver<Speech>,
}
pub struct Controller {
    pub config: Arc<Config>,
    agents: Herdr,
    intent: IntentClient,
    router: Router,
    tts: TtsClient,
    stt: SttClient,
    messages: mpsc::Sender<Message>,
    turns: mpsc::Sender<Turn>,
    snapshots: watch::Receiver<Snapshot>,
    events: broadcast::Sender<Event>,
    recording: Mutex<Option<Recording>>,
    runtime: Mutex<Option<Runtime>>,
}
impl Controller {
    pub async fn new(config: Config) -> Result<Arc<Self>> {
        config.validate()?;
        config.validate_runtime()?;
        tokio::fs::create_dir_all(Config::runtime_dir()).await?;
        let db = Arc::new(Database::open(&Config::data_dir()?.join("orchrd.sqlite3"))?);
        let mode = db
            .setting("announcement_mode")?
            .map(|v| {
                if v == "auto" {
                    AnnouncementMode::Auto
                } else {
                    AnnouncementMode::Muted
                }
            })
            .unwrap_or(config.voice.announcement_mode);
        let snapshot = Snapshot::initial(mode);
        let (snapshots_tx, snapshots) = watch::channel(snapshot.clone());
        let (events, _) = broadcast::channel(128);
        let (messages, messages_rx) = mpsc::channel(256);
        let (turns, turns_rx) = mpsc::channel(16);
        let (speech, speech_rx) = mpsc::channel(32);
        let coordinator = Coordinator::new(snapshot, db, events.clone(), snapshots_tx, speech);
        Ok(Arc::new(Self {
            agents: Herdr::new(config.agents.clone()),
            intent: IntentClient::new(&config)?,
            router: Router::new(&config.routing)?,
            tts: TtsClient::from_config(&config)?,
            stt: SttClient::new(&config)?,
            config: Arc::new(config),
            messages,
            turns,
            snapshots,
            events,
            recording: Mutex::new(None),
            runtime: Mutex::new(Some(Runtime {
                coordinator,
                messages: messages_rx,
                turns: turns_rx,
                speech: speech_rx,
            })),
        }))
    }
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }
    pub async fn snapshot(&self) -> Snapshot {
        self.snapshots.borrow().clone()
    }
    async fn send(&self, message: Message) -> Result<()> {
        self.messages
            .send(message)
            .await
            .context("coordinator stopped")
    }
    fn trace(&self, scope: &Scope, activity: Activity) -> Result<()> {
        match self.messages.try_send(Message::Activity {
            scope: scope.clone(),
            activity,
        }) {
            Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => Ok(()),
            Err(mpsc::error::TrySendError::Closed(_)) => bail!("coordinator stopped"),
        }
    }
    async fn ask<T>(&self, make: impl FnOnce(oneshot::Sender<Result<T>>) -> Message) -> Result<T> {
        let (tx, rx) = oneshot::channel();
        self.send(make(tx)).await?;
        rx.await.context("coordinator stopped before replying")?
    }
    async fn context(&self, scope: &Scope) -> Result<Snapshot> {
        self.ask(|reply| Message::Context {
            scope: scope.clone(),
            reply,
        })
        .await
    }
    async fn voice(&self, scope: &Scope, token: &str, state: &str) -> Result<()> {
        self.ask(|reply| Message::Voice {
            scope: scope.clone(),
            token: token.into(),
            state: state.into(),
            reply,
        })
        .await?;
        self.trace(
            scope,
            Activity::new(token, "utterance", state, false, json!({})),
        )
    }
    async fn record(
        &self,
        scope: &Scope,
        role: &str,
        agent: Option<&str>,
        kind: &str,
        content: &str,
    ) -> Result<()> {
        self.ask(|reply| Message::Record {
            scope: scope.clone(),
            role: role.into(),
            agent: agent.map(str::to_owned),
            kind: kind.into(),
            content: content.into(),
            reply,
        })
        .await
    }
    async fn reply(&self, scope: &Scope, text: &str) -> Result<()> {
        self.ask(|reply| Message::Reply {
            scope: scope.clone(),
            text: text.into(),
            reply,
        })
        .await
    }
    pub async fn set_announcement_mode(&self, mode: AnnouncementMode) -> Result<()> {
        self.ask(|reply| Message::Mode { mode, reply }).await
    }
    pub async fn repeat_last(&self) -> Result<()> {
        self.ask(|reply| Message::Repeat { reply }).await
    }
    pub async fn start_listening(&self) -> Result<()> {
        let mut recording = self.recording.lock().await;
        if recording.is_some() {
            bail!("already recording")
        }
        let scope = self.snapshots.borrow().scope.clone();
        let token = Uuid::new_v4().to_string();
        let permissions = self.agents.question_ids().await;
        self.voice(&scope, &token, "listening").await?;
        let _ = self.tts.cancel().await;
        match self.stt.start(&token).await {
            Ok(()) => {
                *recording = Some(Recording {
                    scope,
                    token,
                    permissions,
                });
                Ok(())
            }
            Err(error) => {
                self.trace(
                    &scope,
                    Activity::new(
                        &token,
                        "utterance",
                        "failed",
                        true,
                        json!({"error":error.to_string()}),
                    ),
                )?;
                self.send(Message::VoiceDone { token }).await?;
                Err(error)
            }
        }
    }
    pub async fn stop_listening(&self) -> Result<()> {
        let recording = self
            .recording
            .lock()
            .await
            .take()
            .context("not recording")?;
        let _ = self
            .voice(&recording.scope, &recording.token, "transcribing")
            .await;
        if let Err(error) = self.stt.stop_capture().await {
            let _ = self.stt.cancel().await;
            self.send(Message::VoiceDone {
                token: recording.token,
            })
            .await?;
            return Err(error);
        }
        if self
            .turns
            .try_send(Turn {
                permissions: recording.permissions,
                scope: recording.scope,
                token: recording.token.clone(),
                text: None,
                completed: None,
            })
            .is_err()
        {
            let _ = self.stt.cancel().await;
            self.send(Message::VoiceDone {
                token: recording.token,
            })
            .await?;
            bail!("conversation queue is full or closed")
        }
        Ok(())
    }
    pub async fn cancel_listening(&self) -> Result<()> {
        let mut recording = self.recording.lock().await;
        if let Some(recording) = recording.take() {
            let result = self.stt.cancel().await;
            self.trace(
                &recording.scope,
                Activity::new(&recording.token, "utterance", "cancelled", true, json!({})),
            )?;
            self.send(Message::VoiceDone {
                token: recording.token,
            })
            .await?;
            result?;
        }
        Ok(())
    }
    pub async fn dispatch_transcript(&self, text: &str) -> Result<()> {
        if text.trim().is_empty() || text.len() > 64_000 {
            bail!("dispatch text must contain between 1 and 64000 bytes")
        }
        let permissions = self.agents.question_ids().await;
        // Pin the controller lifetime at submission, before this turn waits behind another.
        let scope = self.snapshots.borrow().scope.clone();
        let (completed, rx) = oneshot::channel();
        let token = Uuid::new_v4().to_string();
        self.trace(
            &scope,
            Activity::new(
                &token,
                "utterance",
                "queued",
                false,
                json!({"transcript":text}),
            ),
        )?;
        self.turns
            .try_send(Turn {
                permissions,
                scope,
                token,
                text: Some(text.into()),
                completed: Some(completed),
            })
            .context("conversation queue is full or closed")?;
        rx.await
            .context("conversation worker stopped before replying")?
    }
    pub async fn resolve_approval(&self, id: &str, approve: bool) -> Result<()> {
        let target: Value = serde_json::from_str(id).context("invalid approval reference")?;
        let scope: Scope = serde_json::from_value(target["scope"].clone())?;
        let agent = string_arg(&target, "agent_name")?;
        let question_id = string_arg(&target, "question_id")?;
        let current = self.context(&scope).await?;
        let expected = current
            .agents
            .iter()
            .find(|a| {
                a.name == agent && Some(a.instance_id.as_str()) == target["instance_id"].as_str()
            })
            .context("The agent associated with this approval is no longer present")?;
        let result = self
            .operation(
                &scope,
                expected,
                if approve {
                    OperationKind::Approve
                } else {
                    OperationKind::Deny
                },
                json!({"approve":approve,"question_id":question_id}),
                None,
            )
            .await?;
        if result["outcome"] != "response_observed" {
            bail!("Response not confirmed: {result}")
        }
        Ok(())
    }
    pub async fn run(self: Arc<Self>) -> Result<()> {
        let runtime = self
            .runtime
            .lock()
            .await
            .take()
            .context("controller already running")?;
        let mut tasks = JoinSet::new();
        tasks.spawn(runtime.coordinator.run(runtime.messages));
        tasks.spawn(self.clone().poll_worker());
        tasks.spawn(self.clone().completion_worker());
        // Keep slow extraction separate from status/voice processing.
        for _ in 0..2 {
            tasks.spawn(self.clone().dialog_worker());
        }
        tasks.spawn(self.clone().conversation_worker(runtime.turns));
        tasks.spawn(Self::speech_worker(
            self.tts.clone(),
            self.snapshots.clone(),
            self.messages.clone(),
            runtime.speech,
        ));
        tasks
            .join_next()
            .await
            .context("no controller workers")?
            .context("controller worker panicked")??;
        bail!("controller worker stopped unexpectedly")
    }
    async fn poll_worker(self: Arc<Self>) -> Result<()> {
        loop {
            let scope = self.snapshots.borrow().scope.clone();
            let agents = match self.agents.poll().await {
                Ok(agents) => agents,
                Err(error) => {
                    tracing::warn!(%error, "agent observation unavailable");
                    self.trace(
                        &scope,
                        Activity::new(
                            "herdr-monitor",
                            "monitor",
                            "unavailable",
                            true,
                            json!({"error":error.to_string()}),
                        ),
                    )?;
                    Vec::new()
                }
            };
            self.send(Message::Polled { scope, agents }).await?;
            let scope = self.snapshots.borrow().scope.clone();
            let questions = self.agents.questions().await;
            self.send(Message::ObservedQuestions { scope, questions })
                .await?;
            sleep(Duration::from_secs(self.config.agents.poll_seconds)).await;
        }
    }
    async fn dialog_worker(self: Arc<Self>) -> Result<()> {
        let herdr = &self.agents;
        loop {
            let Some(capture) = herdr.next().await else {
                sleep(Duration::from_millis(250)).await;
                continue;
            };
            let scope = self.snapshots.borrow().scope.clone();
            let id = Uuid::new_v4().to_string();
            let trace = |state: &str, finished: bool, detail: Value| {
                self.trace(
                    &scope,
                    Activity::new(&id, "dialog", state, finished, detail)
                        .linked(None, Some(capture.agent.name.clone())),
                )
            };
            trace("detecting", false, json!({"observation":true}))?;
            let result = async {
                let route = crate::dialog::detect(&self.router, &capture).await?;
                if !herdr.current(&capture).await {
                    return Ok(("stale", None));
                }
                herdr.mark_detection(&capture, route).await;
                match route {
                    crate::dialog::Route::Ignore => Ok(("ignored", None)),
                    crate::dialog::Route::Inspect => Ok(("inspect", None)),
                    _ => {
                        trace("extracting", false, json!({"observation":true}))?;
                        let (question, extracted) =
                            crate::dialog::extract(&self.intent, &capture, route).await?;
                        if !herdr.current(&capture).await {
                            return Ok(("stale", None));
                        }
                        let detail =
                            json!({"question":question,"extraction":extracted,"observation":true});
                        herdr
                            .store(
                                &capture,
                                question,
                                extracted,
                                route == crate::dialog::Route::Conversation,
                            )
                            .await;
                        Ok::<_, anyhow::Error>(("available", Some(detail)))
                    }
                }
            }
            .await;
            match result {
                Ok((state, detail)) => trace(
                    state,
                    true,
                    detail.unwrap_or_else(|| json!({"observation":true})),
                )?,
                Err(error) => trace(
                    "inspect",
                    true,
                    json!({"reason":format!("{error:#}"),"observation":true}),
                )?,
            }
            herdr.finish(&capture).await;
        }
    }

    async fn completion_worker(self: Arc<Self>) -> Result<()> {
        loop {
            let Some(completion) = self.agents.next_completion().await else {
                sleep(Duration::from_millis(250)).await;
                continue;
            };
            let scope = self.snapshots.borrow().scope.clone();
            let agent = &completion.capture.agent;
            self.trace(
                &scope,
                Activity::new(
                    &completion.id,
                    "summary",
                    "summarizing",
                    false,
                    json!({"observation":true}),
                )
                .linked(None, Some(agent.name.clone())),
            )?;
            let result = self
                .intent
                .summarize_completion(&agent.display_name, &completion.capture.screen)
                .await;
            // Recheck the work cycle as well as the terminal: identical text after
            // another turn must never make an old summary current again.
            if !self.agents.completion_current(&completion).await {
                self.trace(
                    &scope,
                    Activity::new(
                        &completion.id,
                        "summary",
                        "stale",
                        true,
                        json!({"observation":true}),
                    ),
                )?;
                self.agents.finish_completion(&completion, Err(false)).await;
                continue;
            }
            let delivered = match result {
                Ok(Some(speech)) => {
                    self.ask(|reply| Message::CompletionSummary {
                        scope: scope.clone(),
                        id: completion.id.clone(),
                        agent: agent.clone(),
                        speech,
                        reply,
                    })
                    .await
                }
                Ok(None) => {
                    self.trace(&scope, Activity::new(&completion.id, "summary", "ignored", true,
                        json!({"reason":"No finished assistant response in the current screen"})))?;
                    Ok(())
                }
                Err(error) => Err(error),
            };
            if let Err(error) = &delivered {
                self.trace(
                    &scope,
                    Activity::new(
                        &completion.id,
                        "summary",
                        "failed",
                        true,
                        json!({"reason":redact(&format!("{error:#}"))}),
                    ),
                )?;
            }
            self.agents
                .finish_completion(&completion, delivered.map_err(|_| true))
                .await;
        }
    }

    pub async fn rescan_questions(&self) -> Result<()> {
        self.agents.retry().await;
        Ok(())
    }
    async fn conversation_worker(self: Arc<Self>, mut turns: mpsc::Receiver<Turn>) -> Result<()> {
        while let Some(turn) = turns.recv().await {
            let result = self.process_turn(&turn).await;
            self.trace(
                &turn.scope,
                Activity::new(
                    &turn.token,
                    "utterance",
                    if result.is_ok() {
                        "responded"
                    } else {
                        "failed"
                    },
                    true,
                    json!({"error":result.as_ref().err().map(|e|e.to_string())}),
                ),
            )?;
            self.send(Message::VoiceDone {
                token: turn.token.clone(),
            })
            .await?;
            if let Err(error) = &result {
                self.send(Message::Error {
                    scope: turn.scope.clone(),
                    error: redact(&format!("{error:#}")),
                })
                .await?;
            }
            if let Some(completed) = turn.completed {
                let _ = completed.send(result);
            }
        }
        bail!("conversation queue closed")
    }
    async fn process_turn(&self, turn: &Turn) -> Result<()> {
        let text = match &turn.text {
            Some(text) => text.clone(),
            None => self.stt.stop_and_wait(&turn.token).await?,
        };
        self.voice(&turn.scope, &turn.token, "routing").await?;
        self.trace(
            &turn.scope,
            Activity::new(
                &turn.token,
                "utterance",
                "routing",
                false,
                json!({"transcript":text}),
            ),
        )?;
        self.record(&turn.scope, "user", None, "utterance", &text)
            .await?;
        let context = self.context(&turn.scope).await?;
        let (decision, evidence) = self
            .router
            .classify_with_questions(
                &text,
                &context.agents,
                context.active_agent.as_deref(),
                self.agents.routing_questions().await,
            )
            .await?;
        self.trace(
            &turn.scope,
            Activity::new(
                format!("{}/routing", turn.token),
                "routing",
                "classified",
                true,
                json!({"result":evidence}),
            )
            .linked(Some(turn.token.clone()), None),
        )?;
        if decision.operation == Operation::RepeatLast {
            return self.repeat_last().await;
        }
        if decision.operation == Operation::Clarify {
            return self
                .reply(
                    &turn.scope,
                    decision
                        .clarification
                        .as_deref()
                        .unwrap_or("Which agent do you mean?"),
                )
                .await;
        }
        let agent = decision
            .agent
            .context("routing omitted its selected agent")?;
        let current = self.context(&turn.scope).await?;
        if !current
            .agents
            .iter()
            .any(|a| a.name == agent.name && a.instance_id == agent.instance_id)
        {
            return self
                .reply(
                    &turn.scope,
                    "That agent disconnected while I was routing the request. Please try again.",
                )
                .await;
        }
        // A blocked question must receive an answer, never another prompt.
        if matches!(
            decision.operation,
            Operation::AnswerQuestion
                | Operation::SendToAgent
                | Operation::Approve
                | Operation::Deny
        ) {
            if let Some(question) = self.agents.question(&agent.name).await.filter(|q| {
                q.question_kind == "question" || decision.operation == Operation::AnswerQuestion
            }) {
                if turn.permissions.get(&agent.name) != Some(&question.question_id) {
                    return self.reply(&turn.scope,"That question was not current when you started speaking. Please answer the latest question.").await;
                }
                let (answer, evidence) = self.router.answer_question(&text, &question).await?;
                self.trace(
                    &turn.scope,
                    Activity::new(
                        format!("{}/question", turn.token),
                        "routing",
                        "classified",
                        true,
                        json!({"question_id":question.question_id,"result":evidence}),
                    )
                    .linked(Some(turn.token.clone()), Some(agent.name.clone())),
                )?;
                if matches!(answer, crate::question::Answer::Clarify) {
                    return self.reply(&turn.scope,"Please say an option number, its label, or your own answer if offered.").await;
                }
                if self
                    .agents
                    .question(&agent.name)
                    .await
                    .is_none_or(|q| q.question_id != question.question_id)
                {
                    return self
                        .reply(
                            &turn.scope,
                            "That question has changed. Please answer the latest question.",
                        )
                        .await;
                }
                let value = json!({"question_id":question.question_id,"answer":answer});
                let result = self
                    .operation(
                        &turn.scope,
                        &agent,
                        OperationKind::Answer,
                        value,
                        Some(turn.token.clone()),
                    )
                    .await?;
                return self
                    .reply(
                        &turn.scope,
                        &crate::routing::acknowledgement(
                            Operation::AnswerQuestion,
                            &agent.display_name,
                            &result,
                        ),
                    )
                    .await;
            }
            if decision.operation == Operation::AnswerQuestion {
                return self
                    .reply(&turn.scope, "That agent has no supported pending question.")
                    .await;
            }
        }
        let (name, mut arguments) = match decision.operation {
            Operation::SendToAgent => ("send_to_agent", json!({"agent_id":agent.name})),
            Operation::ReadAgent => ("read_agent", json!({"agent_id":agent.name})),
            Operation::Cancel => ("cancel_agent", json!({"agent_id":agent.name})),
            Operation::Approve | Operation::Deny => {
                let Some(question) = self
                    .agents
                    .question(&agent.name)
                    .await
                    .filter(|q| q.question_kind == "permission")
                else {
                    return self
                        .reply(&turn.scope, "That agent has no pending permission request.")
                        .await;
                };
                let question_id = permission_for_turn(&turn.permissions, &agent.name, &question)?;
                (
                    "respond_to_agent",
                    json!({"agent_id":agent.name,"approve":decision.operation==Operation::Approve,"question_id":question_id}),
                )
            }
            _ => unreachable!(),
        };
        arguments["expected_instance_id"] = json!(agent.instance_id);
        arguments["expected_state_change_seq"] = json!(agent.state_change_seq);
        let trace_id = format!("{}/tool", turn.token);
        let call = ToolCall {
            id: trace_id.clone(),
            name: name.into(),
            arguments,
        };
        self.trace(
            &turn.scope,
            Activity::new(
                &trace_id,
                name,
                "executing",
                false,
                json!({"arguments":call.arguments}),
            )
            .linked(Some(turn.token.clone()), Some(agent.name.clone())),
        )?;
        // Exactly one execution. Neither the router nor the request writer can retry it.
        let result = self
            .execute_tool(&turn.scope, &call, &trace_id, &HashSet::new(), &text)
            .await;
        self.trace(
            &turn.scope,
            Activity::new(
                &trace_id,
                name,
                if result.is_ok() { "returned" } else { "failed" },
                true,
                match &result {
                    Ok(value) => json!({"result":value}),
                    Err(error) => json!({"error":redact(&error.to_string())}),
                },
            ),
        )?;
        let result = match result {
            Ok(value) => value,
            Err(error) => {
                return self
                    .reply(
                        &turn.scope,
                        &format!(
                            "I couldn't complete that request: {}",
                            redact(&error.to_string())
                        ),
                    )
                    .await
            }
        };
        if decision.operation == Operation::ReadAgent {
            let questions = self.agents.questions().await;
            self.send(Message::ObservedQuestions {
                scope: turn.scope.clone(),
                questions,
            })
            .await?;
            if self
                .ask(|reply| Message::ReadObservedQuestion {
                    scope: turn.scope.clone(),
                    agent_name: agent.name.clone(),
                    reply,
                })
                .await?
            {
                return Ok(());
            }
            self.record(
                &turn.scope,
                "tool",
                Some(&agent.name),
                "tool_result",
                &result.to_string(),
            )
            .await?;
            let speech = if result["status"] == "pending" {
                format!(
                    "{} is still working; a fresh result is not available yet.",
                    agent.display_name
                )
            } else {
                let output = result["output"].as_str().unwrap_or_default();
                if output.trim().is_empty() {
                    format!(
                        "{} is {}. No response is available yet.",
                        agent.display_name,
                        result["status"].as_str().unwrap_or("connected")
                    )
                } else {
                    self.intent.summarize(&agent.display_name, output).await?
                }
            };
            self.reply(&turn.scope, &speech).await
        } else {
            self.reply(
                &turn.scope,
                &crate::routing::acknowledgement(decision.operation, &agent.display_name, &result),
            )
            .await
        }
    }
    async fn execute_tool(
        &self,
        scope: &Scope,
        call: &ToolCall,
        trace_id: &str,
        sent_agents: &HashSet<String>,
        utterance: &str,
    ) -> Result<Value> {
        let context = self.context(scope).await?;
        if call.name == "list_agents" {
            return Ok(crate::intent::routing_agents(&context.agents));
        }
        let name = string_arg(&call.arguments, "agent_id")?;
        let agent = context
            .agents
            .iter()
            .find(|a| a.name == name)
            .with_context(|| format!("Unknown agent_id {name:?}. Use an exact agent_id from the current roster, not a display name: {}", crate::intent::routing_agents(&context.agents)))?;
        if call.arguments["expected_instance_id"]
            .as_str()
            .is_some_and(|id| id != agent.instance_id)
            || call
                .arguments
                .get("expected_state_change_seq")
                .is_some_and(|seq| *seq != json!(agent.state_change_seq))
        {
            bail!("The selected agent changed while routing; please try again")
        }
        // Assign an agent lane only after validating the actual target. Invalid
        // arguments remain visible on the Orchrd lane, never as invented agents.
        self.trace(
            scope,
            Activity::new(trace_id, &call.name, "executing", false, json!({}))
                .linked(None, Some(agent.name.clone())),
        )?;
        match call.name.as_str() {
            "read_agent" => {
                let result = if let Some(reason) = pending_read_reason(agent, sent_agents) {
                    json!({"agent_id":name,"status":"pending","reason":reason,
                        "message":"A fresh response is not available yet. Do not present an earlier result as this request's answer."})
                } else {
                    let lines = call.arguments["lines"]
                        .as_u64()
                        .unwrap_or(100)
                        .clamp(10, 300) as usize;
                    let output = self.agents.read(name).await?;
                    json!({"agent_id":name,"status":agent.status,"output":output.chars().rev().take(lines * 120).collect::<String>().chars().rev().collect::<String>()})
                };
                self.ask(|reply| Message::Addressed {
                    scope: scope.clone(),
                    agent_name: name.into(),
                    instance_id: agent.instance_id.clone(),
                    reply,
                })
                .await?;
                Ok(result)
            }
            "send_to_agent" => {
                if sent_agents.contains(name) {
                    bail!("a send to this agent was already attempted in this turn; do not resend");
                }
                let request = self
                    .intent
                    .draft_agent_request(utterance, &agent.display_name)
                    .await?;
                self.operation(
                    scope,
                    agent,
                    OperationKind::Send,
                    json!({"agent_id":name,"request":request}),
                    Some(trace_id.into()),
                )
                .await
            }
            "respond_to_agent" => {
                let approve = call.arguments["approve"]
                    .as_bool()
                    .context("respond_to_agent omitted approve")?;
                self.operation(
                    scope,
                    agent,
                    if approve {
                        OperationKind::Approve
                    } else {
                        OperationKind::Deny
                    },
                    call.arguments.clone(),
                    Some(trace_id.into()),
                )
                .await
            }
            "cancel_agent" => {
                self.operation(
                    scope,
                    agent,
                    OperationKind::Cancel,
                    call.arguments.clone(),
                    Some(trace_id.into()),
                )
                .await
            }
            other => bail!("unknown conversation tool {other}"),
        }
    }
    async fn operation(
        &self,
        scope: &Scope,
        target: &crate::agent::AgentInfo,
        kind: OperationKind,
        request: Value,
        parent: Option<String>,
    ) -> Result<Value> {
        let name = target.name.as_str();
        let (completed, rx) = oneshot::channel();
        let (id, _registered_agent) = self
            .ask(|registered| Message::Begin {
                scope: scope.clone(),
                agent_name: name.into(),
                kind,
                request: request.clone(),
                parent,
                registered,
                completed,
            })
            .await?;
        let delivery = timeout(Duration::from_secs(20), async {
            self.context(scope).await?;
            match kind {
                OperationKind::Send => {
                    self.agents
                        .command(
                            target,
                            "prompt",
                            json!({"text":string_arg(&request,"request")?}),
                        )
                        .await
                }
                OperationKind::Answer => {
                    self.agents
                        .command(target, "answer_question", request.clone())
                        .await
                }
                OperationKind::Approve | OperationKind::Deny => {
                    self.agents
                        .command(
                            target,
                            "permission",
                            json!({"approve":matches!(kind,OperationKind::Approve),"question_id":string_arg(&request,"question_id")?}),
                        )
                        .await
                }
                OperationKind::Cancel => self.agents.command(target, "cancel", json!({})).await,
            }
        })
        .await
        .unwrap_or_else(|_| {
            Err(anyhow::anyhow!(
                "delivery deadline elapsed; outcome is unconfirmed"
            ))
        });
        self.send(Message::Delivered {
            id,
            result: delivery,
        })
        .await?;
        rx.await.context("operation confirmation was interrupted")
    }
    async fn speech_worker(
        tts: TtsClient,
        mut policy: watch::Receiver<Snapshot>,
        messages: mpsc::Sender<Message>,
        mut queue: mpsc::Receiver<Speech>,
    ) -> Result<()> {
        while let Some(speech) = queue.recv().await {
            while speech.current(&policy.borrow()) && policy.borrow().voice_state != "idle" {
                policy.changed().await.context("speech policy closed")?;
            }
            if !speech.current(&policy.borrow()) {
                messages
                    .send(Message::Activity {
                        scope: speech.scope.clone(),
                        activity: Activity::new(
                            format!("{}/speech", speech.id),
                            "speech",
                            "skipped",
                            true,
                            json!({"reason":"speech policy changed"}),
                        ),
                    })
                    .await
                    .context("coordinator stopped")?;
                continue;
            }
            messages
                .send(Message::Activity {
                    scope: speech.scope.clone(),
                    activity: Activity::new(
                        format!("{}/speech", speech.id),
                        "speech",
                        "speaking",
                        false,
                        json!({}),
                    ),
                })
                .await
                .context("coordinator stopped")?;
            let result = {
                let playback = tts.speak(&speech.id, &speech.text);
                tokio::pin!(playback);
                loop {
                    tokio::select! {
                        result=&mut playback=>break result,
                        changed=policy.changed()=>{
                            changed.context("speech policy closed")?;
                            if !speech.current(&policy.borrow()) || policy.borrow().voice_state=="listening" {
                                break Err(anyhow::anyhow!("speech interrupted"));
                            }
                        }
                    }
                }
            };
            if result.is_err() {
                let _ = tts.cancel().await;
            }
            messages
                .send(Message::Spoken { speech, result })
                .await
                .context("coordinator stopped")?;
        }
        bail!("speech queue closed")
    }
}

fn permission_for_turn<'a>(
    permissions: &'a HashMap<String, String>,
    agent: &str,
    question: &crate::question::Question,
) -> Result<&'a str> {
    let id = permissions
        .get(agent)
        .context("No permission was pending when this utterance began")?;
    if question.question_kind != "permission" || *id != question.question_id {
        bail!("That permission request has changed; please answer the latest request")
    }
    Ok(id)
}

fn pending_read_reason(
    agent: &crate::agent::AgentInfo,
    sent_agents: &HashSet<String>,
) -> Option<&'static str> {
    if sent_agents.contains(&agent.name) {
        Some("sent_in_current_turn")
    } else {
        None
    }
}

#[cfg(test)]
mod readback_tests {
    use super::*;

    #[test]
    fn queued_or_routing_turn_cannot_approve_a_new_permission() {
        let permissions = HashMap::from([("agent".into(), "old".into())]);
        let mut question: crate::question::Question = serde_json::from_value(json!({"question_id":"old","question_kind":"permission","title":"Approval","body":"Write?","options":[],"multiple":false,"allow_custom":false})).unwrap();
        assert_eq!(
            permission_for_turn(&permissions, "agent", &question).unwrap(),
            "old"
        );
        question.question_id = "new".into();
        assert!(permission_for_turn(&permissions, "agent", &question).is_err());
        assert!(permission_for_turn(&HashMap::new(), "agent", &question).is_err());
    }
    #[test]
    fn a_new_send_cannot_read_a_previous_result_in_the_same_turn() {
        let agent = crate::agent::AgentInfo {
            display_name: "Coder".into(),
            name: "coder".into(),
            kind: None,
            instance_id: "instance".into(),
            cwd: None,
            status: "done".into(),
            state_change_seq: Some(2),
        };
        let mut sent = HashSet::new();
        sent.insert("coder".into());
        assert_eq!(
            pending_read_reason(&agent, &sent),
            Some("sent_in_current_turn")
        );
        sent.clear();
        assert_eq!(pending_read_reason(&agent, &sent), None);
        let working = crate::agent::AgentInfo {
            status: "working".into(),
            ..agent
        };
        assert_eq!(pending_read_reason(&working, &sent), None);
        let blocked = crate::agent::AgentInfo {
            status: "blocked".into(),
            ..working
        };
        assert_eq!(pending_read_reason(&blocked, &sent), None);
    }
}
fn string_arg<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("tool call omitted {key}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::UnixListener,
    };

    #[tokio::test]
    async fn speech_waits_for_each_playback_receipt_and_skips_stale_lifetime() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("tts.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let snapshot = Snapshot::initial(AnnouncementMode::Auto);
        let (_policy, rx) = watch::channel(snapshot.clone());
        let (messages, mut results) = mpsc::channel(256);
        let (speech, queue) = mpsc::channel(256);
        let worker = tokio::spawn(Controller::speech_worker(
            TtsClient::new(socket),
            rx,
            messages,
            queue,
        ));
        let make = |id: &str| Speech {
            id: id.into(),
            text: format!("Result {id}."),
            scope: snapshot.scope.clone(),
            epoch: snapshot.speech_epoch,
            automatic: true,
            question_id: None,
            completion_agent: None,
        };
        let mut stale = make("stale");
        stale.scope.epoch = "old lifetime".into();
        speech.try_send(stale).unwrap();
        speech.try_send(make("first")).unwrap();
        speech.try_send(make("second")).unwrap();
        for id in ["first", "second"] {
            let (stream, _) = timeout(Duration::from_secs(2), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let mut stream = BufReader::new(stream);
            let mut line = String::new();
            stream.read_line(&mut line).await.unwrap();
            assert_eq!(serde_json::from_str::<Value>(&line).unwrap()["id"], id);
            // The next speech must not reach TTS until this playback completes.
            assert!(timeout(Duration::from_millis(30), listener.accept())
                .await
                .is_err());
            stream
                .get_mut()
                .write_all(b"{\"ok\":true}\n")
                .await
                .unwrap();
            loop {
                match timeout(Duration::from_secs(2), results.recv())
                    .await
                    .unwrap()
                    .unwrap()
                {
                    Message::Spoken { speech, result } => {
                        assert_eq!(speech.id, id);
                        result.unwrap();
                        break;
                    }
                    Message::Activity { activity, .. } => {
                        assert!(matches!(activity.state.as_str(), "speaking" | "skipped"))
                    }
                    _ => panic!("unexpected worker message"),
                }
            }
        }
        worker.abort();
    }
}
