use crate::{
    config::{AnnouncementMode, Config, Project},
    db::{AgentSession, Database},
    herdr::{agent_states, HerdrClient},
    intent::{redact, IntentClient, ToolCall},
    tts::{concise_speech, TtsClient},
    voxtype::Voxtype,
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};
use tokio::{
    sync::{broadcast, Mutex, RwLock},
    time::{sleep, Instant},
};
use uuid::Uuid;

const APPROVAL_CONFIRM_TIMEOUT: Duration = Duration::from_secs(3);
const APPROVAL_CONFIRM_POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub state: String,
    pub active_project: Option<String>,
    pub agents: Vec<AgentSession>,
    pub pending_results: usize,
    pub announcement_mode: AnnouncementMode,
    pub message: Option<String>,
}
#[derive(Debug, Clone)]
pub enum Event {
    Snapshot(Snapshot),
    Transcript(String),
    Completion { project: String, speech: String },
    Approval { id: String, message: String },
    Error(String),
}
struct Recording {
    path: PathBuf,
}
pub struct Controller {
    pub config: Arc<Config>,
    db: Arc<Database>,
    voxtype: Voxtype,
    herdr: HerdrClient,
    intent: IntentClient,
    tts: TtsClient,
    projects: RwLock<Vec<Project>>,
    state: RwLock<Snapshot>,
    recording: Mutex<Option<Recording>>,
    events: broadcast::Sender<Event>,
}
impl Controller {
    pub async fn new(config: Config) -> Result<Arc<Self>> {
        tokio::fs::create_dir_all(Config::runtime_dir()).await?;
        let data = Config::data_dir()?;
        tokio::fs::create_dir_all(&data).await?;
        let db = Arc::new(Database::open(&data.join("orchrd.sqlite3"))?);
        let projects = config.resolved_projects()?;
        db.sync_projects(&projects)?;
        let mode = db
            .setting("announcement_mode")?
            .and_then(|v| {
                if v == "auto" {
                    Some(AnnouncementMode::Auto)
                } else if v == "queued" {
                    Some(AnnouncementMode::Queued)
                } else {
                    None
                }
            })
            .unwrap_or(config.voice.announcement_mode);
        let agents = db.sessions()?;
        let pending = db.pending_results()?.len();
        let initial_state = reconciled_state(&agents, pending).to_owned();
        let active_project = db.setting("active_project")?;
        let (events, _) = broadcast::channel(128);
        let c = Arc::new(Self {
            voxtype: Voxtype::new(
                config.commands.voxtype.clone(),
                config.voxtype_state_file(),
                Config::runtime_dir(),
                config.voice.max_recording_seconds,
            ),
            herdr: HerdrClient::new(config.commands.herdr.clone(), config.herdr.session.clone()),
            intent: IntentClient::new(&config)?,
            tts: TtsClient::new(config.tts_socket()),
            config: Arc::new(config),
            db,
            projects: RwLock::new(projects),
            state: RwLock::new(Snapshot {
                state: initial_state,
                active_project,
                agents,
                pending_results: pending,
                announcement_mode: mode,
                message: None,
            }),
            recording: Mutex::new(None),
            events,
        });
        Ok(c)
    }
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }
    pub async fn snapshot(&self) -> Snapshot {
        self.state.read().await.clone()
    }
    async fn update(&self, state: &str, message: Option<String>) {
        let mut s = self.state.write().await;
        s.state = state.into();
        s.message = message;
        s.agents = self.db.sessions().unwrap_or_default();
        s.pending_results = self.db.pending_results().map(|x| x.len()).unwrap_or(0);
        let _ = self.events.send(Event::Snapshot(s.clone()));
    }
    pub async fn start_listening(&self) -> Result<()> {
        let _ = self.tts.cancel().await;
        let mut r = self.recording.lock().await;
        if r.is_some() {
            bail!("already recording")
        };
        let id = Uuid::new_v4().to_string();
        let path = self.voxtype.start(&id).await?;
        *r = Some(Recording { path });
        drop(r);
        self.update("listening", None).await;
        Ok(())
    }
    pub async fn stop_listening(self: &Arc<Self>) -> Result<()> {
        let rec = self
            .recording
            .lock()
            .await
            .take()
            .context("not recording")?;
        self.update("transcribing", None).await;
        let this = Arc::clone(self);
        tokio::spawn(async move {
            match this.voxtype.stop_and_wait(&rec.path).await {
                Ok(text) => {
                    let _ = this.events.send(Event::Transcript(text.clone()));
                    this.update("routing", Some(text.clone())).await;
                    if let Err(e) = this.dispatch_transcript(&text).await {
                        this.report_error(e).await
                    }
                }
                Err(e) => this.report_error(e).await,
            }
        });
        Ok(())
    }
    pub async fn cancel_listening(&self) -> Result<()> {
        self.recording.lock().await.take();
        self.voxtype.cancel().await?;
        self.update_reconciled(Some("Recording cancelled".into()))
            .await;
        Ok(())
    }
    pub async fn dispatch_transcript(&self, text: &str) -> Result<()> {
        let projects = self.config.resolved_projects()?;
        self.db.sync_projects(&projects)?;
        *self.projects.write().await = projects.clone();
        let active = self.state.read().await.active_project.clone();
        self.db
            .add_conversation_message("user", text, active.as_deref(), "utterance")?;
        let sessions = match self.refresh_live_sessions().await {
            Ok(sessions) => sessions,
            Err(error) => {
                tracing::debug!(%error, "could not refresh Herdr state before conversation");
                self.db.sessions()?
            }
        };
        let history = self
            .db
            .recent_conversation(self.config.conversation.max_messages)?;
        let mut messages =
            self.intent
                .conversation_messages(&history, &projects, &sessions, active.as_deref());
        for _ in 0..self.config.conversation.max_tool_rounds {
            let turn = self.intent.next_conversation_turn(&messages).await?;
            messages.push(turn.message);
            for call in turn.tool_calls {
                tracing::info!(
                    tool = %call.name,
                    tool_call_id = %call.id,
                    "conversation tool call selected"
                );
                let result = match self.execute_tool(&call, text).await {
                    Ok(value) => {
                        tracing::info!(
                            tool = %call.name,
                            tool_call_id = %call.id,
                            "conversation tool call succeeded"
                        );
                        if call.name == "speak" {
                            let speech = string_arg(&value, "speech")?;
                            return self.reply(speech, "reply").await;
                        }
                        if let Some(speech) = successful_action_speech(&call.name, &value) {
                            return self.reply(&speech, "action_result").await;
                        }
                        json!({"ok":true,"result":value})
                    }
                    Err(error) => {
                        let error = redact(&format!("{error:#}"));
                        tracing::warn!(
                            tool = %call.name,
                            tool_call_id = %call.id,
                            error = %error,
                            "conversation tool call failed"
                        );
                        if let Some(speech) = failed_action_speech(&call.name) {
                            return self.reply(speech, "action_error").await;
                        }
                        json!({"ok":false,"error":error})
                    }
                };
                let mut content = result.to_string();
                content = content
                    .chars()
                    .take(self.config.conversation.max_tool_output_chars)
                    .collect();
                messages.push(json!({
                    "role":"tool",
                    "tool_call_id":call.id,
                    "content":content
                }));
            }
        }
        bail!("conversation exceeded the configured tool-call limit")
    }
    async fn execute_tool(&self, call: &ToolCall, transcript: &str) -> Result<Value> {
        match call.name.as_str() {
            "speak" => {
                let speech = string_arg(&call.arguments, "speech")?;
                let kind = string_arg(&call.arguments, "kind")?;
                if !matches!(kind, "answer" | "clarification") {
                    bail!("speak kind must be answer or clarification")
                }
                Ok(json!({"speech":speech,"kind":kind}))
            }
            "list_projects" => Ok(json!(self
                .projects
                .read()
                .await
                .iter()
                .map(|project| json!({"id": project.id, "name": project.name}))
                .collect::<Vec<_>>())),
            "list_agents" => Ok(serde_json::to_value(self.refresh_live_sessions().await?)?),
            "read_agent" => {
                let project_id = string_arg(&call.arguments, "project_id")?;
                let lines = call
                    .arguments
                    .get("lines")
                    .and_then(Value::as_u64)
                    .unwrap_or(100)
                    .clamp(10, 300) as usize;
                let session = self
                    .db
                    .session(project_id)?
                    .with_context(|| format!("project {project_id} has no agent"))?;
                self.set_active_project(project_id).await?;
                Ok(
                    json!({"project_id":project_id,"status":session.status,"output":self.herdr.read(&session.agent_name, lines).await?}),
                )
            }
            "start_session" => {
                let project_id = string_arg(&call.arguments, "project_id")?;
                let session = self.open_session(project_id, false).await?;
                Ok(json!({"project_id":project_id,"session":session,"resumed":false}))
            }
            "resume_session" => {
                let project_id = string_arg(&call.arguments, "project_id")?;
                let session = self.open_session(project_id, true).await?;
                Ok(json!({"project_id":project_id,"session":session,"resumed":true}))
            }
            "send_to_agent" => {
                let project_id = string_arg(&call.arguments, "project_id")?;
                let instruction = string_arg(&call.arguments, "request")?;
                let fresh = call
                    .arguments
                    .get("fresh_session")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let run_id = self
                    .start_task(project_id, instruction, transcript, fresh)
                    .await?;
                Ok(json!({
                    "project_id":project_id,
                    "outcome":"sent",
                    "receipt":run_id
                }))
            }
            "latest_result" => {
                let project_id = string_arg(&call.arguments, "project_id")?;
                self.set_active_project(project_id).await?;
                Ok(serde_json::to_value(self.db.latest_run(project_id)?)?)
            }
            "respond_to_agent" => {
                let project_id = string_arg(&call.arguments, "project_id")?;
                let approve = call
                    .arguments
                    .get("approve")
                    .and_then(Value::as_bool)
                    .context("respond_to_agent omitted approve")?;
                self.resolve_project_decision(project_id, approve).await?;
                Ok(json!({"project_id":project_id,"approved":approve}))
            }
            "cancel_agent" => {
                let project_id = string_arg(&call.arguments, "project_id")?;
                let session = self
                    .db
                    .session(project_id)?
                    .with_context(|| format!("project {project_id} has no agent"))?;
                self.herdr.send_keys(&session.agent_name, &["esc"]).await?;
                self.db.set_session_status(project_id, "idle")?;
                self.update_reconciled(Some(format!("Cancelled {project_id}")))
                    .await;
                Ok(json!({"project_id":project_id,"cancelled":true}))
            }
            other => bail!("unknown conversation tool {other}"),
        }
    }
    pub async fn resolve_approval(&self, approval_id: &str, approve: bool) -> Result<()> {
        let project_id = approval_id
            .strip_prefix("agent:")
            .context("approval did not originate from a coding agent")?;
        self.resolve_agent_decision(project_id, approve).await
    }
    async fn resolve_project_decision(&self, project_id: &str, approve: bool) -> Result<()> {
        self.resolve_agent_decision(project_id, approve).await
    }
    async fn resolve_agent_decision(&self, project_id: &str, approve: bool) -> Result<()> {
        let session = self
            .db
            .session(project_id)?
            .with_context(|| format!("project {project_id} has no agent"))?;
        let live_status = self.live_agent_status(&session.agent_name).await?;
        if live_status.as_deref() != Some("blocked") {
            bail!("{project_id} is not waiting for approval")
        }
        // The monitor may not have persisted the latest Herdr transition yet.
        if session.status != "blocked" {
            self.db.set_session_status(project_id, "blocked")?;
        }
        let terminal = self.herdr.read(&session.agent_name, 100).await?;
        let keys = if approve {
            approval_keys(&terminal)
        } else {
            vec!["esc"]
        };
        self.herdr.send_keys(&session.agent_name, &keys).await?;

        let deadline = Instant::now() + APPROVAL_CONFIRM_TIMEOUT;
        let confirmed_status = loop {
            let status = self.live_agent_status(&session.agent_name).await?;
            if status.as_deref() != Some("blocked") {
                break status;
            }
            if Instant::now() >= deadline {
                bail!("Herdr still reports {project_id} as blocked; the response was not accepted")
            }
            sleep(APPROVAL_CONFIRM_POLL_INTERVAL).await;
        };
        self.db
            .set_session_status(project_id, confirmed_status.as_deref().unwrap_or("unknown"))?;
        let decision = if approve { "Approved" } else { "Denied" };
        self.update_reconciled(Some(format!("{decision} request for {project_id}")))
            .await;
        Ok(())
    }
    async fn live_agent_status(&self, agent_name: &str) -> Result<Option<String>> {
        Ok(agent_states(&self.herdr.agent_list().await?)
            .into_iter()
            .find_map(|(name, status)| (name == agent_name).then_some(status)))
    }
    async fn refresh_live_sessions(&self) -> Result<Vec<AgentSession>> {
        let states: HashMap<_, _> = agent_states(&self.herdr.agent_list().await?)
            .into_iter()
            .collect();
        let mut sessions = self.db.sessions()?;
        for session in &mut sessions {
            if let Some(status) = states.get(&session.agent_name) {
                if session.status != *status {
                    self.db.set_session_status(&session.project_id, status)?;
                    session.status.clone_from(status);
                }
            }
        }
        Ok(sessions)
    }
    async fn start_task(
        &self,
        project_id: &str,
        instruction: &str,
        transcript: &str,
        fresh: bool,
    ) -> Result<String> {
        let project = self.project(project_id).await?;
        let session = if !fresh {
            self.db.session(project_id)?
        } else {
            None
        };
        let mut session = match session {
            Some(s) => s,
            None => {
                let mut launch_project = project.clone();
                if fresh {
                    launch_project.id =
                        format!("{}-{}", project.id, &Uuid::new_v4().to_string()[..8]);
                }
                let mut s = self.herdr.create_session(&launch_project).await?;
                s.project_id = project.id.clone();
                self.db.upsert_session(&s)?;
                s
            }
        };
        let terminal_baseline = self
            .herdr
            .read(&session.agent_name, self.config.summary.max_terminal_lines)
            .await
            .unwrap_or_default();
        self.herdr.prompt(&session.agent_name, instruction).await?;
        session.status = "working".into();
        self.db.upsert_session(&session)?;
        let run = Uuid::new_v4().to_string();
        self.db.create_run(
            &run,
            project_id,
            transcript,
            instruction,
            &terminal_baseline,
        )?;
        self.set_active_project(project_id).await?;
        self.update_reconciled(Some(format!("Sent to {}", project.name)))
            .await;
        Ok(run)
    }
    async fn open_session(&self, project_id: &str, resume: bool) -> Result<AgentSession> {
        let project = self.project(project_id).await?;
        let mut launch_project = project.clone();
        launch_project.id = format!("{}-{}", project.id, &Uuid::new_v4().to_string()[..8]);
        let mut session = if resume {
            self.herdr.resume_session(&launch_project).await?
        } else {
            self.herdr.create_session(&launch_project).await?
        };
        session.project_id = project.id.clone();
        self.db.upsert_session(&session)?;
        self.set_active_project(project_id).await?;
        let message = if resume {
            format!("Resumed the latest {} session", project.name)
        } else {
            format!("Started a new {} session", project.name)
        };
        self.update_reconciled(Some(message)).await;
        Ok(session)
    }
    async fn project(&self, id: &str) -> Result<Project> {
        self.projects
            .read()
            .await
            .iter()
            .find(|p| p.id == id)
            .cloned()
            .with_context(|| format!("unknown project {id}"))
    }
    async fn set_active_project(&self, project_id: &str) -> Result<()> {
        self.project(project_id).await?;
        self.db.set_setting("active_project", project_id)?;
        self.state.write().await.active_project = Some(project_id.into());
        Ok(())
    }
    pub async fn set_announcement_mode(&self, mode: AnnouncementMode) -> Result<()> {
        self.db.set_setting(
            "announcement_mode",
            if mode == AnnouncementMode::Auto {
                "auto"
            } else {
                "queued"
            },
        )?;
        self.state.write().await.announcement_mode = mode;
        self.update_reconciled(None).await;
        Ok(())
    }
    pub async fn speak_next(&self) -> Result<()> {
        let pending = self.db.pending_results()?;
        let run = pending.first().context("no pending result")?;
        self.tts
            .speak(&run.id, run.result.as_deref().unwrap_or("No result"))
            .await?;
        self.db.mark_announced(&run.id)?;
        self.update_reconciled(None).await;
        Ok(())
    }
    pub async fn dismiss_next(&self) -> Result<()> {
        let pending = self.db.pending_results()?;
        let run = pending.first().context("no pending result")?;
        self.db.mark_announced(&run.id)?;
        self.update_reconciled(None).await;
        Ok(())
    }
    async fn announce_completion(
        &self,
        run_id: &str,
        project_id: &str,
        speech: &str,
    ) -> Result<()> {
        let mut announced = false;
        if self.state.read().await.announcement_mode == AnnouncementMode::Auto {
            match self.tts.speak(run_id, speech).await {
                Ok(()) => {
                    self.db.mark_announced(run_id)?;
                    announced = true;
                }
                Err(error) => {
                    tracing::warn!(%error, "could not auto-speak completion; leaving it queued");
                }
            }
        }
        let _ = self.events.send(Event::Completion {
            project: project_id.into(),
            speech: speech.into(),
        });
        if announced {
            self.update_reconciled(Some(speech.into())).await;
        }
        Ok(())
    }
    async fn speak_now(&self, id: &str, speech: &str) -> Result<()> {
        self.tts.speak(id, speech).await
    }
    async fn reply(&self, speech: &str, kind: &str) -> Result<()> {
        let speech = concise_speech(speech);
        let active = self.state.read().await.active_project.clone();
        self.db
            .add_conversation_message("assistant", &speech, active.as_deref(), kind)?;
        self.update_reconciled(Some(speech.clone())).await;
        self.speak_now(&format!("reply-{}", Uuid::new_v4()), &speech)
            .await?;
        Ok(())
    }
    async fn update_reconciled(&self, message: Option<String>) {
        let agents = self.db.sessions().unwrap_or_default();
        let pending = self
            .db
            .pending_results()
            .map(|runs| runs.len())
            .unwrap_or(0);
        let state = reconciled_state(&agents, pending);
        self.update(state, message).await;
    }
    async fn report_error(&self, e: anyhow::Error) {
        let msg = format!("{e:#}");
        tracing::error!("{msg}");
        let _ = self.events.send(Event::Error(msg.clone()));
        self.update("error", Some(msg)).await;
    }
    pub async fn monitor(self: Arc<Self>) {
        loop {
            match self.herdr.agent_list().await {
                Ok(value) => {
                    for (name, status) in agent_states(&value) {
                        if let Some(session) = self
                            .db
                            .sessions()
                            .unwrap_or_default()
                            .into_iter()
                            .find(|s| s.agent_name == name)
                        {
                            let old = session.status.clone();
                            if old != status {
                                let _ = self.db.set_session_status(&session.project_id, &status);
                                if status == "done" && old != "done" {
                                    let has_working_run = self
                                        .db
                                        .latest_run(&session.project_id)
                                        .ok()
                                        .flatten()
                                        .is_some_and(|run| run.status == "working");
                                    if has_working_run {
                                        if let Err(error) = self
                                            .finish_project(
                                                &session.project_id,
                                                &session.agent_name,
                                            )
                                            .await
                                        {
                                            self.report_error(error).await;
                                        }
                                    } else {
                                        self.update_reconciled(Some(format!(
                                            "{} session is ready",
                                            session.project_id
                                        )))
                                        .await;
                                    }
                                } else if status == "blocked" {
                                    let project = self.project(&session.project_id).await;
                                    let terminal = self
                                        .herdr
                                        .read(&session.agent_name, 100)
                                        .await
                                        .unwrap_or_default();
                                    let message = match project {
                                        Ok(project) => self
                                            .intent
                                            .describe_blocker(&project, &terminal)
                                            .await
                                            .unwrap_or_else(|_| format!("{} needs your input. Say approve or deny, or open Herdr for details.", session.project_id)),
                                        Err(_) => format!("{} needs your input. Say approve or deny, or open Herdr for details.", session.project_id),
                                    };
                                    let message = concise_speech(&message);
                                    let _ = self.db.add_conversation_message(
                                        "assistant",
                                        &message,
                                        Some(&session.project_id),
                                        "agent_question",
                                    );
                                    let _ = self.events.send(Event::Approval {
                                        id: format!("agent:{}", session.project_id),
                                        message: message.clone(),
                                    });
                                    if self.state.read().await.announcement_mode
                                        == AnnouncementMode::Auto
                                    {
                                        if let Err(error) = self
                                            .speak_now(
                                                &format!("blocked-{}", session.project_id),
                                                &message,
                                            )
                                            .await
                                        {
                                            tracing::warn!(%error, "could not speak agent question");
                                        }
                                    }
                                    self.update_reconciled(Some(message)).await;
                                } else {
                                    self.update_reconciled(Some(format!(
                                        "{} is {}",
                                        session.project_id, status
                                    )))
                                    .await;
                                }
                            }
                        }
                    }
                }
                Err(error) => tracing::debug!("Herdr poll unavailable: {error:#}"),
            }
            sleep(Duration::from_millis(self.config.herdr.poll_interval_ms)).await;
        }
    }
    async fn finish_project(&self, project_id: &str, agent: &str) -> Result<()> {
        let project = self.project(project_id).await?;
        let run = self
            .db
            .latest_run(project_id)?
            .context("completed agent has no run")?;
        let baseline = self.db.run_terminal_baseline(&run.id)?;
        let terminal = match self
            .herdr
            .read(agent, self.config.summary.max_terminal_lines)
            .await
        {
            Ok(terminal) => terminal_since_baseline(&baseline, &terminal),
            Err(error) => {
                tracing::warn!(%error, %project_id, "could not read completed agent output");
                String::new()
            }
        };
        let speech = if terminal.trim().is_empty() {
            reliable_summary_fallback()
        } else {
            match self
                .intent
                .summarize(&project, &run.prompt, &terminal)
                .await
            {
                Ok(speech) if !looks_like_terminal_chrome(&speech) => speech,
                Ok(speech) => {
                    tracing::warn!(%project_id, %speech, "discarded unreliable completion summary");
                    reliable_summary_fallback()
                }
                Err(error) => {
                    tracing::warn!(%error, %project_id, "could not summarize completed agent output");
                    reliable_summary_fallback()
                }
            }
        };
        let speech = concise_speech(&speech);
        let id = self
            .db
            .finish_latest_run(project_id, "pending_announcement", &speech)?
            .context("no working run")?;
        self.db.add_conversation_message(
            "assistant",
            &speech,
            Some(project_id),
            "agent_completion",
        )?;
        self.update_reconciled(Some(speech.clone())).await;
        self.announce_completion(&id, project_id, &speech).await
    }
}
fn terminal_since_baseline(baseline: &str, current: &str) -> String {
    let baseline = substantive_terminal_lines(baseline);
    let current = substantive_terminal_lines(current);
    let max_overlap = baseline.len().min(current.len());
    let overlap = (1..=max_overlap)
        .rev()
        .find(|&count| baseline[baseline.len() - count..] == current[..count])
        .unwrap_or(0);
    current[overlap..].join("\n")
}

