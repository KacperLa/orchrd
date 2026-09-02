use anyhow::{Context, Result};
use orchard::{
    config::{AnnouncementMode, Config},
    controller::{Controller, Event},
    DBUS_NAME, DBUS_PATH,
};
use std::{path::PathBuf, sync::Arc};
use zbus::{fdo, Connection, SignalContext};

struct OrchardApi {
    controller: Arc<Controller>,
}
#[zbus::interface(name = "io.github.kacper.Orchard1")]
impl OrchardApi {
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
    async fn speak_next(&self) -> fdo::Result<()> {
        self.controller.speak_next().await.map_err(fdo_err)
    }
    async fn dismiss_next(&self) -> fdo::Result<()> {
        self.controller.dismiss_next().await.map_err(fdo_err)
    }
    async fn set_announcement_mode(&self, mode: &str) -> fdo::Result<()> {
        let mode = match mode {
            "auto" => AnnouncementMode::Auto,
            "queued" => AnnouncementMode::Queued,
            _ => {
                return Err(fdo::Error::InvalidArgs(
                    "mode must be auto or queued".into(),
                ))
            }
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
    async fn completion_queued(
        ctxt: &SignalContext<'_>,
        project: &str,
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
            OrchardApi {
                controller: Arc::clone(&controller),
            },
        )?
        .build()
        .await
        .context("connect to user D-Bus")?;
    let monitor = Arc::clone(&controller);
    tokio::spawn(async move { monitor.monitor().await });
    let signals = controller.subscribe();
    tokio::spawn(signal_loop(connection.clone(), signals));
    tracing::info!("Orchard is ready");
    std::future::pending::<()>().await;
    Ok(())
}
async fn signal_loop(connection: Connection, mut events: tokio::sync::broadcast::Receiver<Event>) {
    let ctxt = match SignalContext::new(&connection, DBUS_PATH) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("create D-Bus signal context: {e}");
            return;
        }
    };
    while let Ok(event) = events.recv().await {
        let result = match event {
            Event::Snapshot(s) => {
                OrchardApi::state_changed(&ctxt, &serde_json::to_string(&s).unwrap_or_default())
                    .await
            }
            Event::Transcript(t) => OrchardApi::transcript_ready(&ctxt, &t).await,
            Event::Completion { project, speech } => {
                OrchardApi::completion_queued(&ctxt, &project, &speech).await
            }
            Event::Approval { id, message } => {
                OrchardApi::approval_required(&ctxt, &id, &message).await
            }
            Event::Error(e) => OrchardApi::error(&ctxt, &e).await,
        };
        if let Err(e) = result {
            tracing::warn!("emit D-Bus signal: {e}")
        }
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
