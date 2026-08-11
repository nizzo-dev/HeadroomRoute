use crate::{
    config,
    model::AuthStyle,
    notification,
    state::{AppState, should_stop},
};
use anyhow::{Context, Result, anyhow};
use rand::{Rng, distr::Alphanumeric};
use reqwest::blocking::Client;
use std::{
    fs,
    io::Write,
    net::{TcpListener, TcpStream},
    path::Path,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

mod client;
mod control_api;
mod http;
mod request_policy;
mod server;
#[cfg(test)]
mod tests;

use client::handle;
pub use server::run;

use control_api::{compat_status, stable_status, valid_control_token};
use http::{Incoming, read_request, write_json};
use request_policy::{
    is_ai_conversation_path, is_hop_header, is_route_failure, join_url,
    should_forward_request_header, top_level_model,
};

#[cfg(test)]
use http::decode_chunked;

pub fn load_or_create_token(state_dir: &Path, legacy_dir: &Path) -> Result<String> {
    let path = state_dir.join("control.token");
    if let Ok(token) = fs::read_to_string(&path)
        && token.trim().len() >= 32
    {
        mirror_token(legacy_dir, token.trim())?;
        return Ok(token.trim().to_owned());
    }
    fs::create_dir_all(state_dir)?;
    let token: String = rand::rng()
        .sample_iter(Alphanumeric)
        .take(48)
        .map(char::from)
        .collect();
    fs::write(&path, &token)?;
    mirror_token(legacy_dir, &token)?;
    Ok(token)
}

fn mirror_token(dir: &Path, token: &str) -> Result<()> {
    fs::create_dir_all(dir)?;
    fs::write(dir.join("control.token"), token)?;
    Ok(())
}