fn substantive_terminal_lines(terminal: &str) -> Vec<String> {
    terminal
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !looks_like_terminal_chrome(line))
        .map(str::to_owned)
        .collect()
}

fn looks_like_terminal_chrome(text: &str) -> bool {
    let line = text.trim();
    line == "›"
        || line.starts_with('›')
        || line.starts_with("✔ You approved")
        || line.starts_with("gpt-") && line.contains(" · ~/")
        || line.contains("ctrl +") && line.contains("transcript")
        || line.starts_with("─ Worked for")
        || (!line.is_empty()
            && line
                .chars()
                .all(|character| matches!(character, '─' | '━' | '═' | ' ')))
}

fn reliable_summary_fallback() -> String {
    "The agent finished, but Orchrd could not produce a reliable summary.".into()
}

fn string_arg<'a>(arguments: &'a Value, name: &str) -> Result<&'a str> {
    arguments
        .get(name)
        .and_then(Value::as_str)
        .with_context(|| format!("tool call omitted {name}"))
}

fn successful_action_speech(tool: &str, result: &Value) -> Option<String> {
    let project = result
        .get("project_id")
        .and_then(Value::as_str)
        .unwrap_or("the project");
    match tool {
        "send_to_agent" => match result.get("outcome").and_then(Value::as_str) {
            Some("sent")
                if result
                    .get("receipt")
                    .and_then(Value::as_str)
                    .is_some_and(|receipt| !receipt.is_empty()) =>
            {
                Some(format!("Sent to {project}."))
            }
            _ => None,
        },
        "start_session" => Some(format!("Started a new session for {project}.")),
        "resume_session" => Some(format!("Resumed the latest session for {project}.")),
        "respond_to_agent" => {
            if result.get("approved").and_then(Value::as_bool) == Some(true) {
                Some("Approved.".into())
            } else {
                Some("Denied.".into())
            }
        }
        "cancel_agent" => Some(format!("Cancelled the current operation for {project}.")),
        _ => None,
    }
}

