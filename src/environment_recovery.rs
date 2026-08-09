//! Pure recovery state machine for environment changes and crashed sessions.

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::{REPLACEFILE_WRITE_THROUGH, ReplaceFileW};

pub const SESSION_SCHEMA_VERSION: u32 = 1;

pub type Tick = u64;

pub trait Clock: Send + Sync {
    fn now(&self) -> Tick;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Tick {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as Tick
    }
}

#[derive(Clone, Debug)]
pub struct TestClock {
    tick: Tick,
}

impl TestClock {
    pub fn new(tick: Tick) -> Self {
        Self { tick }
    }

    pub fn set(&mut self, tick: Tick) {
        self.tick = tick;
    }

    pub fn advance(&mut self, amount: u64) {
        self.tick = self.tick.saturating_add(amount);
    }
}

impl Clock for TestClock {
    fn now(&self) -> Tick {
        self.tick
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentEvent {
    Resume,
    NetworkOrProxyChanged,
    PortConflict,
    PreviousSessionUnclean,
    RecoverySucceeded,
    RecoveryFailed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMarker {
    pub version: u32,
    pub session_id: String,
    pub session_start: DateTime<Utc>,
    pub last_heartbeat: Option<DateTime<Utc>>,
    pub is_healthy: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortOccupant {
    pub pid: u32,
    pub name: String,
    pub proc_name: String,
    pub is_headroom: bool,
}

/// Return a conservative classification when only a PID is known.
pub fn classify_port(pid: u32) -> Option<PortOccupant> {
    (pid != 0).then(|| PortOccupant {
        pid,
        name: format!("proc-{pid}"),
        proc_name: format!("proc-{pid}"),
        is_headroom: false,
    })
}

/// Classify a Windows port owner using the process name and known Headroom PID.
pub fn classify_port_with_process(
    pid: u32,
    process_name: impl Into<String>,
    headroom_pid: Option<u32>,
) -> Option<PortOccupant> {
    if pid == 0 {
        return None;
    }
    let process_name = process_name.into();
    let is_headroom = headroom_pid == Some(pid)
        || process_name.eq_ignore_ascii_case("headroom")
        || process_name.eq_ignore_ascii_case("headroom.exe");
    Some(PortOccupant {
        pid,
        name: process_name.clone(),
        proc_name: process_name,
        is_headroom,
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryAction {
    Probe,
    SyncConfig,
    RestartHeadroom,
    TakeOverPort,
    KeepExisting,
    StopStart,
    CleanSession,
    Notify,
    NoOp,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryState {
    Pending,
    InProgress,
    Succeeded,
    Failed,
    Paused,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RecoveryConfig {
    pub max_retries: u32,
    pub debounce_ms: u64,
    pub backoff_base_ms: u64,
    pub backoff_factor: f64,
}

impl Default for RecoveryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            debounce_ms: 500,
            backoff_base_ms: 500,
            backoff_factor: 2.0,
        }
    }
}

impl RecoveryConfig {
    pub fn validate(&self) -> Result<()> {
        if !self.backoff_factor.is_finite() || self.backoff_factor < 1.0 {
            bail!("recovery backoff_factor must be finite and at least 1");
        }
        Ok(())
    }
}

pub struct RecoveryEngine {
    state: RecoveryState,
    config: RecoveryConfig,
    clock: Arc<dyn Clock>,
    retry_count: u32,
    retry_at: Option<Tick>,
    last_event: Option<EnvironmentEvent>,
    last_event_tick: Option<Tick>,
}

impl RecoveryEngine {
    pub fn new(config: RecoveryConfig) -> Self {
        Self::with_shared_clock(config, Arc::new(SystemClock))
            .expect("default recovery configuration is valid")
    }

    pub fn with_clock<C>(config: RecoveryConfig, clock: C) -> Result<Self>
    where
        C: Clock + 'static,
    {
        Self::with_shared_clock(config, Arc::new(clock))
    }

    pub fn with_shared_clock(config: RecoveryConfig, clock: Arc<dyn Clock>) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            state: RecoveryState::Pending,
            config,
            clock,
            retry_count: 0,
            retry_at: None,
            last_event: None,
            last_event_tick: None,
        })
    }

    pub fn state(&self) -> RecoveryState {
        self.state.clone()
    }

    pub fn config(&self) -> &RecoveryConfig {
        &self.config
    }

    pub fn retry_count(&self) -> u32 {
        self.retry_count
    }

    pub fn retry_at(&self) -> Option<Tick> {
        self.retry_at
    }

    pub fn last_event(&self) -> Option<&EnvironmentEvent> {
        self.last_event.as_ref()
    }

    pub fn can_retry(&self) -> bool {
        self.retry_at
            .is_none_or(|retry_at| self.clock.now() >= retry_at)
    }

    pub fn retry_delay(&self) -> Duration {
        let exponent = self.retry_count.saturating_sub(1).min(31);
        let factor = self.config.backoff_factor.powi(exponent as i32);
        let millis = (self.config.backoff_base_ms as f64 * factor).min(u64::MAX as f64) as u64;
        Duration::from_millis(millis)
    }

    pub fn process(&mut self, event: EnvironmentEvent) -> RecoveryAction {
        let now = self.clock.now();
        if matches!(event, EnvironmentEvent::RecoverySucceeded) {
            self.last_event = Some(event);
            self.last_event_tick = Some(now);
            self.state = RecoveryState::Succeeded;
            self.retry_at = None;
            return RecoveryAction::Notify;
        }
        if matches!(event, EnvironmentEvent::RecoveryFailed) {
            self.last_event = Some(event);
            self.last_event_tick = Some(now);
            return self.process_failure(now);
        }
        if self.state == RecoveryState::Paused {
            self.last_event = Some(event);
            self.last_event_tick = Some(now);
            return RecoveryAction::Notify;
        }
        if self.is_debounced(&event, now) {
            return RecoveryAction::NoOp;
        }
        if self.state == RecoveryState::Failed && !self.can_retry() {
            return RecoveryAction::NoOp;
        }
        self.last_event = Some(event.clone());
        self.last_event_tick = Some(now);
        self.retry_at = None;
        self.state = RecoveryState::InProgress;
        action_for_event(&event)
    }

    /// Start a retry once the exponential backoff has elapsed.
    pub fn attempt_retry(&mut self) -> Result<()> {
        if self.state == RecoveryState::Paused {
            bail!("recovery is paused after reaching the retry limit");
        }
        if self.retry_count >= self.config.max_retries {
            self.state = RecoveryState::Paused;
            bail!("recovery retry limit reached");
        }
        if !self.can_retry() {
            bail!("recovery backoff has not elapsed");
        }
        self.state = RecoveryState::InProgress;
        self.retry_at = None;
        Ok(())
    }

    pub fn reset(&mut self) {
        self.state = RecoveryState::Pending;
        self.retry_count = 0;
        self.retry_at = None;
        self.last_event = None;
        self.last_event_tick = None;
    }

    fn process_failure(&mut self, now: Tick) -> RecoveryAction {
        self.retry_count = self.retry_count.saturating_add(1);
        if self.retry_count >= self.config.max_retries || self.config.max_retries == 0 {
            self.state = RecoveryState::Paused;
            self.retry_at = None;
            RecoveryAction::Notify
        } else {
            self.state = RecoveryState::Failed;
            self.retry_at = Some(now.saturating_add(self.retry_delay().as_millis() as Tick));
            RecoveryAction::Notify
        }
    }

    fn is_debounced(&self, event: &EnvironmentEvent, now: Tick) -> bool {
        self.config.debounce_ms > 0
            && self.last_event.as_ref() == Some(event)
            && self
                .last_event_tick
                .is_some_and(|last| now.saturating_sub(last) < self.config.debounce_ms)
    }
}

fn action_for_event(event: &EnvironmentEvent) -> RecoveryAction {
    match event {
        EnvironmentEvent::Resume => RecoveryAction::Probe,
        EnvironmentEvent::NetworkOrProxyChanged => RecoveryAction::SyncConfig,
        EnvironmentEvent::PortConflict => RecoveryAction::TakeOverPort,
        EnvironmentEvent::PreviousSessionUnclean => RecoveryAction::StopStart,
        EnvironmentEvent::RecoverySucceeded | EnvironmentEvent::RecoveryFailed => {
            RecoveryAction::NoOp
        }
    }
}

pub struct SessionStore {
    path: PathBuf,
}

impl SessionStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn save(&self, marker: SessionMarker) -> Result<()> {
        if marker.version != SESSION_SCHEMA_VERSION {
            bail!("unsupported session marker version {}", marker.version);
        }
        let bytes = serde_json::to_vec_pretty(&marker)?;
        atomic_write(&self.path, &bytes)
    }

    pub fn load(&self) -> Result<Option<SessionMarker>> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to read session marker: {}", self.path.display())
                });
            }
        };
        let marker: SessionMarker =
            serde_json::from_slice(&bytes).context("session marker is not valid JSON")?;
        if marker.version != SESSION_SCHEMA_VERSION {
            bail!("unsupported session marker version {}", marker.version);
        }
        Ok(Some(marker))
    }

    pub fn previous_session_unclean(&self) -> Result<bool> {
        Ok(self
            .load()?
            .is_some_and(|marker| !marker.session_id.is_empty() && !marker.is_healthy))
    }

    /// Create an unclean marker for the new session and report the old state.
    pub fn begin_session_with_status(&self) -> Result<bool> {
        let previous_unclean = self.previous_session_unclean()?;
        let now = Utc::now();
        self.save(SessionMarker {
            version: SESSION_SCHEMA_VERSION,
            session_id: next_session_id(),
            session_start: now,
            last_heartbeat: Some(now),
            is_healthy: false,
        })?;
        Ok(previous_unclean)
    }

    pub fn begin_session(&self) -> Result<()> {
        self.begin_session_with_status().map(|_| ())
    }

    pub fn heartbeat(&self) -> Result<()> {
        let mut marker = self
            .load()?
            .ok_or_else(|| anyhow!("session marker does not exist"))?;
        marker.last_heartbeat = Some(Utc::now());
        self.save(marker)
    }

    pub fn finish_session(&self) -> Result<()> {
        let Some(mut marker) = self.load()? else {
            return Ok(());
        };
        marker.last_heartbeat = Some(Utc::now());
        marker.is_healthy = true;
        self.save(marker)
    }
}

