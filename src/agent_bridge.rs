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
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader},
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
    sender: mpsc::Sender<Value>,
    pending: HashMap<String, oneshot::Sender<Result<()>>>,
}

impl AgentBridge {
    pub async fn serve(&self, messages: mpsc::Sender<Message>) -> Result<()> {
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
        messages: mpsc::Sender<Message>,
    ) -> Result<()> {
        let (read, mut write) = stream.into_split();
        let mut reader = BufReader::new(read);
        let (sender, mut receiver) = mpsc::channel::<Value>(32);
        let mut writer = tokio::spawn(async move {
            while let Some(value) = receiver.recv().await {
                write.write_all(format!("{value}\n").as_bytes()).await?;
            }
            Ok::<_, anyhow::Error>(())
        });
        let mut registered: Option<String> = None;
        let events = async {
            while let Some(line) = read_frame(&mut reader).await? {
                let value: Value = serde_json::from_slice(&line).context("decode agent event")?;
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
                    let mut peers = self.0.lock().await;
                    if peers.contains_key(id) {
                        bail!("agent id is already connected")
                    }
                    peers.insert(
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
                            let mut remaining = MAX_FRAME_BYTES
                                .saturating_sub(peer.output.len())
                                .min(text.len());
                            while !text.is_char_boundary(remaining) {
                                remaining -= 1;
                            }
                            peer.output.push_str(&text[..remaining]);
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
                        .await
                        .context("coordinator stopped")?;
                }
            }
            Ok::<_, anyhow::Error>(())
        };
        let result = tokio::select! {
            result = events => result,
            result = &mut writer => result.context("agent writer stopped").and_then(|r| r),
        };
        if let Some(id) = registered {
            self.0.lock().await.remove(&id);
        }
        if !writer.is_finished() {
            writer.abort();
            let _ = writer.await;
        }
        result
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
    pub(crate) async fn question_ids(&self) -> HashMap<String, String> {
        self.0
            .lock()
            .await
            .iter()
            .filter_map(|(id, peer)| {
                peer.question
                    .as_ref()
                    .map(|q| (id.clone(), q.question_id.clone()))
            })
            .collect()
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
        self.command_with_timeout(id, action, value, Duration::from_secs(10))
            .await
    }
    async fn command_with_timeout(
        &self,
        id: &str,
        action: &str,
        value: Value,
        deadline: Duration,
    ) -> Result<()> {
        let request_id = Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        let connection;
        {
            let mut peers = self.0.lock().await;
            let peer = peers.get_mut(id).context("agent session disappeared")?;
            connection = peer.sender.clone();
            peer.pending.retain(|_, reply| !reply.is_closed());
            if peer.pending.len() >= 32 {
                bail!("too many pending agent commands; try again later")
            }
            if matches!(action, "permission" | "answer_question") {
                let question = peer
                    .question
                    .as_ref()
                    .context("agent has no pending question")?;
                if value["question_id"].as_str() != Some(question.question_id.as_str()) {
                    bail!("That question is no longer pending")
                }
                if action == "permission" && question.question_kind != "permission" {
                    bail!("Use question controls for this request")
                }
            }
            peer.sender
                .try_send(json!({"request_id":request_id,"action":action,"value":value}))
                .context("agent command queue is full or closed")?;
            peer.pending.insert(request_id.clone(), tx);
        }
        let _cleanup = PendingCommand {
            bridge: self.clone(),
            agent_id: id.into(),
            request_id: request_id.clone(),
            connection,
        };
        let result = timeout(deadline, rx).await;
        if let Some(peer) = self.0.lock().await.get_mut(id) {
            peer.pending.remove(&request_id);
        }
        result
            .context("agent command timed out")?
            .context("agent disconnected")?
    }
}

// Cancellation can drop the command future before its normal timeout cleanup.
struct PendingCommand {
    bridge: AgentBridge,
    agent_id: String,
    request_id: String,
    connection: mpsc::Sender<Value>,
}
impl Drop for PendingCommand {
    fn drop(&mut self) {
        let bridge = self.bridge.clone();
        let id = self.agent_id.clone();
        let request = self.request_id.clone();
        let connection = self.connection.clone();
        tokio::spawn(async move {
            if let Some(peer) = bridge.0.lock().await.get_mut(&id) {
                if peer.sender.same_channel(&connection) {
                    peer.pending.remove(&request);
                }
            }
        });
    }
}

const MAX_FRAME_BYTES: usize = 1_000_000;

// Stop allocating as soon as the limit is reached, even without a newline.
async fn read_frame(reader: &mut (impl AsyncBufRead + Unpin)) -> Result<Option<Vec<u8>>> {
    let mut frame = Vec::new();
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            return Ok((!frame.is_empty()).then_some(frame));
        }
        let newline = chunk.iter().position(|&b| b == b'\n');
        let count = newline.unwrap_or(chunk.len());
        if frame.len() + count > MAX_FRAME_BYTES {
            bail!("oversized agent bridge message")
        }
        frame.extend_from_slice(&chunk[..count]);
        reader.consume(count + usize::from(newline.is_some()));
        if newline.is_some() {
            return Ok(Some(frame));
        }
    }
}

