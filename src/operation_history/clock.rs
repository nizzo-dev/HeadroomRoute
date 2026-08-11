use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, Utc};

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