fn failed_action_speech(tool: &str) -> Option<&'static str> {
    match tool {
        "send_to_agent" => Some("I couldn't send that request. Check the Orchrd log for details."),
        "start_session" => Some("I couldn't start that session. Check the Orchrd log for details."),
        "resume_session" => {
            Some("I couldn't resume that session. Check the Orchrd log for details.")
        }
        "respond_to_agent" => {
            Some("I couldn't apply that response. Check the Orchrd log for details.")
        }
        "cancel_agent" => {
            Some("I couldn't cancel that operation. Check the Orchrd log for details.")
        }
        _ => None,
    }
}

fn approval_keys(terminal: &str) -> Vec<&'static str> {
    let lower = terminal.to_ascii_lowercase();
    let numbered_approval = lower.lines().any(|line| {
        let line = line.trim_start();
        (line.starts_with("1.") || line.starts_with("1)"))
            && ["yes", "approve", "allow", "proceed", "run"]
                .iter()
                .any(|word| line.contains(word))
    }) || [
        "press 1 to approve",
        "reply with 1 to approve",
        "enter 1 to approve",
    ]
    .iter()
    .any(|phrase| lower.contains(phrase));

    if numbered_approval {
        vec!["1", "enter"]
    } else if lower.contains("y/n") || lower.contains("y / n") {
        vec!["y", "enter"]
    } else {
        // Codex approval menus select the highlighted default with Enter.
        vec!["enter"]
    }
}

