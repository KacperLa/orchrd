use anyhow::{bail, Result};
use orchrd::{DBUS_NAME, DBUS_PATH};
use zbus::Proxy;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = args.first().map(String::as_str).unwrap_or("status");
    let conn = zbus::Connection::session().await?;
    let proxy = Proxy::new(&conn, DBUS_NAME, DBUS_PATH, "io.github.kacper.Orchrd1").await?;
    match command {
        "status" => {
            let snapshot: String = proxy.call("GetSnapshot", &()).await?;
            let value: serde_json::Value = serde_json::from_str(&snapshot)?;
            println!("{}", serde_json::to_string_pretty(&value)?)
        }
        "start" => proxy.call::<_, _, ()>("StartListening", &()).await?,
        "stop" => proxy.call::<_, _, ()>("StopListening", &()).await?,
        "cancel" => proxy.call::<_, _, ()>("CancelListening", &()).await?,
        "repeat-last" => proxy.call::<_, _, ()>("RepeatLast", &()).await?,
        "mode" => {
            let mode = args
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("mode requires auto or muted"))?;
            proxy
                .call::<_, _, ()>("SetAnnouncementMode", &(mode.as_str(),))
                .await?
        }
        "approve" | "deny" => {
            let id = args
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("{command} requires an approval id"))?;
            proxy
                .call::<_, _, ()>("Approve", &(id.as_str(), command == "approve"))
                .await?
        }
        "dispatch" => {
            let text = args.get(1..).unwrap_or_default().join(" ");
            if text.is_empty() {
                bail!("dispatch requires text")
            };
            proxy
                .call::<_, _, ()>("DispatchText", &(text.as_str(),))
                .await?
        }
        _ => bail!("usage: orchrdctl [status|start|stop|cancel|repeat-last|mode auto|mode muted|approve ID|deny ID|dispatch TEXT]"),
    };
    Ok(())
}
