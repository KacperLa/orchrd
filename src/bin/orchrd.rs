use anyhow::{bail, Context, Result};
use std::{env, os::unix::process::CommandExt, process::Command};

fn main() -> Result<()> {
    let mut args = env::args_os().skip(1);
    match args.next().as_deref() {
        Some(command) if command == "codex" => {
            let runtime = env::var("XDG_RUNTIME_DIR")
                .unwrap_or_else(|_| format!("/run/user/{}", unsafe { libc::getuid() }));
            let socket = env::var("ORCHRD_CODEX_SOCKET")
                .unwrap_or_else(|_| "%t/orchrd-codex/server.sock".into())
                .replace("%t", &runtime);
            let executable = env::var_os("ORCHRD_CODEX_COMMAND").unwrap_or_else(|| "codex".into());
            let error = Command::new(executable)
                .arg("--remote")
                .arg(format!("unix://{socket}"))
                .args(args)
                .exec();
            Err(error).context("Could not launch Codex")
        }
        None => {
            println!(
                "Usage: orchrd codex [CODEX_ARGS...]\nLaunch Codex on Orchard's shared app server."
            );
            Ok(())
        }
        Some(command) if command == "--help" || command == "-h" => {
            println!("Usage: orchrd codex [CODEX_ARGS...]\nLaunch Codex on Orchard's shared app server.\n\nOverrides: ORCHRD_CODEX_SOCKET, ORCHRD_CODEX_COMMAND");
            Ok(())
        }
        _ => bail!("usage: orchrd codex [CODEX_ARGS...]"),
    }
}