fn reconciled_state(agents: &[AgentSession], pending_results: usize) -> &'static str {
    if agents.iter().any(|agent| agent.status == "blocked") {
        "blocked"
    } else if agents.iter().any(|agent| agent.status == "working") {
        "working"
    } else if pending_results > 0 {
        "result_pending"
    } else if agents.iter().any(|agent| agent.status == "unknown") {
        "unknown"
    } else {
        "idle"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(status: &str) -> AgentSession {
        AgentSession {
            project_id: status.into(),
            workspace_id: "w1".into(),
            pane_id: "w1:p1".into(),
            agent_name: format!("agent_{status}"),
            status: status.into(),
        }
    }

    #[test]
    fn important_agent_states_win_over_replies_and_results() {
        assert_eq!(reconciled_state(&[session("working")], 1), "working");
        assert_eq!(
            reconciled_state(&[session("working"), session("blocked")], 1),
            "blocked"
        );
        assert_eq!(reconciled_state(&[session("done")], 1), "result_pending");
        assert_eq!(reconciled_state(&[session("done")], 0), "idle");
    }

    #[test]
    fn chooses_keys_from_the_agent_prompt() {
        assert_eq!(
            approval_keys("1. Yes, proceed\n2. No, and tell Codex what to do"),
            vec!["1", "enter"]
        );
        assert_eq!(approval_keys("Continue? [y/N]"), vec!["y", "enter"]);
        assert_eq!(
            approval_keys("Press Enter to approve or Esc to cancel"),
            vec!["enter"]
        );
    }

    #[test]
    fn completion_output_starts_after_the_saved_terminal_baseline() {
        let baseline = "old result\ngpt-5.6-sol medium · ~/project\n›";
        let current = "old result\ngpt-5.6-sol medium · ~/project\n› new request\nnew result\ngpt-5.6-sol medium · ~/project";
        assert_eq!(terminal_since_baseline(baseline, current), "new result");
    }

    #[test]
    fn completion_output_handles_scrollback_that_dropped_old_lines() {
        let baseline = "old one\nold two\nold three";
        let current = "old two\nold three\ncurrent task result";
        assert_eq!(
            terminal_since_baseline(baseline, current),
            "current task result"
        );
    }

    #[test]
    fn terminal_footer_is_not_a_valid_summary() {
        assert!(looks_like_terminal_chrome(
            "gpt-5.6-sol medium · ~/Documents/Projects/orchrd"
        ));
        assert!(!looks_like_terminal_chrome(
            "The requested layout change is complete."
        ));
    }

    #[test]
    fn controller_acknowledges_only_successful_action_results() {
        assert_eq!(
            successful_action_speech(
                "send_to_agent",
                &json!({
                    "project_id":"orchrd",
                    "outcome":"sent",
                    "receipt":"run-123"
                })
            )
            .as_deref(),
            Some("Sent to orchrd.")
        );
        assert_eq!(
            successful_action_speech(
                "send_to_agent",
                &json!({"project_id":"orchrd","outcome":"unknown"})
            ),
            None
        );
        assert_eq!(
            successful_action_speech(
                "send_to_agent",
                &json!({"project_id":"orchrd","outcome":"sent"})
            ),
            None
        );
    }

    #[test]
    fn controller_owns_side_effect_failure_speech() {
        assert!(failed_action_speech("send_to_agent").is_some());
        assert!(failed_action_speech("respond_to_agent").is_some());
        assert_eq!(failed_action_speech("read_agent"), None);
    }
}
