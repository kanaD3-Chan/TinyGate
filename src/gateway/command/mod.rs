//! Transport-independent set_power ingress and durable result outbox.
mod journal;

use crate::gateway::device::DeviceId;
use embassy_time::{Duration, Instant};
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use serde::{Deserialize, Serialize};
use std::sync::{Mutex, OnceLock};

use journal::Journal;

const MAX_TTL_MS: u64 = 30_000;
static JOURNAL: OnceLock<Mutex<Journal>> = OnceLock::new();
static NETWORK_TIME: Mutex<Option<(u64, Instant)>> = Mutex::new(None);

/// Call after a trusted clock synchronization. Clear on loss of clock validity.
pub fn set_network_time(epoch_ms: Option<u64>) {
    *NETWORK_TIME.lock().unwrap() = epoch_ms.map(|ms| (ms, Instant::now()));
}

pub(crate) fn utc_now() -> Option<u64> {
    NETWORK_TIME
        .lock()
        .unwrap()
        .map(|(ms, instant)| ms.saturating_add(instant.elapsed().as_millis()))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetPower {
    pub command_id: String,
    pub device: DeviceId,
    pub endpoint: u8,
    pub on: bool,
    /// Backend-assigned expiry; local callers must pass None.
    pub expires_at_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Failure {
    ClockUnsynced,
    DeviceOffline,
    UnsupportedAction,
    DeviceTimeout,
    ExecutionUncertain,
    Rejected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outcome {
    Accepted,
    Succeeded,
    Failed(Failure),
    Expired,
}

impl Outcome {
    pub(super) fn terminal(self) -> bool {
        self != Self::Accepted
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResultEvent {
    pub command: SetPower,
    pub outcome: Outcome,
}

#[derive(Debug)]
pub enum SubmitError {
    Invalid,
    Conflict,
    Full,
    StorageUnavailable,
}

pub(crate) struct Work {
    pub command: SetPower,
    pub local_deadline: Option<Instant>,
    pub recovery: bool,
    pub was_accepted: bool,
}

impl Work {
    pub fn deadline(&self) -> Result<Instant, Outcome> {
        if let Some(expires) = self.command.expires_at_ms {
            let now = utc_now().ok_or(Outcome::Failed(Failure::ClockUnsynced))?;
            let left = expires
                .checked_sub(now)
                .filter(|left| *left > 0)
                .ok_or(Outcome::Expired)?;
            Ok(Instant::now() + Duration::from_millis(left.min(MAX_TTL_MS)))
        } else {
            self.local_deadline
                .ok_or(Outcome::Failed(Failure::ExecutionUncertain))
        }
    }
}

pub(crate) fn init(partition: EspDefaultNvsPartition) -> anyhow::Result<()> {
    let journal = Journal::open(partition)?;
    JOURNAL
        .set(Mutex::new(journal))
        .map_err(|_| anyhow::anyhow!("command journal already initialized"))
}

/// Queue a backend command. An accepted or final outcome is emitted through the outbox.
pub fn submit(command: SetPower) -> Result<(), SubmitError> {
    if command.expires_at_ms.is_none() {
        return Err(SubmitError::Invalid);
    }
    enqueue(command, None)
}

/// Local entry point for a controller or hardware bring-up. Does not require SNTP.
pub fn submit_local(command: SetPower, timeout_ms: u64) -> Result<(), SubmitError> {
    if command.expires_at_ms.is_some() || !(100..=MAX_TTL_MS).contains(&timeout_ms) {
        return Err(SubmitError::Invalid);
    }
    enqueue(
        command,
        Some(Instant::now() + Duration::from_millis(timeout_ms)),
    )
}

fn enqueue(command: SetPower, local_deadline: Option<Instant>) -> Result<(), SubmitError> {
    if command.command_id.is_empty()
        || command.command_id.len() > 128
        || !(1..=240).contains(&command.endpoint)
    {
        return Err(SubmitError::Invalid);
    }
    let journal = JOURNAL.get().ok_or(SubmitError::StorageUnavailable)?;
    journal.lock().unwrap().enqueue(command, local_deadline)
}

/// Undelivered outcomes survive restart. A later terminal outcome can replace Accepted.
pub fn pending_results() -> Vec<ResultEvent> {
    JOURNAL.get().map_or_else(Vec::new, |journal| {
        journal.lock().unwrap().pending_results()
    })
}

/// Call only after delivery. If the outcome changed in the meantime, it stays pending.
pub fn acknowledge(result: &ResultEvent) -> anyhow::Result<()> {
    let journal = JOURNAL
        .get()
        .ok_or_else(|| anyhow::anyhow!("command journal unavailable"))?;
    journal.lock().unwrap().acknowledge(result)
}

pub(crate) fn next_work(recovery: bool) -> Option<Work> {
    JOURNAL.get()?.lock().unwrap().next_work(recovery)
}

pub(crate) fn record(command_id: &str, outcome: Outcome) -> anyhow::Result<()> {
    let journal = JOURNAL
        .get()
        .ok_or_else(|| anyhow::anyhow!("command journal unavailable"))?;
    journal.lock().unwrap().record(command_id, outcome)
}
