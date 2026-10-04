use crate::activity::Activity;
pub use crate::coordinator::{Event, Scope, Snapshot};
use crate::{
    agent_bridge::AgentBridge,
    config::{AnnouncementMode, Config},
    coordinator::{Coordinator, Message, OperationKind, Speech},
    db::Database,
    intent::{redact, IntentClient, ToolCall},
    routing::{Operation, Router},
    stt::SttClient,
    tts::TtsClient,
};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{collections::HashSet, path::PathBuf, sync::Arc, time::Duration};
use tokio::{
    sync::{broadcast, mpsc, oneshot, watch, Mutex, Notify},
    task::JoinSet,
    time::{sleep, timeout},
};
use uuid::Uuid;

struct Recording {
    scope: Scope,
    token: String,
    path: PathBuf,
}
struct Turn {
    scope: Scope,
    token: String,
    text: Option<String>,
    path: Option<PathBuf>,
    completed: Option<oneshot::Sender<Result<()>>>,
}
struct Runtime {
    coordinator: Coordinator,
    messages: mpsc::UnboundedReceiver<Message>,
    turns: mpsc::UnboundedReceiver<Turn>,
    speech: mpsc::UnboundedReceiver<Speech>,
}
pub struct Controller {
    pub config: Arc<Config>,
    db: Arc<Database>,
    agents: AgentBridge,
    intent: IntentClient,
    router: Router,
    tts: TtsClient,
    stt: SttClient,
    messages: mpsc::UnboundedSender<Message>,
    turns: mpsc::UnboundedSender<Turn>,
    snapshots: watch::Receiver<Snapshot>,
    events: broadcast::Sender<Event>,
    summaries: Arc<Notify>,
    recording: Mutex<Option<Recording>>,
    runtime: Mutex<Option<Runtime>>,
}
impl Controller {
    pub async fn new(config: Config) -> Result<Arc<Self>> {
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
        let (messages, messages_rx) = mpsc::unbounded_channel();
        let (turns, turns_rx) = mpsc::unbounded_channel();
        let (speech, speech_rx) = mpsc::unbounded_channel();
        let summaries = Arc::new(Notify::new());
        let coordinator = Coordinator::new(
            snapshot,
            db.clone(),
            events.clone(),
            snapshots_tx,
            speech,
            summaries.clone(),
        );
        Ok(Arc::new(Self {
            agents: AgentBridge::default(),
            intent: IntentClient::new(&config)?,
            router: Router::new(&config.routing)?,
            tts: TtsClient::from_config(&config)?,
            stt: SttClient::new(&config)?,
            config: Arc::new(config),
            db,
            messages,
            turns,
            snapshots,
            events,
            summaries,
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
    fn send(&self, message: Message) -> Result<()> {
        self.messages.send(message).context("coordinator stopped")
    }
    fn trace(&self, scope: &Scope, activity: Activity) -> Result<()> {
        self.send(Message::Activity {
            scope: scope.clone(),
            activity,
        })
    }
    async fn ask<T>(&self, make: impl FnOnce(oneshot::Sender<Result<T>>) -> Message) -> Result<T> {
        let (tx, rx) = oneshot::channel();
        self.send(make(tx))?;
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
        self.voice(&scope, &token, "listening").await?;
        let _ = self.tts.cancel().await;
        match self.stt.start(&token).await {
            Ok(path) => {
                *recording = Some(Recording { scope, token, path });
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
                self.send(Message::VoiceDone { token })?;
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
            })?;
            return Err(error);
        }
        self.turns
            .send(Turn {
                scope: recording.scope,
                token: recording.token,
                text: None,
                path: Some(recording.path),
                completed: None,
            })
            .context("conversation worker stopped")
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
            })?;
            result?;
        }
        Ok(())
    }
    pub async fn dispatch_transcript(&self, text: &str) -> Result<()> {
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
            .send(Turn {
                scope,
                token,
                text: Some(text.into()),
                path: None,
                completed: Some(completed),
            })
            .context("conversation worker stopped")?;
        rx.await
            .context("conversation worker stopped before replying")?
    }
    pub async fn resolve_approval(&self, id: &str, approve: bool) -> Result<()> {
        let target: Value = serde_json::from_str(id).context("invalid approval reference")?;
        let scope: Scope = serde_json::from_value(target["scope"].clone())?;
        let agent = string_arg(&target, "agent_name")?;
        let current = self.context(&scope).await?;
        if !current.agents.iter().any(|a| {
            a.name == agent && Some(a.instance_id.as_str()) == target["instance_id"].as_str()
        }) {
            bail!("The agent associated with this approval is no longer present")
        }
        let result = self
            .operation(
                &scope,
                agent,
                if approve {
                    OperationKind::Approve
                } else {
                    OperationKind::Deny
                },
                json!({"approve":approve}),
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
        let agents = self.agents.clone();
        let bridge_messages = self.messages.clone();
        tasks.spawn(async move { agents.serve(bridge_messages).await });
        tasks.spawn(self.clone().poll_worker());
        tasks.spawn(self.clone().summary_worker());
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
            let agents = self.agents.snapshot().await;
            self.send(Message::Polled { scope, agents })?;
            sleep(Duration::from_millis(200)).await;
        }
    }
    async fn summary_worker(self: Arc<Self>) -> Result<()> {
        loop {
            // Create the notification future before querying, avoiding a lost wakeup.
            let notified = self.summaries.notified();
            let Some(job) = self.db.next_response()? else {
                notified.await;
                continue;
            };
            let mut summary = None;
            let scope = Scope {
                session: job.session.clone(),
                epoch: job.epoch.clone(),
            };
            self.trace(
                &scope,
                Activity::new(&job.id, "summary", "summarizing", false, json!({})),
            )?;
            let display_name = self
                .snapshots
                .borrow()
                .agents
                .iter()
                .find(|agent| agent.name == job.agent_name)
                .map(|agent| agent.display_name.clone())
                .unwrap_or_else(|| job.agent_name.clone());
            if !job.output.trim().is_empty() {
                for attempt in 0..2 {
                    let result = if job.kind == "agent_question"
                        && serde_json::from_str::<crate::question::Question>(&job.output)
                            .is_ok_and(|q| q.question_kind == "question")
                    {
                        Ok(
                            serde_json::from_str::<crate::question::Question>(&job.output)?
                                .speech(),
                        )
                    } else if job.kind == "agent_question" {
                        self.intent
                            .describe_blocker(&display_name, &job.output)
                            .await
                    } else {
                        self.intent.summarize(&display_name, &job.output).await
                    };
                    match result {
                        Ok(speech) => {
                            summary = Some(speech);
                            break;
                        }
                        Err(error) => {
                            self.trace(
                                &scope,
                                Activity::new(
                                    &job.id,
                                    "summary",
                                    "retrying",
                                    false,
                                    json!({"error":error.to_string(),"attempt":attempt+1}),
                                ),
                            )?;
                            tracing::warn!(id=%job.id,%error,attempt,"summary failed");
                            if attempt == 0 {
                                sleep(Duration::from_secs(1)).await;
                            }
                        }
                    }
                }
            }
            let fallback = summary.is_none();
            let speech = summary.unwrap_or_else(|| {
                format!(
                    "{} has new output, but I couldn't summarize it. Open the agent window for details.",
                    display_name
                )
            });
            self.trace(
                &scope,
                Activity::new(
                    &job.id,
                    "summary",
                    "persisting",
                    false,
                    json!({"speech":speech,"fallback":fallback}),
                ),
            )?;
            self.ask(|reply| Message::Summarized { job, speech, reply })
                .await?;
        }
    }
    async fn conversation_worker(
        self: Arc<Self>,
        mut turns: mpsc::UnboundedReceiver<Turn>,
    ) -> Result<()> {
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
            })?;
            if let Err(error) = &result {
                self.send(Message::Error {
                    scope: turn.scope.clone(),
                    error: redact(&format!("{error:#}")),
                })?;
            }
            if let Some(completed) = turn.completed {
                let _ = completed.send(result);
            }
        }
        bail!("conversation queue closed")
    }
    async fn process_turn(&self, turn: &Turn) -> Result<()> {
        let text = match (&turn.text, &turn.path) {
            (Some(text), _) => text.clone(),
            (_, Some(path)) => self.stt.stop_and_wait(path).await?,
            _ => bail!("empty conversation turn"),
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
                self.agents.questions().await,
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
            if let Some(question) = self
                .agents
                .question(&agent.name)
                .await
                .filter(|q| q.question_kind == "question")
            {
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
                        &agent.name,
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
        let (name, arguments) = match decision.operation {
            Operation::SendToAgent => ("send_to_agent", json!({"agent_id":agent.name})),
            Operation::ReadAgent => ("read_agent", json!({"agent_id":agent.name})),
            Operation::Cancel => ("cancel_agent", json!({"agent_id":agent.name})),
            Operation::Approve | Operation::Deny => {
                if !current
                    .agents
                    .iter()
                    .any(|a| a.name == agent.name && a.status == "blocked")
                {
                    return self
                        .reply(&turn.scope, "That agent has no pending permission request.")
                        .await;
                }
                (
                    "respond_to_agent",
                    json!({"agent_id":agent.name,"approve":decision.operation==Operation::Approve}),
                )
            }
            _ => unreachable!(),
        };
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
        // Assign an agent lane only after validating the actual target. Invalid
        // arguments remain visible on the Orchrd lane, never as invented agents.
        self.trace(
            scope,
            Activity::new(trace_id, &call.name, "executing", false, json!({}))
                .linked(None, Some(agent.name.clone())),
        )?;
        match call.name.as_str() {
            "read_agent" | "latest_result" => {
                let result = if let Some(reason) =
                    pending_read_reason(agent, sent_agents, call.name == "latest_result")
                {
                    json!({"agent_id":name,"status":"pending","reason":reason,
                        "message":"A fresh response is not available yet. Do not present an earlier result as this request's answer."})
                } else if call.name == "read_agent" {
                    let lines = call.arguments["lines"]
                        .as_u64()
                        .unwrap_or(100)
                        .clamp(10, 300) as usize;
                    let output = self.agents.read(name).await?;
                    json!({"agent_id":name,"status":agent.status,"output":output.chars().rev().take(lines * 120).collect::<String>().chars().rev().collect::<String>()})
                } else {
                    serde_json::to_value(self.db.latest_response(&scope.session, name)?)?
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
                    name,
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
                    name,
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
                    name,
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
        name: &str,
        kind: OperationKind,
        request: Value,
        parent: Option<String>,
    ) -> Result<Value> {
        let (completed, rx) = oneshot::channel();
        let (id, _agent) = self
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
                            name,
                            "prompt",
                            json!({"text":string_arg(&request,"request")?}),
                        )
                        .await
                }
                OperationKind::Answer => {
                    self.agents
                        .command(name, "answer_question", request.clone())
                        .await
                }
                OperationKind::Approve | OperationKind::Deny => {
                    self.agents
                        .command(
                            name,
                            "permission",
                            json!({"approve":matches!(kind,OperationKind::Approve)}),
                        )
                        .await
                }
                OperationKind::Cancel => self.agents.command(name, "cancel", json!({})).await,
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
        })?;
        rx.await.context("operation confirmation was interrupted")
    }
    async fn speech_worker(
        tts: TtsClient,
        mut policy: watch::Receiver<Snapshot>,
        messages: mpsc::UnboundedSender<Message>,
        mut queue: mpsc::UnboundedReceiver<Speech>,
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
                .context("coordinator stopped")?;
        }
        bail!("speech queue closed")
    }
}

