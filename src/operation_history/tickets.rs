use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use rand::{Rng, distr::Alphanumeric};
use sha2::{Digest, Sha256};

use super::UndoTicket;

pub fn generate_confirmation_token() -> String {
    rand::rng()
        .sample_iter(Alphanumeric)
        .take(12)
        .map(char::from)
        .collect::<String>()
        .to_ascii_uppercase()
}

pub(super) fn hash_token(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(super) fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |accumulator, (left, right)| {
            accumulator | (left ^ right)
        })
        == 0
}

pub(super) fn validate_ticket(
    ticket: &UndoTicket,
    token: &str,
    current_provider: &str,
    now: DateTime<Utc>,
) -> Result<()> {
    if ticket.is_expired(now) {
        bail!("undo ticket has expired");
    }
    if !ticket.confirms(token) {
        bail!("undo confirmation token is invalid");
    }
    if !ticket.valid_for_current(current_provider) {
        bail!("current Provider is no longer the switch target");
    }
    Ok(())
}