fn next_session_id() -> String {
    static SEQUENCE: OnceLock<AtomicU64> = OnceLock::new();
    let sequence = SEQUENCE
        .get_or_init(|| AtomicU64::new(0))
        .fetch_add(1, Ordering::Relaxed);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("session-{}-{sequence}", timestamp)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncPolicy {
    pub enabled: bool,
    pub max_retries: u32,
    pub base_delay_ms: u64,
}

impl SyncPolicy {
    pub fn new(max_retries: u32, base_delay_ms: u64) -> Self {
        Self {
            enabled: true,
            max_retries,
            base_delay_ms,
        }
    }

    pub fn should_sync(&self) -> bool {
        self.enabled
    }

    pub fn delay(&self, attempt: u32) -> Duration {
        let exponent = attempt.saturating_sub(1).min(63);
        let multiplier = 1u64.checked_shl(exponent).unwrap_or(u64::MAX);
        Duration::from_millis(self.base_delay_ms.saturating_mul(multiplier))
    }

    pub fn is_paused(&self) -> bool {
        self.max_retries == 0
    }

    pub fn is_paused_after(&self, attempts: u32) -> bool {
        self.max_retries == 0 || attempts >= self.max_retries
    }
}

pub fn plan_recovery(config: &RecoveryConfig) -> RecoveryAction {
    if config.debounce_ms > 0 {
        RecoveryAction::NoOp
    } else {
        RecoveryAction::Probe
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension(format!(
        "{}.{}.{}.session.tmp",
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
            .with_context(|| format!("failed to replace session marker: {}", path.display()));
    }
    fs::rename(temp, path)
        .with_context(|| format!("failed to replace session marker: {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> RecoveryConfig {
        RecoveryConfig {
            max_retries: 3,
            debounce_ms: 0,
            backoff_base_ms: 100,
            backoff_factor: 2.0,
        }
    }

    fn temp_path(label: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        std::env::temp_dir().join(format!("headroom-recovery-{label}-{stamp}.json"))
    }

    #[test]
    fn port_classification_is_conservative_without_process_context() {
        assert!(classify_port(0).is_none());
        let occupant = classify_port(1234).unwrap();
        assert_eq!(occupant.pid, 1234);
        assert!(!occupant.is_headroom);
        assert!(
            classify_port_with_process(1234, "Headroom.exe", None)
                .unwrap()
                .is_headroom
        );
    }

    #[test]
    fn recovery_engine_maps_events_and_success() {
        let clock = TestClock::new(0);
        let mut engine = RecoveryEngine::with_clock(config(), clock).unwrap();
        assert_eq!(engine.state(), RecoveryState::Pending);
        assert_eq!(
            engine.process(EnvironmentEvent::Resume),
            RecoveryAction::Probe
        );
        assert_eq!(engine.state(), RecoveryState::InProgress);
        assert_eq!(
            engine.process(EnvironmentEvent::RecoverySucceeded),
            RecoveryAction::Notify
        );
        assert_eq!(engine.state(), RecoveryState::Succeeded);
    }

    #[test]
    fn environment_events_map_to_recovery_actions() {
        let mut engine = RecoveryEngine::with_clock(config(), TestClock::new(0)).unwrap();
        assert_eq!(
            engine.process(EnvironmentEvent::NetworkOrProxyChanged),
            RecoveryAction::SyncConfig
        );
        engine.reset();
        assert_eq!(
            engine.process(EnvironmentEvent::PortConflict),
            RecoveryAction::TakeOverPort
        );
        engine.reset();
        assert_eq!(
            engine.process(EnvironmentEvent::PreviousSessionUnclean),
            RecoveryAction::StopStart
        );
    }

    #[test]
    fn debounce_and_backoff_prevent_hot_loops() {
        let clock = TestClock::new(0);
        let mut recovery_config = config();
        recovery_config.debounce_ms = 100;
        let mut engine = RecoveryEngine::with_clock(recovery_config, clock).unwrap();
        assert_eq!(
            engine.process(EnvironmentEvent::Resume),
            RecoveryAction::Probe
        );
        assert_eq!(
            engine.process(EnvironmentEvent::Resume),
            RecoveryAction::NoOp
        );
        assert_eq!(
            engine.process(EnvironmentEvent::RecoveryFailed),
            RecoveryAction::Notify
        );
        assert_eq!(engine.retry_count(), 1);
        assert_eq!(engine.retry_at(), Some(100));
        assert!(engine.attempt_retry().is_err());
    }

    #[test]
    fn retry_limit_pauses_recovery() {
        let mut recovery_config = config();
        recovery_config.max_retries = 2;
        let mut engine = RecoveryEngine::with_clock(recovery_config, TestClock::new(1000)).unwrap();
        engine.process(EnvironmentEvent::Resume);
        engine.process(EnvironmentEvent::RecoveryFailed);
        engine.process(EnvironmentEvent::RecoveryFailed);
        assert_eq!(engine.state(), RecoveryState::Paused);
    }

    #[test]
    fn session_store_detects_unclean_previous_session() {
        let path = temp_path("session");
        let store = SessionStore::new(path.clone());
        assert!(!store.previous_session_unclean().unwrap());
        assert!(!store.begin_session_with_status().unwrap());
        assert!(store.previous_session_unclean().unwrap());
        store.heartbeat().unwrap();
        store.finish_session().unwrap();
        assert!(!store.previous_session_unclean().unwrap());
        assert!(store.load().unwrap().unwrap().is_healthy);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn sync_policy_uses_exponential_delays() {
        let policy = SyncPolicy::new(3, 100);
        assert_eq!(policy.delay(1), Duration::from_millis(100));
        assert_eq!(policy.delay(4), Duration::from_millis(800));
        assert!(policy.is_paused_after(3));
        assert!(!policy.is_paused_after(2));
    }
}
