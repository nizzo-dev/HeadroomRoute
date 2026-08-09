//! Bounded, redacted operation history with cooldown and undo tickets.

use crate::model::Protocol;
use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Duration, Utc};
use rand::{Rng, distr::Alphanumeric};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fmt, fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::{REPLACEFILE_WRITE_THROUGH, ReplaceFileW};

pub const DEFAULT_MAX_ENTRIES: usize = 200;
pub const DEFAULT_COOLDOWN: Duration = Duration::minutes(5);
pub const DEFAULT_UNDO_LIFETIME: Duration = Duration::minutes(30);

const HISTORY_SCHEMA_VERSION: u32 = 1;
const REDACTED: &str = "[REDACTED]";

/// A clock abstraction keeps expiration and cooldown tests deterministic.
#[derive(Clone)]
pub struct Clock {
    tick: Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>,
}

impl Clock {
    pub fn real() -> Self {
        Self {
            tick: Arc::new(Utc::now),
        }
    }

    pub fn manual(start: DateTime<Utc>) -> (Self, ManualTime) {
        let cell = Arc::new(Mutex::new(start));
        let tick_cell = Arc::clone(&cell);
        let tick = Arc::new(move || *tick_cell.lock().expect("clock cell poisoned"));
        (Self { tick }, ManualTime { cell })
    }

    pub fn now(&self) -> DateTime<Utc> {
        (self.tick)()
    }
}

#[derive(Clone)]
pub struct ManualTime {
    cell: Arc<Mutex<DateTime<Utc>>>,
}

impl ManualTime {
    pub fn advance(&self, amount: Duration) {
        *self.cell.lock().expect("clock cell poisoned") += amount;
    }

    pub fn set(&self, at: DateTime<Utc>) {
        *self.cell.lock().expect("clock cell poisoned") = at;
    }
}

/// Per-protocol cooldown that also blocks the provider which just failed.
pub struct SwitchCooldown {
    duration: Duration,
    clock: Clock,
    last_auto_switch: HashMap<Protocol, DateTime<Utc>>,
    blocked_providers: HashMap<Protocol, HashMap<String, DateTime<Utc>>>,
}

impl SwitchCooldown {
    pub fn new() -> Self {
        Self::with_clock_and_duration(Clock::real(), DEFAULT_COOLDOWN)
    }

    pub fn with_duration(duration: Duration) -> Self {
        Self::with_clock_and_duration(Clock::real(), duration)
    }

    pub fn with_clock(clock: Clock) -> Self {
        Self::with_clock_and_duration(clock, DEFAULT_COOLDOWN)
    }

    pub fn with_clock_and_duration(clock: Clock, duration: Duration) -> Self {
        Self {
            duration: duration.max(Duration::zero()),
            clock,
            last_auto_switch: HashMap::new(),
            blocked_providers: HashMap::new(),
        }
    }

    pub fn duration(&self) -> Duration {
        self.duration
    }

    pub fn clock(&self) -> &Clock {
        &self.clock
    }

    pub fn protocol_allowed(&self, protocol: Protocol) -> bool {
        self.last_auto_switch
            .get(&protocol)
            .is_none_or(|at| self.clock.now() >= *at + self.duration)
    }

    pub fn protocol_ready_at(&self, protocol: Protocol) -> Option<DateTime<Utc>> {
        self.last_auto_switch
            .get(&protocol)
            .map(|at| *at + self.duration)
            .filter(|ready| self.clock.now() < *ready)
    }

    pub fn remaining(&self, protocol: Protocol) -> Duration {
        let now = self.clock.now();
        self.protocol_ready_at(protocol)
            .map(|ready| (ready - now).max(Duration::zero()))
            .unwrap_or_else(Duration::zero)
    }

    pub fn mark_auto_switch(&mut self, protocol: Protocol) {
        self.last_auto_switch.insert(protocol, self.clock.now());
    }

    pub fn block_provider(&mut self, protocol: Protocol, provider: &str) {
        self.blocked_providers
            .entry(protocol)
            .or_default()
            .insert(provider.to_owned(), self.clock.now() + self.duration);
    }

    pub fn provider_allowed(&self, protocol: Protocol, provider: &str) -> bool {
        self.provider_blocked_until(protocol, provider).is_none()
    }

