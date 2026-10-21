//! Herdr observation and input, bound to live agent instances and questions.
use crate::{
    agent::AgentInfo,
    config::Agents,
    dialog::{Capture, Route},
    question::{Answer, Question},
    terminal_input::{self, InputPlan},
};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{process::Command, sync::Mutex, time::timeout};

#[derive(Clone)]
pub(crate) struct Herdr {
    settings: Agents,
    state: Arc<Mutex<HashMap<String, Entry>>>,
    running: Arc<Mutex<HashSet<String>>>,
    actions: Arc<Mutex<()>>,
}
struct Entry {
    capture: Capture,
    changed: Instant,
    attempted: bool,
    running: bool,
    question: Option<Value>,
    route: Option<Route>,
}
impl Herdr {
    pub fn new(settings: Agents) -> Self {
        Self {
            settings,
            state: Arc::default(),
            running: Arc::default(),
            actions: Arc::default(),
        }
    }
    async fn run(&self, args: &[&str]) -> Result<String> {
        let output = timeout(
            Duration::from_secs(5),
            Command::new(&self.settings.herdr_command)
                .args(["--session", &self.settings.herdr_session])
                .args(args)
                .stdin(Stdio::null())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .context("Herdr command timed out; outcome is unconfirmed")?
        .context("run Herdr command")?;
        if !output.status.success() {
            let response: Value = serde_json::from_slice(&output.stderr).unwrap_or(Value::Null);
            if let Some(code) = response["error"]["code"].as_str().filter(|s| {
                s.len() < 80 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            }) {
                bail!("Herdr rejected the command ({code}); outcome is unconfirmed")
            }
            if output.status.code() == Some(2) {
                bail!("Herdr rejected the command arguments (exit 2); input was not delivered")
            }
            bail!("Herdr command failed ({}); check the configured session and target; outcome is unconfirmed", output.status)
        }
        if output.stdout.len() > 1_000_000 {
            bail!("Herdr response exceeds size limit")
        }
        String::from_utf8(output.stdout).context("Herdr output is not UTF-8")
    }
    async fn roster(&self) -> Result<Vec<AgentInfo>> {
        let value: Value = serde_json::from_str(&self.run(&["agent", "list"]).await?)?;
        parse_roster(&value, &self.settings.herdr_session)
    }
    async fn screen(&self, id: &str) -> Result<String> {
        let screen = self
            .run(&[
                "agent",
                "read",
                id,
                "--source",
                "visible",
                "--lines",
                &self.settings.screen_lines.to_string(),
                "--format",
                "text",
            ])
            .await?;
        // Bound the captured *bottom*, even on Herdr versions that return more
        // visible lines than requested. Preserve the full question or inspect.
        let lines: Vec<_> = screen.lines().collect();
        let screen = lines[lines.len().saturating_sub(self.settings.screen_lines)..].join("\n");
        if screen.len() > 64000 {
            bail!("Herdr screen exceeds dialog size limit")
        }
        Ok(screen)
    }
    pub async fn capture(&self, id: &str) -> Result<Capture> {
        let before = self
            .roster()
            .await?
            .into_iter()
            .find(|a| a.name == id)
            .context("Herdr agent disappeared")?;
        let screen = self.screen(id).await?;
        let after = self
            .roster()
            .await?
            .into_iter()
            .find(|a| a.name == id)
            .context("Herdr agent disappeared")?;
        if before != after {
            bail!("Herdr agent changed during capture")
        }
        Ok(Capture {
            agent: after,
            screen,
        })
    }
    pub async fn read(&self, id: &str) -> Result<String> {
        let capture = self.capture(id).await?;
        let mut state = self.state.lock().await;
        if state.get(id).is_some_and(|entry| entry.capture != capture) {
            state.remove(id);
        }
        Ok(capture.screen)
    }
    pub async fn poll(&self) -> Result<Vec<AgentInfo>> {
        let agents = match self.roster().await {
            Ok(agents) => agents,
            Err(error) => {
                self.state.lock().await.clear();
                return Err(error);
            }
        };
        self.state
            .lock()
            .await
            .retain(|id, _| agents.iter().any(|a| &a.name == id));
        for agent in &agents {
            if !matches!(agent.status.as_str(), "blocked" | "idle" | "done") {
                self.state.lock().await.remove(&agent.name);
                continue;
            }
            let screen = match self.screen(&agent.name).await {
                Ok(screen) => screen,
                Err(error) => {
                    self.state.lock().await.remove(&agent.name);
                    tracing::warn!(%error, "Herdr screen unavailable");
                    continue;
                }
            };
            let capture = Capture {
                agent: agent.clone(),
                screen,
            };
            let mut state = self.state.lock().await;
            if state
                .get(&agent.name)
                .is_none_or(|old| old.capture != capture)
            {
                state.insert(
                    agent.name.clone(),
                    Entry {
                        capture,
                        changed: Instant::now(),
                        attempted: false,
                        running: false,
                        question: None,
                        route: None,
                    },
                );
            }
        }
        Ok(agents)
    }
    pub async fn next(&self) -> Option<Capture> {
        let mut state = self.state.lock().await;
        let mut running = self.running.lock().await;
        let entry = state
            .values_mut()
            .filter(|entry| {
                !entry.attempted
                    && !running.contains(&entry.capture.agent.name)
                    && entry.changed.elapsed() >= Duration::from_secs(1)
            })
            .min_by_key(|entry| entry.changed)?;
        entry.attempted = true;
        entry.running = true;
        running.insert(entry.capture.agent.name.clone());
        Some(entry.capture.clone())
    }
    pub async fn current(&self, capture: &Capture) -> bool {
        let matches = self
            .state
            .lock()
            .await
            .get(&capture.agent.name)
            .is_some_and(|entry| entry.capture == *capture);
        matches
            && self
                .capture(&capture.agent.name)
                .await
                .is_ok_and(|fresh| fresh == *capture)
    }
    pub async fn mark_detection(&self, capture: &Capture, route: Route) {
        if let Some(entry) = self
            .state
            .lock()
            .await
            .get_mut(&capture.agent.name)
            .filter(|entry| entry.capture == *capture)
        {
            entry.route = Some(route);
        }
    }
    pub async fn store(
        &self,
        capture: &Capture,
        question: Question,
        extraction: Value,
        conversation: bool,
    ) {
        let can_approve = terminal_input::permission(&question, &extraction, true)
            .and_then(|answer| {
                terminal_input::plan(
                    &question,
                    &extraction,
                    &capture.screen,
                    conversation,
                    &answer,
                )
            })
            .is_ok();
        let can_deny = terminal_input::permission(&question, &extraction, false)
            .and_then(|answer| {
                terminal_input::plan(
                    &question,
                    &extraction,
                    &capture.screen,
                    conversation,
                    &answer,
                )
            })
            .is_ok();
        let actionable = {
            question.options.iter().any(|option| {
                terminal_input::plan(
                    &question,
                    &extraction,
                    &capture.screen,
                    conversation,
                    &Answer::Choices {
                        option_ids: vec![option.id.clone()],
                    },
                )
                .is_ok()
            }) || (question.allow_custom
                && terminal_input::plan(
                    &question,
                    &extraction,
                    &capture.screen,
                    conversation,
                    &Answer::Custom {
                        text: "Answer".into(),
                    },
                )
                .is_ok())
        };
        let mut state = self.state.lock().await;
        if let Some(entry) = state
            .get_mut(&capture.agent.name)
            .filter(|entry| entry.capture == *capture)
        {
            entry.question = Some(
                json!({"agent_name":capture.agent.name,"instance_id":capture.agent.instance_id,
                "state_change_seq":capture.agent.state_change_seq,"read_only":!actionable,
                "conversation":conversation,"can_approve":can_approve,"can_deny":can_deny,
                "question":question,"extraction":extraction}),
            );
        }
    }
    pub async fn questions(&self) -> Vec<Value> {
        let mut values: Vec<_> = self
            .state
            .lock()
            .await
            .values()
            .filter_map(|entry| entry.question.clone())
            .collect();
        values.sort_by(|a, b| a["agent_name"].as_str().cmp(&b["agent_name"].as_str()));
        values
    }
    pub async fn question(&self, id: &str) -> Option<Question> {
        let (capture, value) = {
            let state = self.state.lock().await;
            let entry = state.get(id)?;
            (entry.capture.clone(), entry.question.clone()?)
        };
        if !self.current(&capture).await {
            self.state.lock().await.remove(id);
            return None;
        }
        serde_json::from_value(value["question"].clone()).ok()
    }
    pub async fn question_ids(&self) -> HashMap<String, String> {
        let ids: Vec<_> = self.state.lock().await.keys().cloned().collect();
        let mut values = HashMap::new();
        for id in ids {
            if let Some(question) = self.question(&id).await {
                values.insert(id, question.question_id);
            }
        }
        values
    }
    pub async fn routing_questions(&self) -> Value {
        let ids = self.question_ids().await;
        let state = self.state.lock().await;
        json!(state.iter().filter_map(|(id,entry)| {
            let value = entry.question.as_ref()?;
            (ids.get(id).is_some_and(|key| value["question"]["question_id"] == *key))
                .then(|| json!({"agent_id":id,"agent_name":entry.capture.agent.display_name,"question":value["question"]}))
        }).collect::<Vec<_>>())
    }
    async fn input(&self, args: &[&str]) -> Result<()> {
        let raw = self.run(args).await?;
        let value: Value = serde_json::from_str(&raw)
            .context("Herdr input receipt was not JSON; outcome is unconfirmed")?;
        if value.get("error").is_some() || value.get("result").is_none() {
            bail!("Herdr did not acknowledge input; outcome is unconfirmed")
        }
        Ok(())
    }
    pub async fn command(&self, expected: &AgentInfo, action: &str, value: Value) -> Result<()> {
        let _serial = self.actions.lock().await;
        let fresh = self.capture(&expected.name).await?;
        if fresh.agent.instance_id != expected.instance_id || fresh.agent.kind != expected.kind {
            bail!("The agent was replaced before input delivery")
        }
        if fresh.agent.state_change_seq != expected.state_change_seq {
            bail!("The agent changed state while the request was being prepared; please try again")
        }
        let plan = match action {
            "prompt" => {
                let text = value["text"].as_str().context("Prompt omitted text")?;
                terminal_input::valid_prompt(text)?;
                if fresh.agent.status == "blocked" {
                    bail!("Agent is waiting for input; answer its current question instead")
                }
                if !matches!(fresh.agent.status.as_str(), "idle" | "done" | "working") {
                    bail!("Agent readiness is unknown")
                }
                if fresh.agent.status != "working" {
                    let state = self.state.lock().await;
                    if !state.get(&expected.name).is_some_and(|entry| {
                        entry.capture == fresh
                            && entry.route == Some(Route::Ignore)
                            && entry.question.is_none()
                    }) {
                        bail!("The agent screen has a question or is still being checked; wait for its question or rescan")
                    }
                }
                InputPlan::Prompt(text.into())
            }
            "cancel" => InputPlan::Keys(vec!["ctrl+c".into()]),
            "permission" | "answer_question" => {
                let (bound, metadata) = {
                    let state = self.state.lock().await;
                    let entry = state
                        .get(&expected.name)
                        .context("Question is no longer available")?;
                    (
                        entry.capture.clone(),
                        entry
                            .question
                            .clone()
                            .context("Question was already answered or changed")?,
                    )
                };
                if bound != fresh {
                    self.state.lock().await.remove(&expected.name);
                    bail!("The question changed before input delivery")
                }
                let question: Question = serde_json::from_value(metadata["question"].clone())?;
                if value["question_id"] != question.question_id {
                    bail!("The question changed before input delivery")
                }
                let answer = if action == "permission" {
                    terminal_input::permission(
                        &question,
                        &metadata["extraction"],
                        value["approve"]
                            .as_bool()
                            .context("Permission omitted decision")?,
                    )?
                } else {
                    serde_json::from_value::<Answer>(value["answer"].clone())
                        .context("Invalid question answer")?
                };
                terminal_input::plan(
                    &question,
                    &metadata["extraction"],
                    &bound.screen,
                    metadata["conversation"] == true,
                    &answer,
                )?
            }
            _ => bail!("Unsupported Herdr operation"),
        };
        // The API has no atomic expected-screen condition. Re-read immediately
        // before the write, and never retry an uncertain or partially sent input.
        if self.capture(&expected.name).await? != fresh {
            bail!("The agent screen changed before input delivery")
        }
        {
            let mut state = self.state.lock().await;
            if matches!(action, "permission" | "answer_question") {
                let entry = state
                    .get_mut(&expected.name)
                    .context("Question is no longer available")?;
                if entry.capture != fresh
                    || entry
                        .question
                        .as_ref()
                        .is_none_or(|q| q["question"]["question_id"] != value["question_id"])
                {
                    bail!("The question changed before input delivery")
                }
                entry.question = None;
            } else if action == "prompt" && fresh.agent.status != "working" {
                let entry = state
                    .get(&expected.name)
                    .context("The screen changed before submission")?;
                if entry.capture != fresh
                    || entry.route != Some(Route::Ignore)
                    || entry.question.is_some()
                {
                    bail!("The screen now has a question; answer it instead")
                }
            } else if let Some(entry) = state.get_mut(&expected.name) {
                entry.question = None;
            }
        }
        let args = input_args(&expected.name, plan);
        self.input(&args.iter().map(String::as_str).collect::<Vec<_>>())
            .await
    }

    pub async fn retry(&self) {
        for entry in self.state.lock().await.values_mut() {
            if entry.question.is_none() && !entry.running {
                entry.attempted = false;
            }
        }
    }
    pub async fn finish(&self, capture: &Capture) {
        // A replacement capture must not start another request for this pane
        // until the previous request has completed, even when its entry changed.
        self.running.lock().await.remove(&capture.agent.name);
        if let Some(entry) = self
            .state
            .lock()
            .await
            .get_mut(&capture.agent.name)
            .filter(|entry| entry.capture == *capture)
        {
            entry.running = false;
        }
    }
}

fn parse_roster(value: &Value, session: &str) -> Result<Vec<AgentInfo>> {
    let agents = value
        .pointer("/result/agents")
        .or_else(|| value.get("agents"))
        .and_then(Value::as_array)
        .context("Herdr response omitted agent roster")?;
    let mut out = Vec::new();
    for value in agents {
        let pane = value["pane_id"]
            .as_str()
            .context("Herdr agent omitted pane identity")?;
        let terminal = value["terminal_id"]
            .as_str()
            .context("Herdr agent omitted terminal identity")?;
        let cwd = value["foreground_cwd"]
            .as_str()
            .or_else(|| value["cwd"].as_str())
            .map(std::path::PathBuf::from);
        let project = cwd
            .as_ref()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .unwrap_or(pane);
        let status = value["agent_status"].as_str().unwrap_or("unknown");
        out.push(AgentInfo {
            display_name: format!("{project} ({pane})"),
            name: pane.into(),
            kind: value["agent"].as_str().map(str::to_owned),
            instance_id: format!("{session}:{terminal}"),
            cwd,
            status: status.into(),
            state_change_seq: value["state_change_seq"].as_u64(),
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

// Herdr parses the positional prompt directly; it does not accept a `--`
// separator here. For send-keys that separator would become a literal key.
fn input_args(target: &str, plan: InputPlan) -> Vec<String> {
    let (command, values) = match plan {
        InputPlan::Prompt(text) => ("prompt", vec![text]),
        InputPlan::Text(text) => {
            let mut keys: Vec<_> = text
                .chars()
                .map(|c| {
                    if c == ' ' {
                        "space".into()
                    } else {
                        c.to_string()
                    }
                })
                .collect();
            keys.push("enter".into());
            ("send-keys", keys)
        }
        InputPlan::Keys(keys) => ("send-keys", keys),
    };
    let mut args = vec!["agent".into(), command.into(), target.into()];
    args.extend(values);
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt};
    fn roster(terminal: &str, status: &str) -> Value {
        json!({"result":{"agents":[{"pane_id":"w1:p1","terminal_id":terminal,
            "agent":"example-agent","agent_status":status,"state_change_seq":4,
            "foreground_cwd":"/projects/demo"}]}})
    }
    struct ControlFixture {
        _directory: tempfile::TempDir,
        client: Herdr,
        roster: std::path::PathBuf,
        screen: std::path::PathBuf,
        log: std::path::PathBuf,
        receipt: std::path::PathBuf,
        expected: AgentInfo,
    }
    impl ControlFixture {
        async fn new(status: &str) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let command = directory.path().join("herdr");
            let roster_file = directory.path().join("roster.json");
            let screen = directory.path().join("screen");
            let log = directory.path().join("input");
            let receipt = directory.path().join("receipt");
            fs::write(&roster_file, roster("t1", status).to_string()).unwrap();
            fs::write(&screen,"Run the command?\n› 1. Yes, proceed (y)\n  2. Always allow (p)\n  3. No (esc)\nPress enter to confirm or esc to cancel").unwrap();
            fs::write(&receipt, "{\"result\":{}}").unwrap();
            let shell = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                .map(|p| p.join("bash"))
                .find(|p| p.is_file())
                .unwrap();
            fs::write(&command,format!("#!{}\ncase \"$4\" in\nlist) cat '{}' ;;\nread) cat '{}' ;;\nprompt|send-keys) printf '%s\\0' \"$@\" >> '{}'; cat '{}' ;;\n*) exit 99 ;;\nesac\n",
                shell.display(),roster_file.display(),screen.display(),log.display(),receipt.display())).unwrap();
            fs::set_permissions(&command, fs::Permissions::from_mode(0o700)).unwrap();
            let client = Herdr::new(Agents {
                herdr_command: command.to_string_lossy().into_owned(),
                ..Agents::default()
            });
            let expected = client.poll().await.unwrap().remove(0);
            Self {
                _directory: directory,
                client,
                roster: roster_file,
                screen,
                log,
                receipt,
                expected,
            }
        }
        async fn bind(&self, conversation: bool) -> Question {
            let capture = self.client.capture(&self.expected.name).await.unwrap();
            let value = json!({"status":"active","complete":true,"purpose":if conversation {"clarification"} else {"permission"},
                "input_mode":"single_choice","question":"Run the command?",
                "options":[{"label":"Yes, proceed","effect":"allow_once","shortcut":"y","selected":null},
                    {"label":"Always allow","effect":"allow_persistent","shortcut":"p","selected":null},
                    {"label":"No","effect":"deny","shortcut":"esc","selected":null}],"allows_custom_answer":false,
                "interaction":{"method":"typed_shortcut","selected_index":0,"instructions":"Press enter to confirm or esc to cancel"},
                "evidence":["Run the command?"]});
            let route = if conversation {
                Route::Conversation
            } else {
                Route::Modal
            };
            let (question, extraction) = crate::dialog::present(value, &capture, route).unwrap();
            self.client.mark_detection(&capture, route).await;
            self.client
                .store(&capture, question.clone(), extraction, conversation)
                .await;
            question
        }
        fn inputs(&self) -> Vec<String> {
            fs::read(&self.log)
                .unwrap_or_default()
                .split(|b| *b == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8(s.to_vec()).unwrap())
                .collect()
        }
    }
    #[tokio::test]
    async fn input_delivery_approves_once_denies_and_consumes_the_question() {
        for (approve, key) in [(true, "enter"), (false, "esc")] {
            let f = ControlFixture::new("blocked").await;
            let q = f.bind(false).await;
            let command = json!({"question_id":q.question_id,"approve":approve});
            f.client
                .command(&f.expected, "permission", command.clone())
                .await
                .unwrap();
            assert_eq!(
                f.inputs(),
                vec!["--session", "default", "agent", "send-keys", "w1:p1", key]
            );
            assert!(f
                .client
                .command(&f.expected, "permission", command)
                .await
                .is_err());
            assert_eq!(f.inputs().len(), 6); // Exactly one write, including double clicks.
        }
    }
    #[tokio::test]
    async fn changed_screens_instances_or_question_ids_never_receive_input() {
        for change in ["screen", "instance", "question", "sequence"] {
            let f = ControlFixture::new("blocked").await;
            let q = f.bind(false).await;
            if change == "screen" {
                fs::write(&f.screen, "New question").unwrap();
            }
            if change == "instance" {
                fs::write(&f.roster, roster("replacement", "blocked").to_string()).unwrap();
            }
            if change == "sequence" {
                let mut changed = roster("t1", "blocked");
                changed["result"]["agents"][0]["state_change_seq"] = json!(5);
                fs::write(&f.roster, changed.to_string()).unwrap();
            }
            let id = if change == "question" {
                "wrong".to_owned()
            } else {
                q.question_id
            };
            assert!(f
                .client
                .command(
                    &f.expected,
                    "permission",
                    json!({"question_id":id,"approve":true})
                )
                .await
                .is_err());
            assert!(f.inputs().is_empty());
        }
    }
    #[tokio::test]
    async fn prompt_cancel_and_chat_answers_use_agent_input_with_literal_arguments() {
        let f = ControlFixture::new("idle").await;
        assert!(f
            .client
            .command(&f.expected, "prompt", json!({"text":"task"}))
            .await
            .is_err());
        let capture = f.client.capture(&f.expected.name).await.unwrap();
        f.client.mark_detection(&capture, Route::Ignore).await;
        f.client
            .command(
                &f.expected,
                "prompt",
                json!({"text":"--help; literal $(text)"}),
            )
            .await
            .unwrap();
        assert_eq!(
            f.inputs(),
            vec![
                "--session",
                "default",
                "agent",
                "prompt",
                "w1:p1",
                "--help; literal $(text)"
            ]
        );
        fs::remove_file(&f.log).unwrap();
        f.client
            .command(&f.expected, "cancel", json!({}))
            .await
            .unwrap();
        assert_eq!(f.inputs().last().unwrap(), "ctrl+c");
        fs::remove_file(&f.log).unwrap();
        let q = f.bind(true).await;
        f.client
            .command(
                &f.expected,
                "answer_question",
                json!({"question_id":q.question_id,"answer":{"kind":"choices","option_ids":["1"]}}),
            )
            .await
            .unwrap();
        assert_eq!(f.inputs().last().unwrap(), "Always allow"); // Chat text, not terminal shortcut p.
        assert_eq!(f.inputs()[3], "prompt");
    }
    #[tokio::test]
    async fn modal_text_is_sent_as_characters_and_permission_scope_requires_explicit_selection() {
        let f = ControlFixture::new("blocked").await;
        fs::write(&f.screen, "Project name?\nPress Enter to submit").unwrap();
        f.client.poll().await.unwrap();
        let capture = f.client.capture(&f.expected.name).await.unwrap();
        let value = json!({"status":"active","complete":true,"purpose":"clarification","input_mode":"free_text",
            "question":"Project name?","options":[],"allows_custom_answer":true,
            "interaction":{"method":"text_entry","selected_index":null,"instructions":"Press Enter to submit"},"evidence":["Project name?"]});
        let (q, v) = crate::dialog::present(value, &capture, Route::Modal).unwrap();
        f.client.store(&capture, q.clone(), v, false).await;
        f.client
            .command(
                &f.expected,
                "answer_question",
                json!({"question_id":q.question_id,"answer":{"kind":"custom","text":"ctrl+c"}}),
            )
            .await
            .unwrap();
        let input = f.inputs();
        assert_eq!(&input[5..], &["c", "t", "r", "l", "+", "c", "enter"]);
        let f = ControlFixture::new("blocked").await;
        let q = f.bind(false).await;
        f.client
            .command(
                &f.expected,
                "answer_question",
                json!({"question_id":q.question_id,"answer":{"kind":"choices","option_ids":["1"]}}),
            )
            .await
            .unwrap();
        assert_eq!(f.inputs().last().unwrap(), "p");
    }

    #[tokio::test]
    async fn rejected_receipts_are_not_retried_and_permissions_are_not_reused() {
        let f = ControlFixture::new("blocked").await;
        let q = f.bind(false).await;
        fs::write(&f.receipt, "{\"error\":{\"code\":\"failed\"}}").unwrap();
        let value = json!({"question_id":q.question_id,"approve":true});
        assert!(f
            .client
            .command(&f.expected, "permission", value.clone())
            .await
            .is_err());
        assert!(f
            .client
            .command(&f.expected, "permission", value)
            .await
            .is_err());
        assert_eq!(f.inputs().len(), 6);
    }

    #[tokio::test]
    async fn cli_usage_errors_are_distinguished_from_delivery_failures() {
        let mut f = ControlFixture::new("idle").await;
        let script = fs::read_to_string(&f.client.settings.herdr_command).unwrap();
        let rejected = f._directory.path().join("rejected-herdr");
        fs::write(
            &rejected,
            format!("{}\nexit 2\n", script.lines().next().unwrap()),
        )
        .unwrap();
        fs::set_permissions(&rejected, fs::Permissions::from_mode(0o700)).unwrap();
        f.client.settings.herdr_command = rejected.to_string_lossy().into_owned();
        let error = f
            .client
            .run(&["agent", "list"])
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("command arguments (exit 2)"), "{error}");
        assert!(error.contains("input was not delivered"));
    }

    // Run with ORCHRD_TEST_HERDR=/absolute/path/to/herdr. A random session
    // without a socket exercises the installed parser without sending input.
    #[tokio::test]
    #[ignore = "requires an installed Herdr CLI"]
    async fn installed_cli_accepts_input_arguments() {
        let command = std::env::var("ORCHRD_TEST_HERDR").expect("set ORCHRD_TEST_HERDR");
        let client = Herdr::new(Agents {
            herdr_command: command,
            herdr_session: format!("orchrd-parser-{}", uuid::Uuid::new_v4()),
            ..Agents::default()
        });
        for plan in [
            InputPlan::Prompt("Is everything working?".into()),
            InputPlan::Prompt("--help; literal $(text)".into()),
            InputPlan::Keys(vec!["ctrl+c".into()]),
            InputPlan::Keys(vec!["enter".into()]),
            InputPlan::Text("a b".into()),
        ] {
            let args = input_args("orchrd-parser-probe", plan);
            let error = client
                .run(&args.iter().map(String::as_str).collect::<Vec<_>>())
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("server_not_running"), "{args:?}: {error}");
        }
    }

    #[test]
    fn identity_uses_terminal_instance_and_session_rather_than_agent_brand() {
        let agents = parse_roster(&roster("t1", "blocked"), "default").unwrap();
        assert_eq!(agents[0].name, "w1:p1");
        assert_eq!(agents[0].display_name, "demo (w1:p1)");
        assert_ne!(
            agents[0].instance_id,
            parse_roster(&roster("t2", "blocked"), "default").unwrap()[0].instance_id
        );
        assert_ne!(
            agents[0].instance_id,
            parse_roster(&roster("t1", "blocked"), "another").unwrap()[0].instance_id
        );
        assert!(parse_roster(&json!({"result":{}}), "default").is_err());
    }
    #[tokio::test]
    async fn cli_observation_deduplicates_scans_and_invalidates_changed_screens() {
        let dir = tempfile::tempdir().unwrap();
        let command = dir.path().join("herdr");
        let roster_file = dir.path().join("roster.json");
        let screen_file = dir.path().join("screen.txt");
        fs::write(&roster_file, roster("t1", "blocked").to_string()).unwrap();
        fs::write(&screen_file, "Choose destination:\n> Local\n  Cloud").unwrap();
        let shell = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|path| path.join("bash"))
            .find(|path| path.is_file())
            .expect("bash in test PATH");
        fs::write(
            &command,
            format!(
                "#!{}\ncase \"$4\" in\nlist) cat '{}' ;;\nread) cat '{}' ;;\n*) exit 99 ;;\nesac\n",
                shell.display(),
                roster_file.display(),
                screen_file.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&command, fs::Permissions::from_mode(0o700)).unwrap();
        let client = Herdr::new(Agents {
            herdr_command: command.to_string_lossy().into_owned(),
            ..Agents::default()
        });
        assert_eq!(client.poll().await.unwrap().len(), 1);
        client.state.lock().await.get_mut("w1:p1").unwrap().changed =
            Instant::now() - Duration::from_secs(2);
        let capture = client.next().await.unwrap();
        assert!(client.current(&capture).await);
        client.retry().await; // Never duplicate an in-flight extraction.
        assert!(client.next().await.is_none());
        client.finish(&capture).await;
        client.retry().await;
        assert!(client.next().await.is_some());
        fs::write(&screen_file, "Agent resumed").unwrap();
        assert!(!client.current(&capture).await);
        client.poll().await.unwrap();
        client.state.lock().await.get_mut("w1:p1").unwrap().changed =
            Instant::now() - Duration::from_secs(2);
        assert!(client.next().await.is_none()); // The old capture is still in flight.
        client.finish(&capture).await;
        assert!(client.next().await.is_some()); // The changed screen can now be scanned.
        let q = Question {
            question_id: "old".into(),
            question_kind: "question".into(),
            title: "old".into(),
            body: "old".into(),
            options: vec![],
            multiple: false,
            allow_custom: false,
        };
        client.store(&capture, q, json!({}), false).await;
        assert!(client.questions().await.is_empty()); // A late extraction cannot attach to the new screen.
        fs::write(&roster_file, roster("t2", "working").to_string()).unwrap();
        client.poll().await.unwrap();
        assert!(client.next().await.is_none());
        fs::write(&roster_file, "{}").unwrap();
        assert!(client.poll().await.is_err());
        assert!(client.questions().await.is_empty());
    }
}