fn pending_read_reason(
    agent: &crate::agent::AgentInfo,
    sent_agents: &HashSet<String>,
    latest_result: bool,
) -> Option<&'static str> {
    if sent_agents.contains(&agent.name) {
        Some("sent_in_current_turn")
    } else if latest_result && agent.status == "working" {
        Some("agent_working")
    } else if latest_result && agent.status == "blocked" {
        Some("agent_blocked")
    } else {
        None
    }
}

#[cfg(test)]
mod readback_tests {
    use super::*;

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
            pending_read_reason(&agent, &sent, true),
            Some("sent_in_current_turn")
        );
        assert_eq!(
            pending_read_reason(&agent, &sent, false),
            Some("sent_in_current_turn")
        );
        sent.clear();
        assert_eq!(pending_read_reason(&agent, &sent, true), None);
        let working = crate::agent::AgentInfo {
            status: "working".into(),
            ..agent
        };
        assert_eq!(
            pending_read_reason(&working, &sent, true),
            Some("agent_working")
        );
        assert_eq!(pending_read_reason(&working, &sent, false), None);
        let blocked = crate::agent::AgentInfo {
            status: "blocked".into(),
            ..working
        };
        assert_eq!(
            pending_read_reason(&blocked, &sent, true),
            Some("agent_blocked")
        );
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
        let (messages, mut results) = mpsc::unbounded_channel();
        let (speech, queue) = mpsc::unbounded_channel();
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
        };
        let mut stale = make("stale");
        stale.scope.epoch = "old lifetime".into();
        speech.send(stale).unwrap();
        speech.send(make("first")).unwrap();
        speech.send(make("second")).unwrap();
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