    pub fn provider_blocked_until(
        &self,
        protocol: Protocol,
        provider: &str,
    ) -> Option<DateTime<Utc>> {
        self.blocked_providers
            .get(&protocol)
            .and_then(|providers| providers.get(provider))
            .filter(|until| **until > self.clock.now())
            .copied()
    }

    pub fn target_allowed(&self, protocol: Protocol, target: &str) -> bool {
        self.protocol_allowed(protocol) && self.provider_allowed(protocol, target)
    }

    pub fn apply_auto_failover(&mut self, protocol: Protocol, failed_provider: &str) {
        self.mark_auto_switch(protocol);
        self.block_provider(protocol, failed_provider);
    }

    pub fn prune(&mut self) {
        let now = self.clock.now();
        self.last_auto_switch
            .retain(|_, at| *at + self.duration > now);
        for providers in self.blocked_providers.values_mut() {
            providers.retain(|_, until| *until > now);
        }
        self.blocked_providers
            .retain(|_, providers| !providers.is_empty());
    }

    /// Rebuild cooldown state after loading history during startup.
    pub fn from_history(history: &OperationHistory, duration: Duration) -> Self {
        let clock = history.clock().clone();
        let mut cooldown = Self::with_clock_and_duration(clock, duration);
        let now = cooldown.clock.now();
        for record in history.entries() {
            if record.kind != OperationKind::AutoFailover
                || record.outcome != OperationOutcome::Succeeded
            {
                continue;
            }
            let ready_at = record.occurred_at + cooldown.duration;
            if ready_at <= now {
                continue;
            }
            let replace = cooldown
                .last_auto_switch
                .get(&record.protocol)
                .is_none_or(|at| *at < record.occurred_at);
            if replace {
                cooldown
                    .last_auto_switch
                    .insert(record.protocol, record.occurred_at);
            }
            if let Some(from) = record.from_provider.as_deref() {
                cooldown
                    .blocked_providers
                    .entry(record.protocol)
                    .or_default()
                    .entry(from.to_owned())
                    .and_modify(|until| *until = (*until).max(ready_at))
                    .or_insert(ready_at);
            }
        }
        cooldown
    }
}

impl Default for SwitchCooldown {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    ManualSwitch,
    AutoFailover,
    UndoSwitch,
    CooldownBlocked,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationOutcome {
    Succeeded,
    Blocked,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OperationRecord {
    pub id: String,
    pub occurred_at: DateTime<Utc>,
    pub kind: OperationKind,
    pub protocol: Protocol,
    pub from_provider: Option<String>,
    pub to_provider: Option<String>,
    pub reason: String,
    pub outcome: OperationOutcome,
    pub undo_ticket_id: Option<String>,
    pub undone_by: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UndoTicket {
    pub id: String,
    pub record_id: String,
    pub protocol: Protocol,
    pub switched_to: String,
    pub restore_provider: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub confirmation_hash: String,
}

impl UndoTicket {
    pub fn confirms(&self, token: &str) -> bool {
        let given = hash_token(token);
        constant_time_eq(given.as_bytes(), self.confirmation_hash.as_bytes())
    }

    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        now >= self.expires_at
    }

    pub fn valid_for_current(&self, current_provider: &str) -> bool {
        self.switched_to == current_provider
    }
}

#[derive(Clone)]
pub struct UndoGrant {
    pub ticket: UndoTicket,
    pub confirmation_token: String,
}

impl fmt::Debug for UndoGrant {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UndoGrant")
            .field("ticket", &self.ticket)
            .field("confirmation_token", &REDACTED)
            .finish()
    }
}

pub struct OperationHistory {
    entries: Vec<OperationRecord>,
    undo_tickets: Vec<UndoTicket>,
    max_entries: usize,
    undo_lifetime: Duration,
    next_seq: u64,
    clock: Clock,
}

impl OperationHistory {
    pub fn new() -> Self {
        Self::with_clock_and_limits(Clock::real(), DEFAULT_MAX_ENTRIES, DEFAULT_UNDO_LIFETIME)
    }

