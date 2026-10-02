// Suppress pre-existing warnings in legacy code that was previously binary-only
// and is now compiled as a library for the HTTP transport feature.
#![allow(dead_code, unused_imports, unused_variables, unused_mut, clippy::all)]

//! Library surface of the ghostcloak MCP server crate (binary in main.rs).

pub mod auth;
pub mod config;
pub mod http;

mod liveview;
pub mod recording;
pub mod server;

pub use server::GhostcloakServer;
