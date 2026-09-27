//! Library surface of the ghostcloak MCP server crate (binary in main.rs).

pub mod auth;
pub mod config;
pub mod http;

#[allow(dead_code, unused_imports, unused_variables, unused_mut)]
mod captcha;
#[allow(dead_code, unused_imports, unused_variables, unused_mut)]
mod ddddocr;
#[allow(dead_code, unused_imports, unused_variables, unused_mut)]
mod geetest;
#[allow(dead_code, unused_imports, unused_variables, unused_mut)]
mod hcaptcha;
#[allow(dead_code, unused_imports, unused_variables, unused_mut)]
mod liveview;
#[allow(dead_code, unused_imports, unused_variables, unused_mut)]
mod ocr;
#[allow(dead_code, unused_imports, unused_variables, unused_mut)]
mod recording;
mod server;

pub use server::GhostcloakServer;