    pub fn with_clock(clock: Clock) -> Self {
        Self::with_clock_and_limits(clock, DEFAULT_MAX_ENTRIES, DEFAULT_UNDO_LIFETIME)
    }

    pub fn with_limits(max_entries: usize, undo_lifetime: Duration) -> Self {
        Self::with_clock_and_limits(Clock::real(), max_entries, undo_lifetime)
    }

    pub fn with_clock_and_limits(
        clock: Clock,
        max_entries: usize,
        undo_lifetime: Duration,
    ) -> Self {
        Self {
            entries: Vec::new(),
            undo_tickets: Vec::new(),
            max_entries: max_entries.max(1),
            undo_lifetime: undo_lifetime.max(Duration::zero()),
            next_seq: 0,
            clock,
        }
    }

    pub fn clock(&self) -> &Clock {
        &self.clock
    }

    pub fn now(&self) -> DateTime<Utc> {
        self.clock.now()
    }

    pub fn max_entries(&self) -> usize {
        self.max_entries
    }

    pub fn undo_lifetime(&self) -> Duration {
        self.undo_lifetime
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entries(&self) -> &[OperationRecord] {
        &self.entries
    }

    pub fn tickets(&self) -> &[UndoTicket] {
        &self.undo_tickets
    }

    pub fn undo_ticket(&self, id: &str) -> Option<&UndoTicket> {
        self.undo_tickets.iter().find(|ticket| ticket.id == id)
    }

    pub fn record_manual_switch(
        &mut self,
        protocol: Protocol,
        from: Option<&str>,
        to: &str,
        reason: &str,
    ) -> Result<(OperationRecord, Option<UndoGrant>)> {
        let reason = require_clean(reason)?;
        ensure_identifier_clean(to)?;
        if let Some(from) = from {
            ensure_identifier_clean(from)?;
        }
        let mut record = self.make_record(
            OperationKind::ManualSwitch,
            protocol,
            from,
            Some(to),
            reason,
            OperationOutcome::Succeeded,
        );
        let grant = from
            .filter(|from| *from != to)
            .map(|from| self.issue_grant(record.id.clone(), protocol, to, from, &mut record));
        let stored = record.clone();
        self.push_record(record);
        Ok((stored, grant))
    }

    pub fn record_auto_failover(
        &mut self,
        protocol: Protocol,
        from: &str,
        to: &str,
        reason: &str,
    ) -> Result<(OperationRecord, UndoGrant)> {
        if from == to {
            bail!("automatic failover must change Provider");
        }
        let reason = require_clean(reason)?;
        ensure_identifier_clean(from)?;
        ensure_identifier_clean(to)?;
        let mut record = self.make_record(
            OperationKind::AutoFailover,
            protocol,
            Some(from),
            Some(to),
            reason,
            OperationOutcome::Succeeded,
        );
        let grant = self.issue_grant(record.id.clone(), protocol, to, from, &mut record);
        let stored = record.clone();
        self.push_record(record);
        Ok((stored, grant))
    }

    pub fn record_blocked_failover(
        &mut self,
        protocol: Protocol,
        from: &str,
        to: Option<&str>,
        reason: &str,
    ) -> Result<OperationRecord> {
        let reason = require_clean(reason)?;
        ensure_identifier_clean(from)?;
        if let Some(to) = to {
            ensure_identifier_clean(to)?;
        }
        let record = self.make_record(
            OperationKind::CooldownBlocked,
            protocol,
            Some(from),
            to,
            reason,
            OperationOutcome::Blocked,
        );
        let stored = record.clone();
        self.push_record(record);
        Ok(stored)
    }

    pub fn verify_undo(
        &self,
        ticket_id: &str,
        confirmation_token: &str,
        current_provider: &str,
    ) -> Result<()> {
        let ticket = self
            .undo_ticket(ticket_id)
            .ok_or_else(|| anyhow!("undo ticket does not exist or was already used"))?;
        validate_ticket(ticket, confirmation_token, current_provider, self.now())
    }

    pub fn record_undo(
        &mut self,
        ticket_id: &str,
        confirmation_token: &str,
        current_provider: &str,
    ) -> Result<OperationRecord> {
        let now = self.now();
        let ticket_position = self
            .undo_tickets
            .iter()
            .position(|ticket| ticket.id == ticket_id)
            .ok_or_else(|| anyhow!("undo ticket does not exist or was already used"))?;
        let ticket = self.undo_tickets[ticket_position].clone();
        validate_ticket(&ticket, confirmation_token, current_provider, now)?;
        let record_index = self
            .entries
            .iter()
            .position(|record| record.id == ticket.record_id)
            .ok_or_else(|| anyhow!("the operation record for this undo ticket is unavailable"))?;
        if self.entries[record_index].undone_by.is_some() {
            bail!("this switch was already undone");
        }
        let reason = format!(
            "undo switch: restore {} to {}",
            ticket.switched_to, ticket.restore_provider
        );
        let mut record = self.make_record(
            OperationKind::UndoSwitch,
            ticket.protocol,
            Some(&ticket.switched_to),
            Some(&ticket.restore_provider),
            require_clean(&reason)?,
            OperationOutcome::Succeeded,
        );
        record.undo_ticket_id = Some(ticket.id.clone());
        self.entries[record_index].undone_by = Some(record.id.clone());
        self.undo_tickets.remove(ticket_position);
        let stored = record.clone();
        self.push_record(record);
        Ok(stored)
    }

    pub fn prune(&mut self) {
        let now = self.now();
        self.undo_tickets.retain(|ticket| ticket.expires_at > now);
        self.trim_entries();
        self.remove_orphaned_tickets();
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let now = self.now();
        let file = HistoryFile {
            schema_version: HISTORY_SCHEMA_VERSION,
            next_seq: self.next_seq,
            entries: self.entries.clone(),
            undo_tickets: self
                .undo_tickets
                .iter()
                .filter(|ticket| ticket.expires_at > now)
                .cloned()
                .collect(),
        };
        atomic_write(path, &serde_json::to_vec_pretty(&file)?)
    }

    pub fn load(path: &Path) -> Result<LoadOutcome> {
        Self::load_with(path, DEFAULT_MAX_ENTRIES, Clock::real())
    }

    pub fn load_with(path: &Path, max_entries: usize, clock: Clock) -> Result<LoadOutcome> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(LoadOutcome {
                    history: Self::with_clock_and_limits(clock, max_entries, DEFAULT_UNDO_LIFETIME),
                    quarantined: None,
                });
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to read operation history: {}", path.display())
                });
            }
        };
        let file = match serde_json::from_slice::<HistoryFile>(&bytes) {
            Ok(file) if file.schema_version <= HISTORY_SCHEMA_VERSION => file,
            _ => {
                return Ok(LoadOutcome {
                    history: Self::with_clock_and_limits(clock, max_entries, DEFAULT_UNDO_LIFETIME),
                    quarantined: quarantine_corrupt(path),
                });
            }
        };

        let now = clock.now();
        let mut history = Self::with_clock_and_limits(clock, max_entries, DEFAULT_UNDO_LIFETIME);
        history.next_seq = file.next_seq;
        for mut record in file.entries {
            record.reason = sanitize_reason(&record.reason);
            record.from_provider = record
                .from_provider
                .as_deref()
                .map(sanitize_identifier_for_load);
            record.to_provider = record
                .to_provider
                .as_deref()
                .map(sanitize_identifier_for_load);
            history.entries.push(record);
        }
        history.trim_entries();
        history.undo_tickets = file
            .undo_tickets
            .into_iter()
            .filter(|ticket| ticket.expires_at > now)
            .filter(|ticket| {
                history
                    .entries
                    .iter()
                    .any(|record| record.id == ticket.record_id)
            })
            .collect();
        history.next_seq = history.next_seq.max(history.entries.len() as u64);
        Ok(LoadOutcome {
            history,
            quarantined: None,
        })
    }

    fn make_record(
        &mut self,
        kind: OperationKind,
        protocol: Protocol,
        from: Option<&str>,
        to: Option<&str>,
        reason: String,
        outcome: OperationOutcome,
    ) -> OperationRecord {
        OperationRecord {
            id: self.next_id("op"),
            occurred_at: self.now(),
            kind,
            protocol,
            from_provider: from.map(str::to_owned),
            to_provider: to.map(str::to_owned),
            reason,
            outcome,
            undo_ticket_id: None,
            undone_by: None,
        }
    }

    fn next_id(&mut self, prefix: &str) -> String {
        let id = format!(
            "{prefix}-{}-{:05}",
            self.now().timestamp_millis(),
            self.next_seq
        );
        self.next_seq += 1;
        id
    }

    fn issue_grant(
        &mut self,
        record_id: String,
        protocol: Protocol,
        switched_to: &str,
        restore_provider: &str,
        record: &mut OperationRecord,
    ) -> UndoGrant {
        let token = generate_confirmation_token();
        let now = self.now();
        let ticket = UndoTicket {
            id: self.next_id("ticket"),
            record_id,
            protocol,
            switched_to: switched_to.to_owned(),
            restore_provider: restore_provider.to_owned(),
            created_at: now,
            expires_at: now + self.undo_lifetime,
            confirmation_hash: hash_token(&token),
        };
        record.undo_ticket_id = Some(ticket.id.clone());
        self.undo_tickets.push(ticket.clone());
        UndoGrant {
            ticket,
            confirmation_token: token,
        }
    }

    fn push_record(&mut self, record: OperationRecord) {
        self.entries.push(record);
        self.trim_entries();
        self.remove_orphaned_tickets();
    }

    fn trim_entries(&mut self) {
        let overflow = self.entries.len().saturating_sub(self.max_entries);
        if overflow > 0 {
            self.entries.drain(..overflow);
        }
    }

    fn remove_orphaned_tickets(&mut self) {
        self.undo_tickets.retain(|ticket| {
            self.entries
                .iter()
                .any(|record| record.id == ticket.record_id)
        });
    }
}

