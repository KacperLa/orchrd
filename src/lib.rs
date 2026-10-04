pub mod agent;
pub mod agent_bridge;
pub mod config;
pub mod controller;
mod coordinator;
pub mod db;
pub mod intent;
mod speech_api;
mod stt;
pub mod tts;
pub mod voxtype;

pub const DBUS_NAME: &str = "io.github.kacper.Orchrd1";
pub const DBUS_PATH: &str = "/io/github/kacper/Orchrd1";
mod activity;

mod request_writer;

mod routing;

mod question;