async fn messages_scope(messages: &mpsc::Sender<Message>) -> Result<Scope> {
    let (reply, rx) = oneshot::channel();
    messages.send(Message::CurrentScope { reply }).await?;
    Ok(rx.await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn connected() -> (
        AgentBridge,
        tokio::net::UnixStream,
        tokio::task::JoinHandle<Result<()>>,
    ) {
        let bridge = AgentBridge::default();
        let (server, mut client) = tokio::net::UnixStream::pair().unwrap();
        let (messages, _rx) = mpsc::channel(256);
        let worker = tokio::spawn({
            let bridge = bridge.clone();
            async move { bridge.peer(server, messages).await }
        });
        client
            .write_all(b"{\"event\":\"register\",\"id\":\"test\"}\n")
            .await
            .unwrap();
        timeout(Duration::from_secs(1), async {
            while bridge.snapshot().await.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        (bridge, client, worker)
    }

    #[tokio::test]
    async fn malformed_event_removes_agent_and_closes_writer() {
        let (bridge, mut client, worker) = connected().await;
        client.write_all(b"not-json\n").await.unwrap();
        assert!(timeout(Duration::from_secs(1), worker)
            .await
            .unwrap()
            .unwrap()
            .is_err());
        assert!(bridge.snapshot().await.is_empty());
        let mut reader = BufReader::new(client);
        assert!(timeout(Duration::from_secs(1), reader.fill_buf())
            .await
            .unwrap()
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn oversized_unterminated_frame_is_rejected_and_removes_agent() {
        let (bridge, mut client, worker) = connected().await;
        // Keep the client open: the server must reject before EOF or a newline.
        let write = tokio::spawn(async move {
            let _ = client.write_all(&vec![b'x'; MAX_FRAME_BYTES + 1]).await;
            client
        });
        assert!(timeout(Duration::from_secs(2), worker)
            .await
            .unwrap()
            .unwrap()
            .is_err());
        assert!(bridge.snapshot().await.is_empty());
        drop(write.await.unwrap());
    }

    #[tokio::test]
    async fn duplicate_connection_cannot_replace_or_remove_original_agent() {
        let (bridge, client, worker) = connected().await;
        let (server, mut duplicate) = tokio::net::UnixStream::pair().unwrap();
        let (messages, _rx) = mpsc::channel(256);
        let second = tokio::spawn({
            let bridge = bridge.clone();
            async move { bridge.peer(server, messages).await }
        });
        duplicate
            .write_all(b"{\"event\":\"register\",\"id\":\"test\"}\n")
            .await
            .unwrap();
        assert!(timeout(Duration::from_secs(1), second)
            .await
            .unwrap()
            .unwrap()
            .is_err());
        assert_eq!(bridge.snapshot().await.len(), 1);
        drop(client);
        worker.await.unwrap().unwrap();
        assert!(bridge.snapshot().await.is_empty());
    }

    #[tokio::test]
    async fn timeout_and_cancellation_clear_pending_commands() {
        let (bridge, client, worker) = connected().await;
        let mut client = BufReader::new(client);
        let result = bridge
            .command_with_timeout(
                "test",
                "prompt",
                json!({"text":"hello"}),
                Duration::from_millis(10),
            )
            .await;
        assert!(result.unwrap_err().to_string().contains("timed out"));
        assert!(bridge.0.lock().await["test"].pending.is_empty());
        assert!(client.read_line(&mut String::new()).await.unwrap() > 0);
        let command = tokio::spawn({
            let bridge = bridge.clone();
            async move {
                bridge
                    .command("test", "prompt", json!({"text":"hello"}))
                    .await
            }
        });
        timeout(Duration::from_secs(1), async {
            while bridge.0.lock().await["test"].pending.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(client.read_line(&mut String::new()).await.unwrap() > 0);
        command.abort();
        let _ = command.await;
        timeout(Duration::from_secs(1), async {
            while !bridge.0.lock().await["test"].pending.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        drop(client);
        worker.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn full_command_queue_rejects_without_registering_delivery() {
        let (bridge, client, worker) = connected().await;
        let (sender, _receiver) = mpsc::channel(1);
        sender.try_send(json!({})).unwrap();
        bridge.0.lock().await.get_mut("test").unwrap().sender = sender;
        assert!(bridge
            .command("test", "prompt", json!({"text":"hello"}))
            .await
            .is_err());
        assert!(bridge.0.lock().await["test"].pending.is_empty());
        drop(client);
        worker.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn stale_or_missing_permission_id_never_reaches_agent() {
        let (bridge, client, worker) = connected().await;
        bridge.0.lock().await.get_mut("test").unwrap().question = Some(serde_json::from_value(json!({
            "question_id":"new", "question_kind":"permission", "title":"Permission", "body":"Write?",
            "options":[], "multiple":false, "allow_custom":false
        })).unwrap());
        for value in [
            json!({"approve":true}),
            json!({"approve":true,"question_id":"old"}),
        ] {
            assert!(bridge
                .command("test", "permission", value)
                .await
                .unwrap_err()
                .to_string()
                .contains("no longer pending"));
        }
        assert!(bridge.0.lock().await["test"].pending.is_empty());
        let mut reader = BufReader::new(client);
        assert!(timeout(Duration::from_millis(20), reader.fill_buf())
            .await
            .is_err());
        drop(reader);
        worker.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn frame_boundary_accepts_limit_and_preserves_following_frame() {
        let input = [vec![b'x'; MAX_FRAME_BYTES], b"\nnext\n".to_vec()].concat();
        let mut reader = BufReader::new(input.as_slice());
        assert_eq!(
            read_frame(&mut reader).await.unwrap().unwrap().len(),
            MAX_FRAME_BYTES
        );
        assert_eq!(read_frame(&mut reader).await.unwrap().unwrap(), b"next");
        assert!(read_frame(&mut reader).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn pending_question_survives_registration_and_clears_when_work_resumes() {
        let bridge = AgentBridge::default();
        let (server, mut client) = tokio::net::UnixStream::pair().unwrap();
        let (messages, _rx) = mpsc::channel(256);
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
        let (messages, _rx) = mpsc::channel(256);
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
        let (messages, mut received) = mpsc::channel(256);
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
        let (messages, _rx) = mpsc::channel(256);
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