impl Default for OperationHistory {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for OperationHistory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OperationHistory")
            .field("entries", &self.entries)
            .field("undo_tickets", &self.undo_tickets)
            .field("max_entries", &self.max_entries)
            .field("undo_lifetime", &self.undo_lifetime)
            .field("next_seq", &self.next_seq)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub struct LoadOutcome {
    pub history: OperationHistory,
    pub quarantined: Option<PathBuf>,
}

#[derive(Serialize, Deserialize)]
struct HistoryFile {
    #[serde(default)]
    schema_version: u32,
    #[serde(default)]
    next_seq: u64,
    #[serde(default)]
    entries: Vec<OperationRecord>,
    #[serde(default)]
    undo_tickets: Vec<UndoTicket>,
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension(format!(
        "{}.{}.{}.headroom-route.tmp",
        path.extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("tmp"),
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default()
    ));
    let result = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        replace_file(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn replace_file(temp: &Path, path: &Path) -> Result<()> {
    #[cfg(windows)]
    if path.exists() {
        let temp_wide: Vec<u16> = temp.as_os_str().encode_wide().chain(Some(0)).collect();
        let path_wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let replaced = unsafe {
            ReplaceFileW(
                path_wide.as_ptr(),
                temp_wide.as_ptr(),
                std::ptr::null(),
                REPLACEFILE_WRITE_THROUGH,
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        if replaced != 0 {
            return Ok(());
        }
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("failed to replace operation history: {}", path.display()));
    }
    fs::rename(temp, path)
        .with_context(|| format!("failed to replace operation history: {}", path.display()))
}

fn quarantine_corrupt(path: &Path) -> Option<PathBuf> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let name = path.file_name()?.to_str()?;
    let quarantined = path.with_file_name(format!("{name}.corrupt-{stamp}"));
    fs::rename(path, &quarantined).ok().map(|_| quarantined)
}

pub fn sanitize_reason(raw: &str) -> String {
    redact_tokens(raw)
}

fn redact_tokens(raw: &str) -> String {
    let mut redact_next = false;
    raw.split_inclusive(char::is_whitespace)
        .map(|part| {
            let trimmed = part.trim_end_matches(char::is_whitespace);
            let suffix = &part[trimmed.len()..];
            let unquoted = trimmed.trim_matches(&['"', '\\', '`'][..]);
            let lower = unquoted.to_ascii_lowercase();
            let separator = lower.ends_with([':', '=']);
            let key_name = if separator {
                lower.trim_end_matches([':', '='])
            } else {
                lower.as_str()
            };
            let key_is_secret = is_secret_key(key_name);
            let obvious_secret = contains_obvious_secret(trimmed);
            let marker = SECRET_VALUE_MARKERS
                .iter()
                .any(|marker| lower.contains(marker));
            let should_redact = redact_next || key_is_secret || obvious_secret || marker;
            redact_next = lower == "bearer"
                || lower.ends_with("authorization:")
                || key_is_secret
                || (redact_next && matches!(lower.as_str(), "=" | ":"));
            if should_redact {
                format!("{REDACTED}{suffix}")
            } else if let Some(url) = redact_url_userinfo(trimmed) {
                format!("{url}{suffix}")
            } else {
                part.to_owned()
            }
        })
        .collect()
}

pub fn require_clean(raw: &str) -> Result<String> {
    let cleaned = sanitize_reason(raw);
    let lower = cleaned.to_ascii_lowercase();
    if lower.contains("bearer")
        || lower.contains("authorization")
        || lower.contains("password")
        || lower.contains("secret")
        || lower.contains("api_key")
        || lower.contains("apikey")
        || lower.contains("eyjhb")
    {
        bail!("operation reason contains sensitive material");
    }
    Ok(cleaned)
}

pub fn ensure_identifier_clean(provider: &str) -> Result<()> {
    if contains_obvious_secret(provider)
        || looks_like_jwt(provider)
        || is_secret_key(provider.trim_matches(&['"', '\\', '`'][..]))
    {
        bail!("Provider identifier contains sensitive material");
    }
    Ok(())
}

fn sanitize_identifier_for_load(value: &str) -> String {
    if contains_obvious_secret(value) || looks_like_jwt(value) {
        REDACTED.to_owned()
    } else {
        value.to_owned()
    }
}

fn contains_obvious_secret(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    (lower.contains("sk-") || lower.contains("sk_")) && value.len() >= 12
}

fn looks_like_jwt(value: &str) -> bool {
    let value = value.trim_matches(&['"', '\\', '`'][..]);
    value.starts_with("eyJ") && value.split('.').count() == 3
}

fn redact_url_userinfo(part: &str) -> Option<String> {
    let scheme_end = part.find("://")?;
    let authority_start = scheme_end + 3;
    let authority_end = part[authority_start..]
        .find(['/', '?', '#'])
        .map(|index| authority_start + index)
        .unwrap_or(part.len());
    let at = part[authority_start..authority_end].find('@')?;
    let host_start = authority_start + at + 1;
    Some(format!(
        "{}[REDACTED]@{}",
        &part[..authority_start],
        &part[host_start..]
    ))
}

fn is_secret_key(key: &str) -> bool {
    let normalized = key
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect::<String>();
    normalized.contains("apikey")
        || normalized.contains("authtoken")
        || normalized.contains("accesstoken")
        || normalized.contains("refreshtoken")
        || normalized.contains("clientsecret")
        || normalized.contains("controltoken")
        || normalized.contains("sessiontoken")
        || normalized.contains("authorization")
        || normalized.contains("credential")
        || normalized.contains("password")
        || normalized.contains("privatekey")
        || normalized == "token"
        || normalized == "secret"
        || normalized == "cookie"
}

const SECRET_VALUE_MARKERS: &[&str] = &[
    "api_key=",
    "apikey=",
    "auth_token=",
    "access_token=",
    "refresh_token=",
    "client_secret=",
    "control_token=",
    "session_token=",
    "authorization:",
    "password=",
    "secret=",
    "api_key:",
    "client_secret:",
    "control_token:",
    "session_token:",
    "auth_token:",
    "access_token:",
    "refresh_token:",
    "password:",
    "secret:",
];

pub fn generate_confirmation_token() -> String {
    rand::rng()
        .sample_iter(Alphanumeric)
        .take(12)
        .map(char::from)
        .collect::<String>()
        .to_ascii_uppercase()
}

fn hash_token(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
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

fn validate_ticket(
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

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value)
            .expect("valid timestamp")
            .with_timezone(&Utc)
    }

    fn temp_path(label: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "headroom-history-{label}-{}-{stamp}.json",
            std::process::id()
        ))
    }

