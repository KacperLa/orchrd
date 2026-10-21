pub mod agent;
pub mod config;
pub mod controller;
mod coordinator;
pub mod db;
mod dialog;
mod herdr;
pub mod intent;
mod speech_api;
mod stt;
mod terminal_input;
pub mod tts;

pub const DBUS_NAME: &str = "io.github.kacper.Orchrd1";
pub const DBUS_PATH: &str = "/io/github/kacper/Orchrd1";
mod activity;

mod request_writer;

mod routing;

mod question;
