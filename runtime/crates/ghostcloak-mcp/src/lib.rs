//! Library surface of the ghostcloak MCP server crate (binary in main.rs).

pub mod auth;
pub mod config;
pub mod http;

mod captcha;
mod ddddocr;
mod geetest;
mod hcaptcha;
mod liveview;
mod ocr;
mod recording;
mod server;

pub use server::GhostcloakServer;
