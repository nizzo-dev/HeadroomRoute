use super::{Clock, DEFAULT_COOLDOWN, OperationHistory, OperationKind, OperationOutcome};
use crate::model::Protocol;
use chrono::{DateTime, Duration, Utc};
use std::collections::HashMap;

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
