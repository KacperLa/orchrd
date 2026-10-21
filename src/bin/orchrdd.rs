use anyhow::{bail, Context, Result};
use orchrd::{
    config::{AnnouncementMode, Config},
    controller::{Controller, Event},
    DBUS_NAME, DBUS_PATH,
};
use std::{path::PathBuf, sync::Arc};
use tokio::{sync::broadcast, task::JoinSet};
use zbus::{fdo, Connection, SignalContext};

struct OrchrdApi {
    controller: Arc<Controller>,
}
#[zbus::interface(name = "io.github.kacper.Orchrd1")]
impl OrchrdApi {
    async fn rescan_questions(&self) -> fdo::Result<()> {
        self.controller.rescan_questions().await.map_err(fdo_err)
    }
    async fn start_listening(&self) -> fdo::Result<()> {
        self.controller.start_listening().await.map_err(fdo_err)
    }
    async fn stop_listening(&self) -> fdo::Result<()> {
        self.controller.stop_listening().await.map_err(fdo_err)
    }
    async fn cancel_listening(&self) -> fdo::Result<()> {
        self.controller.cancel_listening().await.map_err(fdo_err)
    }
    async fn dispatch_text(&self, text: &str) -> fdo::Result<()> {
        self.controller
            .dispatch_transcript(text)
            .await
            .map_err(fdo_err)
    }
    async fn repeat_last(&self) -> fdo::Result<()> {
        self.controller.repeat_last().await.map_err(fdo_err)
    }
    async fn set_announcement_mode(&self, mode: &str) -> fdo::Result<()> {
        let mode = match mode {
            "auto" => AnnouncementMode::Auto,
            "muted" | "queued" => AnnouncementMode::Muted,
            _ => return Err(fdo::Error::InvalidArgs("mode must be auto or muted".into())),
        };
        self.controller
            .set_announcement_mode(mode)
            .await
            .map_err(fdo_err)
    }
    async fn approve(&self, id: &str, approve: bool) -> fdo::Result<()> {
        self.controller
            .resolve_approval(id, approve)
            .await
            .map_err(fdo_err)
    }
    async fn get_snapshot(&self) -> fdo::Result<String> {
        serde_json::to_string(&self.controller.snapshot().await).map_err(fdo_err)
    }
    #[zbus(signal)]
    async fn state_changed(ctxt: &SignalContext<'_>, json: &str) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn transcript_ready(ctxt: &SignalContext<'_>, text: &str) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn completion_finished(
        ctxt: &SignalContext<'_>,
        agent_name: &str,
        speech: &str,
    ) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn approval_required(
        ctxt: &SignalContext<'_>,
        id: &str,
        message: &str,
    ) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn error(ctxt: &SignalContext<'_>, message: &str) -> zbus::Result<()>;
}
fn fdo_err(e: impl std::fmt::Display) -> fdo::Error {
    fdo::Error::Failed(e.to_string())
}

#[tokio::main]
async fn main() -> Result<()> {
    let log_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(log_filter).init();
    let config_path = arg_value("--config").map(PathBuf::from);
    let config = Config::load(config_path.as_deref())?;
    let controller = Controller::new(config).await?;
    let connection = zbus::connection::Builder::session()?
        .name(DBUS_NAME)?
        .serve_at(
            DBUS_PATH,
            OrchrdApi {
                controller: Arc::clone(&controller),
            },
        )?
        .build()
        .await
        .context("connect to user D-Bus")?;
    let signals = controller.subscribe();
    let monitor = Arc::clone(&controller);
    let mut tasks = JoinSet::new();
    tasks.spawn(monitor.run());
    tasks.spawn(signal_loop(connection, signals, controller));
    tracing::info!("Orchrd is ready");
    supervise(tasks).await
}
async fn supervise(mut tasks: JoinSet<Result<()>>) -> Result<()> {
    tasks
        .join_next()
        .await
        .context("no controller background tasks")?
        .context("controller background task panicked or was cancelled")??;
    bail!("controller background task stopped unexpectedly")
}

// None requests a fresh snapshot after the receiver falls behind.
async fn next_signal(events: &mut broadcast::Receiver<Event>) -> Result<Option<Event>> {
    match events.recv().await {
        Ok(event) => Ok(Some(event)),
        Err(broadcast::error::RecvError::Lagged(count)) => {
            tracing::warn!(count, "D-Bus receiver lagged; refreshing snapshot");
            Ok(None)
        }
        Err(broadcast::error::RecvError::Closed) => bail!("controller event channel closed"),
    }
}

async fn signal_loop(
    connection: Connection,
    mut events: broadcast::Receiver<Event>,
    controller: Arc<Controller>,
) -> Result<()> {
    let ctxt = SignalContext::new(&connection, DBUS_PATH).context("create D-Bus signal context")?;
    let mut resync = false;
    loop {
        let event = if resync && events.is_empty() {
            resync = false;
            Event::Snapshot(Arc::new(controller.snapshot().await))
        } else {
            match next_signal(&mut events).await? {
                Some(Event::Snapshot(_)) if resync => continue,
                Some(event) => event,
                None => {
                    // Preserve queued notifications, then publish current state.
                    resync = true;
                    continue;
                }
            }
        };
        let result = match event {
            Event::Snapshot(s) => {
                OrchrdApi::state_changed(&ctxt, &serde_json::to_string(s.as_ref())?).await
            }
            Event::Transcript(t) => OrchrdApi::transcript_ready(&ctxt, &t).await,
            Event::Completion { agent_name, speech } => {
                OrchrdApi::completion_finished(&ctxt, &agent_name, &speech).await
            }
            Event::Approval { id, message } => {
                OrchrdApi::approval_required(&ctxt, &id, &message).await
            }
            Event::Error(e) => OrchrdApi::error(&ctxt, &e).await,
        };
        result.context("emit D-Bus signal")?;
    }
}

fn arg_value(flag: &str) -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == flag {
            return args.next();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lag_requests_resync_and_receiver_remains_usable() {
        let (sender, mut receiver) = broadcast::channel(1);
        sender.send(Event::Transcript("old".into())).unwrap();
        sender.send(Event::Transcript("new".into())).unwrap();
        assert!(next_signal(&mut receiver).await.unwrap().is_none());
        assert!(
            matches!(next_signal(&mut receiver).await.unwrap(), Some(Event::Transcript(text)) if text == "new")
        );
        drop(sender);
        assert!(next_signal(&mut receiver).await.is_err());
    }

    #[tokio::test]
    async fn supervisor_fails_on_unexpected_task_exit_or_panic() {
        let mut tasks = JoinSet::new();
        tasks.spawn(async { Ok(()) });
        assert!(supervise(tasks).await.is_err());
        let mut tasks = JoinSet::new();
        tasks.spawn(async {
            panic!("test failure");
            #[allow(unreachable_code)]
            Ok(())
        });
        assert!(supervise(tasks).await.is_err());
    }
}
