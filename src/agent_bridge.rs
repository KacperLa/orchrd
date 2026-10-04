//! Local, structured bridge to interactive coding-agent sessions.
//! The socket is private to the user and agents connect opportunistically.
use crate::{
    agent::AgentInfo,
    config::Config,
    coordinator::{Message, Scope},
    db::ResponseJob,
};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixListener,
    sync::{mpsc, oneshot, Mutex},
    time::timeout,
};
use uuid::Uuid;

#[derive(Clone, Default)]
pub struct AgentBridge(Arc<Mutex<HashMap<String, Peer>>>);

struct Peer {
    name: String,
    kind: String,
    cwd: String,
    status: String,
    seq: u64,
    output: String,
    question: Option<crate::question::Question>,
    sender: mpsc::UnboundedSender<Value>,
    pending: HashMap<String, oneshot::Sender<Result<()>>>,
}

impl AgentBridge {
    pub async fn serve(&self, messages: mpsc::UnboundedSender<Message>) -> Result<()> {
        let path = Config::runtime_dir().join("agents.sock");
        tokio::fs::create_dir_all(path.parent().unwrap()).await?;
        if path.exists() {
            // A live daemon would already own the D-Bus name; this is a stale socket.
            tokio::fs::remove_file(&path).await?;
        }
        let listener = UnixListener::bind(&path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).await?;
        }
        loop {
            let (stream, _) = listener.accept().await?;
            let bridge = self.clone();
            let messages = messages.clone();
            tokio::spawn(async move {
                if let Err(error) = bridge.peer(stream, messages).await {
                    tracing::warn!(%error, "agent bridge disconnected");
                }
            });
        }
    }

    async fn peer(
        &self,
        stream: tokio::net::UnixStream,
        messages: mpsc::UnboundedSender<Message>,
    ) -> Result<()> {
        let (read, mut write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        let (sender, mut receiver) = mpsc::unbounded_channel::<Value>();
        let writer = tokio::spawn(async move {
            while let Some(value) = receiver.recv().await {
                if write
                    .write_all(format!("{value}\n").as_bytes())
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        let mut registered: Option<String> = None;
        while let Some(line) = lines.next_line().await? {
            if line.len() > 1_000_000 {
                bail!("oversized agent bridge message")
            }
            let value: Value = serde_json::from_str(&line).context("decode agent event")?;
            let event = value["event"].as_str().unwrap_or_default();
            if event == "register" {
                if registered.is_some() {
                    bail!("duplicate agent registration")
                }
                let id = value["id"].as_str().context("missing agent id")?;
                if id.is_empty() || id.len() > 160 {
                    bail!("invalid agent id")
                }
                let name = value["name"].as_str().unwrap_or("Agent").to_owned();
                let kind = value["kind"].as_str().unwrap_or("agent").to_owned();
                let cwd = value["cwd"].as_str().unwrap_or_default().to_owned();
                let status = value["status"]
                    .as_str()
                    .filter(|status| matches!(*status, "idle" | "working" | "blocked" | "done"))
                    .unwrap_or("idle")
                    .to_owned();
                let output = value["output"].as_str().unwrap_or_default().to_owned();
                self.0.lock().await.insert(
                    id.into(),
                    Peer {
                        name,
                        kind,
                        cwd,
                        status,
                        seq: 0,
                        output,
                        question: value
                            .get("pending_question")
                            .and_then(|q| serde_json::from_value(q.clone()).ok()),
                        sender: sender.clone(),
                        pending: HashMap::new(),
                    },
                );
                registered = Some(id.into());
                continue;
            }
            let id = registered
                .as_ref()
                .context("agent event before registration")?;
            let mut peers = self.0.lock().await;
            let Some(peer) = peers.get_mut(id) else { break };
            match event {
                "metadata" => {
                    if let Some(name) = value["name"].as_str().filter(|name| !name.is_empty()) {
                        peer.name = name.to_owned();
                    }
                    if let Some(cwd) = value["cwd"].as_str() {
                        peer.cwd = cwd.to_owned();
                    }
                }
                "status" => {
                    let status = value["status"].as_str().unwrap_or("idle");
                    if matches!(status, "idle" | "working" | "blocked" | "done")
                        && peer.status != status
                    {
                        if status == "working" && peer.status != "blocked" {
                            peer.output.clear();
                        }
                        if status != "blocked" {
                            peer.question = None;
                        }
                        peer.status = status.into();
                        peer.seq += 1;
                    }
                }
                "message" => {
                    if let Some(text) = value["text"].as_str() {
                        if peer.output.len() < 1_000_000 {
                            peer.output.push_str(text);
                        }
                    }
                }
                "permission" => {
                    peer.question = serde_json::from_value(value.clone()).ok();
                    peer.status = "blocked".into();
                    peer.seq += 1;
                    peer.output = if let Some(question) = &peer.question {
                        serde_json::to_string(question)?
                    } else {
                        format!(
                            "Question: {}. {} Options: {}",
                            value["title"].as_str().unwrap_or("Input requested"),
                            value["body"].as_str().unwrap_or_default(),
                            value["options"]
                        )
                    };
                }
                "question_resolved"
                    if peer.question.as_ref().is_some_and(|q| {
                        value["question_id"].as_str() == Some(q.question_id.as_str())
                    }) =>
                {
                    peer.question = None;
                    peer.output.clear();
                    peer.seq += 1;
                }
                "finished" => {
                    peer.question = None;
                    peer.status = "done".into();
                    peer.seq += 1;
                    peer.output = value["output"].as_str().unwrap_or_default().to_owned();
                }
                "receipt" => {
                    if let Some(request_id) = value["request_id"].as_str() {
                        if let Some(reply) = peer.pending.remove(request_id) {
                            let result = if value["ok"] == true {
                                Ok(())
                            } else {
                                Err(anyhow::anyhow!(
                                    "{}",
                                    value["error"].as_str().unwrap_or("agent rejected command")
                                ))
                            };
                            let _ = reply.send(result);
                        }
                    }
                }
                _ => {}
            }
            let response_output = if matches!(event, "finished" | "permission") {
                Some(peer.output.clone())
            } else {
                None
            };
            drop(peers);
            if let Some(peer_output) = response_output {
                if peer_output.trim().is_empty() {
                    continue;
                }
                let scope = messages_scope(&messages).await?;
                let job = ResponseJob {
                    id: Uuid::new_v4().to_string(),
                    session: scope.session.clone(),
                    epoch: scope.epoch.clone(),
                    agent_name: id.clone(),
                    instance_id: id.clone(),
                    kind: if event == "permission" {
                        "agent_question"
                    } else {
                        "agent_response"
                    }
                    .into(),
                    output: peer_output,
                    speech: None,
                };
                messages
                    .send(Message::StructuredResponse { job, reply: None })
                    .context("coordinator stopped")?;
            }
        }
        if let Some(id) = registered {
            self.0.lock().await.remove(&id);
        }
        writer.abort();
        Ok(())
    }

    pub async fn snapshot(&self) -> Vec<AgentInfo> {
        let peers = self.0.lock().await;
        let mut agents: Vec<_> = peers
            .iter()
            .map(|(id, p)| AgentInfo {
                display_name: p.name.clone(),
                name: id.clone(),
                kind: Some(p.kind.clone()),
                instance_id: id.clone(),
                cwd: Some(p.cwd.clone().into()),
                status: p.status.clone(),
                state_change_seq: Some(p.seq),
            })
            .collect();
        agents.sort_by(|a, b| a.name.cmp(&b.name));
        agents
    }
    pub async fn read(&self, id: &str) -> Result<String> {
        Ok(self
            .0
            .lock()
            .await
            .get(id)
            .context("agent session disappeared")?
            .output
            .clone())
    }
    pub(crate) async fn question(&self, id: &str) -> Option<crate::question::Question> {
        self.0.lock().await.get(id).and_then(|p| p.question.clone())
    }
    pub(crate) async fn questions(&self) -> Value {
        let peers = self.0.lock().await;
        json!(peers
            .iter()
            .filter_map(|(id, p)| p
                .question
                .as_ref()
                .filter(|q| q.question_kind == "question")
                .map(|q| json!({"agent_id":id,"agent_name":p.name,"question":q})))
            .collect::<Vec<_>>())
    }
    pub async fn command(&self, id: &str, action: &str, value: Value) -> Result<()> {
        let request_id = Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        {
            let mut peers = self.0.lock().await;
            let peer = peers.get_mut(id).context("agent session disappeared")?;
            peer.pending.insert(request_id.clone(), tx);
            peer.sender
                .send(json!({"request_id":request_id,"action":action,"value":value}))
                .context("agent connection closed")?;
        }
        timeout(Duration::from_secs(10), rx)
            .await
            .context("agent command timed out")?
            .context("agent disconnected")?
    }
}

async fn messages_scope(messages: &mpsc::UnboundedSender<Message>) -> Result<Scope> {
    let (reply, rx) = oneshot::channel();
    messages.send(Message::CurrentScope { reply })?;
    Ok(rx.await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pending_question_survives_registration_and_clears_when_work_resumes() {
        let bridge = AgentBridge::default();
        let (server, mut client) = tokio::net::UnixStream::pair().unwrap();
        let (messages, _rx) = mpsc::unbounded_channel();
        let worker = tokio::spawn({
            let bridge = bridge.clone();
            async move { bridge.peer(server, messages).await }
        });
        let question = json!({"question_id":"q1","question_kind":"question","title":"Scope","body":"Which scope?","options":[{"id":"x","label":"Code"}],"multiple":false,"allow_custom":true});
        client.write_all(format!("{}\n", json!({"event":"register","id":"nobs:test","name":"NOBS","status":"blocked","pending_question":question})).as_bytes()).await.unwrap();
        timeout(Duration::from_secs(1), async {
            while bridge.question("nobs:test").await.is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(bridge.questions().await[0]["question"]["question_id"], "q1");
        assert!(bridge.question("nobs:test").await.unwrap().allow_custom);
        client
            .write_all(b"{\"event\":\"status\",\"status\":\"working\"}\n")
            .await
            .unwrap();
        timeout(Duration::from_secs(1), async {
            while bridge.question("nobs:test").await.is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(bridge.questions().await, json!([]));
        drop(client);
        worker.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn terminal_resolution_clears_only_the_matching_question() {
        let bridge = AgentBridge::default();
        let (server, mut client) = tokio::net::UnixStream::pair().unwrap();
        let (messages, _rx) = mpsc::unbounded_channel();
        let worker = tokio::spawn({
            let bridge = bridge.clone();
            async move { bridge.peer(server, messages).await }
        });
        let question = json!({"question_id":"current","question_kind":"question","title":"Scope","body":"Which scope?","options":[],"multiple":false,"allow_custom":true});
        client.write_all(format!("{}\n", json!({"event":"register","id":"codex:test","status":"blocked","pending_question":question})).as_bytes()).await.unwrap();
        client.write_all(b"{\"event\":\"question_resolved\",\"question_id\":\"old\"}\n{\"event\":\"metadata\",\"name\":\"ready\"}\n").await.unwrap();
        timeout(Duration::from_secs(1), async {
            while !bridge
                .snapshot()
                .await
                .iter()
                .any(|a| a.display_name == "ready")
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            bridge.question("codex:test").await.unwrap().question_id,
            "current"
        );
        client
            .write_all(b"{\"event\":\"question_resolved\",\"question_id\":\"current\"}\n")
            .await
            .unwrap();
        timeout(Duration::from_secs(1), async {
            while bridge.question("codex:test").await.is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(bridge.snapshot().await[0].status, "blocked");
        drop(client);
        worker.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn every_connected_agent_forwards_its_response_without_selection() {
        let bridge = AgentBridge::default();
        let (messages, mut received) = mpsc::unbounded_channel();
        let mut clients = Vec::new();
        let mut workers = Vec::new();
        for (id, name) in [("toad:nobs:one", "NOBS"), ("toad:codex:two", "Codex")] {
            let (server, client) = tokio::net::UnixStream::pair().unwrap();
            let bridge = bridge.clone();
            let messages = messages.clone();
            workers.push(tokio::spawn(
                async move { bridge.peer(server, messages).await },
            ));
            let (_, mut writer) = client.into_split();
            writer
                .write_all(
                    format!("{}\n", json!({"event":"register","id":id,"name":name})).as_bytes(),
                )
                .await
                .unwrap();
            clients.push(writer);
        }
        timeout(Duration::from_secs(1), async {
            loop {
                if bridge.snapshot().await.len() == 2 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        for (writer, output) in clients.iter_mut().zip(["NOBS finished", "Codex finished"]) {
            writer
                .write_all(format!("{}\n", json!({"event":"finished","output":output})).as_bytes())
                .await
                .unwrap();
        }
        let mut outputs = Vec::new();
        while outputs.len() < 2 {
            let message = timeout(Duration::from_secs(1), received.recv())
                .await
                .unwrap()
                .unwrap();
            match message {
                Message::CurrentScope { reply } => {
                    reply
                        .send(Scope {
                            session: "toad".into(),
                            epoch: "live".into(),
                        })
                        .unwrap();
                }
                Message::StructuredResponse { job, .. } => {
                    assert_eq!(job.session, "toad");
                    outputs.push((job.agent_name, job.output));
                }
                _ => panic!("unexpected bridge message"),
            }
        }
        outputs.sort();
        assert_eq!(
            outputs,
            [
                ("toad:codex:two".into(), "Codex finished".into()),
                ("toad:nobs:one".into(), "NOBS finished".into())
            ]
        );
        drop(clients);
        for worker in workers {
            worker.await.unwrap().unwrap();
        }
    }

    #[tokio::test]
    async fn registration_and_receipt_use_the_live_connection() {
        let bridge = AgentBridge::default();
        let (server, client) = tokio::net::UnixStream::pair().unwrap();
        let (messages, _rx) = mpsc::unbounded_channel();
        let worker = tokio::spawn({
            let bridge = bridge.clone();
            async move { bridge.peer(server, messages).await }
        });
        let (read, mut write) = client.into_split();
        let mut lines = BufReader::new(read).lines();
        write.write_all(b"{\"event\":\"register\",\"id\":\"nobs:test\",\"kind\":\"nobs\",\"name\":\"NOBS\",\"cwd\":\"/tmp\",\"status\":\"done\",\"output\":\"previous result\"}\n").await.unwrap();
        for _ in 0..20 {
            if bridge
                .snapshot()
                .await
                .first()
                .is_some_and(|a| a.status == "done")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        let snapshot = bridge.snapshot().await;
        assert_eq!(snapshot[0].display_name, "NOBS");
        assert_eq!(snapshot[0].kind.as_deref(), Some("nobs"));
        assert_eq!(bridge.read("nobs:test").await.unwrap(), "previous result");
        write
            .write_all(b"{\"event\":\"status\",\"status\":\"working\"}\n")
            .await
            .unwrap();
        let command = tokio::spawn({
            let bridge = bridge.clone();
            async move {
                bridge
                    .command("nobs:test", "prompt", json!({"text":"hello"}))
                    .await
            }
        });
        let line = timeout(Duration::from_secs(1), lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let sent: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(sent["action"], "prompt");
        write
            .write_all(
                format!(
                    "{}\n",
                    json!({"event":"receipt","request_id":sent["request_id"],"ok":true})
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        command.await.unwrap().unwrap();
        drop(write);
        worker.await.unwrap().unwrap();
        assert!(bridge.snapshot().await.is_empty());
    }
}
