use anyhow::Result;
use clap::Parser;

use apple_tv_remote::Options;

#[derive(Parser, Debug)]
#[command(
    name = "apple-tv-remote",
    about = "Apple TV Siri Remote Bluetooth connection setup"
)]
struct Args {
    /// Bluetooth address of the paired remote.
    #[arg(long, default_value = "28:EC:95:12:B9:E5")]
    mac: String,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "apple_tv_remote=info".into()),
        )
        .init();
    let args = Args::parse();
    apple_tv_remote::run(Options { mac: args.mac })
}
