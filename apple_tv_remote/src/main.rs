use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::Parser;

use apple_tv_remote::layout::{DEFAULT_CORNER_MM, DEFAULT_EDGE_MM, DEFAULT_SCROLL_STEP_MM};
use apple_tv_remote::touch::MotionConfig;

#[derive(Parser, Debug)]
#[command(
    name = "apple-tv-remote",
    about = "Apple TV Siri Remote touchpad and buttons",
    // Negative numbers reach validate() so it can name the offending knob,
    // instead of clap rejecting them as unknown flags.
    allow_negative_numbers = true
)]
struct Args {
    /// Bluetooth address of the paired remote.
    #[arg(long, default_value = "28:EC:95:12:B9:E5")]
    mac: String,
    /// HCI controller index.
    #[arg(long, default_value_t = 0)]
    device: u16,
    /// Relative pointer sensitivity multiplier.
    #[arg(long, default_value_t = 1.0)]
    sensitivity: f64,
    /// Base pointer travel in pixels per millimetre of finger motion.
    #[arg(long, default_value_t = 3.0)]
    pointer_speed: f64,
    /// Maximum additional gain for fast swipes.
    #[arg(long, default_value_t = 3.0)]
    acceleration: f64,
    /// Finger speed in mm/s where half the acceleration is applied.
    #[arg(long, default_value_t = 80.0)]
    acceleration_threshold: f64,
    /// Milliseconds each report's pointer travel is spread over.
    #[arg(
        long = "interpolation-ms",
        visible_alias = "smoothing-ms",
        default_value_t = 6.0,
        value_name = "MS"
    )]
    interpolation_ms: f64,
    /// Pointer updates per second while interpolating.
    #[arg(long, default_value_t = 240.0, value_name = "HZ")]
    output_rate: f64,
    /// Width of the right and top scroll strips in millimetres.
    #[arg(long, default_value_t = DEFAULT_EDGE_MM)]
    edge_mm: f64,
    /// Side of the top-right right-click zone; at least --edge-mm.
    #[arg(long, default_value_t = DEFAULT_CORNER_MM)]
    corner_mm: f64,
    /// Finger travel per scroll notch in the edge strips.
    #[arg(long, default_value_t = DEFAULT_SCROLL_STEP_MM)]
    scroll_step_mm: f64,
    /// Request a short Bluetooth connection interval on connect.
    #[arg(
        long,
        default_value_t = true,
        default_missing_value = "true",
        num_args = 0..=1,
        value_name = "BOOL",
        overrides_with = "no_tune_link"
    )]
    tune_link: bool,
    /// Leave the Bluetooth connection interval unchanged.
    #[arg(long, action = clap::ArgAction::SetTrue, overrides_with = "tune_link")]
    no_tune_link: bool,
    /// Override the automatically detected hidraw device.
    #[arg(long, value_name = "PATH")]
    hidraw: Option<PathBuf>,
}

impl Args {
    fn tune_link(&self) -> bool {
        self.tune_link && !self.no_tune_link
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "apple_tv_remote=info".into()),
        )
        .init();
    let args = Args::parse();
    validate(&args)?;
    let options = apple_tv_remote::Options {
        mac: args.mac.clone(),
        hidraw: args.hidraw.clone(),
        pointer: MotionConfig::new(
            args.sensitivity,
            args.pointer_speed,
            args.acceleration,
            args.acceleration_threshold,
            args.edge_mm,
            args.corner_mm,
            args.scroll_step_mm,
            args.interpolation_ms / 1000.0,
        ),
        tick_interval: Duration::from_secs_f64(1.0 / args.output_rate),
        tune_link: args.tune_link(),
        device: args.device,
    };
    if args.hidraw.is_none() {
        apple_tv_remote::warn_if_unpaired(&args.mac)?;
    }
    apple_tv_remote::run(options)
}

fn validate(args: &Args) -> Result<()> {
    if args.sensitivity <= 0.0 {
        bail!("--sensitivity must be greater than zero");
    }
    if args.pointer_speed <= 0.0 {
        bail!("--pointer-speed must be greater than zero");
    }
    if args.acceleration < 0.0 {
        bail!("--acceleration cannot be negative");
    }
    if args.acceleration_threshold <= 0.0 {
        bail!("--acceleration-threshold must be greater than zero");
    }
    if args.interpolation_ms < 0.0 {
        bail!("--interpolation-ms cannot be negative");
    }
    if args.output_rate <= 0.0 {
        bail!("--output-rate must be greater than zero");
    }
    if args.edge_mm <= 0.0 {
        bail!("--edge-mm must be greater than zero");
    }
    if args.corner_mm < args.edge_mm {
        bail!("--corner-mm must be at least --edge-mm");
    }
    if args.scroll_step_mm <= 0.0 {
        bail!("--scroll-step-mm must be greater than zero");
    }
    apple_tv_remote::bluetooth::parse_mac(&args.mac).context("--mac")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(arguments: &[&str]) -> Args {
        Args::try_parse_from(std::iter::once("apple-tv-remote").chain(arguments.iter().copied()))
            .expect("arguments parse")
    }

    #[test]
    fn the_smoothing_defaults_match_the_python_daemon() {
        let args = parse(&[]);
        assert_eq!(args.interpolation_ms, 6.0);
        assert_eq!(args.output_rate, 240.0);
        // --smoothing-ms was the Python daemon's alias for the same knob.
        assert_eq!(parse(&["--smoothing-ms", "12"]).interpolation_ms, 12.0);
    }

    #[test]
    fn boolean_flags_accept_bare_valued_and_negated_forms() {
        assert!(parse(&[]).tune_link());
        assert!(parse(&["--tune-link"]).tune_link());
        assert!(!parse(&["--tune-link", "false"]).tune_link());
        assert!(!parse(&["--no-tune-link"]).tune_link());
        assert!(parse(&["--no-tune-link", "--tune-link"]).tune_link());
    }

    #[test]
    fn the_hidraw_override_parses_and_bad_numbers_are_rejected() {
        let args = parse(&["--hidraw", "/dev/hidraw3"]);
        assert_eq!(args.hidraw, Some(PathBuf::from("/dev/hidraw3")));
        assert!(validate(&args).is_ok());
        assert!(validate(&parse(&["--interpolation-ms", "-1"])).is_err());
        assert!(validate(&parse(&["--output-rate", "0"])).is_err());
    }
}
