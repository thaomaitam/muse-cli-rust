//! Rust rewrite of muse-cli (original: Python, version 0.3.2).
//!
//! Module ownership is fixed by CONTRACT.md. Do not move a module's
//! code into another module without updating CONTRACT.md first.

pub mod chat;
pub mod cli;
pub mod commands;
pub mod config;
pub mod errors;
pub mod fsclient;
pub mod gateway;
pub mod media;
pub mod noise;
pub mod output;
pub mod proto;
pub mod routes;
pub mod update;
pub mod usage;
pub mod wait;