    #[test]
    fn sanitizes_secret_values_and_url_credentials() {
        assert_eq!(
            sanitize_reason("failure Bearer sk-live-0123456789"),
            "failure Bearer [REDACTED]"
        );
        assert_eq!(
            sanitize_reason("api_key = sk-abc123def456gh789"),
            "[REDACTED] [REDACTED] [REDACTED]"
        );
        assert_eq!(
            sanitize_reason("authorization: abcdef0123456789"),
            "[REDACTED] [REDACTED]"
        );
        assert_eq!(
            sanitize_reason("base https://user:pass@example.com/v1"),
            "base https://[REDACTED]@example.com/v1"
        );
        assert_eq!(sanitize_reason("token_count: 12"), "token_count: 12");
    }

    #[test]
    fn require_clean_rejects_unredacted_jwt_and_bearer_context() {
        assert!(require_clean("Bearer eyJhbGciOiJIUzI1NiJ9.x.y").is_err());
        assert!(require_clean("Bearer sk-live-abcdef0123456789").is_err());
        assert!(require_clean("normal failover reason").is_ok());
    }

    #[test]
    fn history_is_bounded_and_undo_marks_original() {
        let (clock, _) = Clock::manual(fixed("2026-01-01T00:00:00Z"));
        let mut history = OperationHistory::with_clock_and_limits(clock, 2, Duration::minutes(30));
        history
            .record_manual_switch(Protocol::OpenAi, None, "one", "manual")
            .unwrap();
        let (switch, grant) = history
            .record_auto_failover(Protocol::OpenAi, "one", "two", "failover")
            .unwrap();
        let undo = history
            .record_undo(&grant.ticket.id, &grant.confirmation_token, "two")
            .unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history.tickets().len(), 0);
        assert_eq!(
            undo.undo_ticket_id.as_deref(),
            Some(grant.ticket.id.as_str())
        );
        assert_eq!(
            history
                .entries()
                .iter()
                .find(|entry| entry.id == switch.id)
                .and_then(|entry| entry.undone_by.as_deref()),
            Some(undo.id.as_str())
        );
    }

    #[test]
    fn save_load_and_corrupt_quarantine_are_atomic() {
        let path = temp_path("persist");
        let (clock, _) = Clock::manual(fixed("2026-01-01T00:00:00Z"));
        let mut history = OperationHistory::with_clock(clock.clone());
        history
            .record_manual_switch(Protocol::Anthropic, None, "claude", "manual")
            .unwrap();
        history.save(&path).unwrap();
        let loaded = OperationHistory::load_with(&path, 200, clock).unwrap();
        assert_eq!(loaded.history.len(), 1);
        fs::write(&path, b"not json").unwrap();
        let quarantined = OperationHistory::load(&path).unwrap();
        assert_eq!(quarantined.history.len(), 0);
        assert!(quarantined.quarantined.is_some());
        let _ = fs::remove_file(&path);
        if let Some(path) = quarantined.quarantined {
            let _ = fs::remove_file(path);
        }
    }

    #[test]
    fn cooldown_rebuilds_from_recent_failover() {
        let (clock, time) = Clock::manual(fixed("2026-01-01T00:00:00Z"));
        let mut history =
            OperationHistory::with_clock_and_limits(clock, 200, Duration::minutes(30));
        history
            .record_auto_failover(Protocol::OpenAi, "old", "new", "failover")
            .unwrap();
        time.advance(Duration::minutes(2));
        let cooldown = SwitchCooldown::from_history(&history, Duration::minutes(5));
        assert!(!cooldown.protocol_allowed(Protocol::OpenAi));
        assert!(!cooldown.provider_allowed(Protocol::OpenAi, "old"));
    }

    #[test]
    fn undo_grant_debug_redacts_token() {
        let (clock, _) = Clock::manual(fixed("2026-01-01T00:00:00Z"));
        let mut history = OperationHistory::with_clock(clock);
        let (_, grant) = history
            .record_auto_failover(Protocol::OpenAi, "old", "new", "failover")
            .unwrap();
        let debug = format!("{grant:?}");
        assert!(!debug.contains(&grant.confirmation_token));
        assert!(debug.contains(REDACTED));
    }
}
