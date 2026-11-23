use super::{Outcome, ResultEvent, SetPower, SubmitError, Work, utc_now};
use embassy_time::{Duration, Instant};
use esp_idf_svc::nvs::{EspDefaultNvs, EspDefaultNvsPartition};
use serde::{Deserialize, Serialize};

const SLOTS: usize = 16;
const RETENTION_MS: u64 = 600_000;

#[derive(Clone, Serialize, Deserialize)]
struct Stored {
    command: SetPower,
    outcome: Option<Outcome>,
    delivered: bool,
}

struct Entry {
    stored: Stored,
    local_deadline: Option<Instant>,
    reclaim_local_after: Instant,
    recovery: bool,
}

pub(super) struct Journal {
    nvs: EspDefaultNvs,
    entries: [Option<Entry>; SLOTS],
}

impl Journal {
    pub fn open(partition: EspDefaultNvsPartition) -> anyhow::Result<Self> {
        let nvs = EspDefaultNvs::new(partition, "tg_cmd_v1", true)?;
        let mut entries = std::array::from_fn(|_| None);
        for (index, entry) in entries.iter_mut().enumerate() {
            let key = key(index);
            if let Some(len) = nvs.blob_len(&key)? {
                anyhow::ensure!(len <= 1024, "command journal record too large");
                let mut bytes = vec![0; len];
                let value = nvs
                    .get_blob(&key, &mut bytes)?
                    .ok_or_else(|| anyhow::anyhow!("command journal record disappeared"))?;
                *entry = Some(Entry {
                    stored: serde_json::from_slice(value)?,
                    local_deadline: None,
                    reclaim_local_after: Instant::now() + Duration::from_millis(RETENTION_MS),
                    recovery: true,
                });
            }
        }
        Ok(Self { nvs, entries })
    }

    pub fn enqueue(
        &mut self,
        command: SetPower,
        local_deadline: Option<Instant>,
    ) -> Result<(), SubmitError> {
        if let Some(index) = self.find(&command.command_id) {
            let entry = self.entries[index].as_ref().unwrap();
            if entry.stored.command != command {
                return Err(SubmitError::Conflict);
            }
            if entry.stored.delivered {
                let mut stored = entry.stored.clone();
                stored.delivered = false;
                self.write(index, stored)
                    .map_err(|_| SubmitError::StorageUnavailable)?;
            }
            return Ok(());
        }

        let now_utc = utc_now();
        let now = Instant::now();
        let slot = self
            .entries
            .iter()
            .position(|entry| entry.is_none())
            .or_else(|| {
                self.entries.iter().position(|entry| {
                    entry
                        .as_ref()
                        .is_some_and(|entry| entry.reclaimable(now_utc, now))
                })
            })
            .ok_or(SubmitError::Full)?;
        let stored = Stored {
            command,
            outcome: None,
            delivered: false,
        };
        persist(&self.nvs, slot, &stored).map_err(|_| SubmitError::StorageUnavailable)?;
        self.entries[slot] = Some(Entry {
            stored,
            local_deadline,
            reclaim_local_after: local_deadline.unwrap_or(now)
                + Duration::from_millis(RETENTION_MS),
            recovery: false,
        });
        Ok(())
    }

    pub fn next_work(&self, recovery: bool) -> Option<Work> {
        self.entries.iter().flatten().find_map(|entry| {
            if entry.recovery != recovery || entry.stored.outcome.is_some_and(Outcome::terminal) {
                return None;
            }
            Some(Work {
                command: entry.stored.command.clone(),
                local_deadline: entry.local_deadline,
                recovery,
                was_accepted: entry.stored.outcome == Some(Outcome::Accepted),
            })
        })
    }

    pub fn pending_results(&self) -> Vec<ResultEvent> {
        self.entries
            .iter()
            .flatten()
            .filter_map(|entry| {
                let stored = &entry.stored;
                if stored.delivered {
                    return None;
                }
                Some(ResultEvent {
                    command: stored.command.clone(),
                    outcome: stored.outcome?,
                })
            })
            .collect()
    }

    pub fn acknowledge(&mut self, result: &ResultEvent) -> anyhow::Result<()> {
        if let Some(index) = self.find(&result.command.command_id) {
            let mut stored = self.entries[index].as_ref().unwrap().stored.clone();
            if stored.command == result.command
                && stored.outcome == Some(result.outcome)
                && !stored.delivered
            {
                stored.delivered = true;
                self.write(index, stored)?;
            }
        }
        Ok(())
    }

    pub fn record(&mut self, id: &str, outcome: Outcome) -> anyhow::Result<()> {
        let index = self
            .find(id)
            .ok_or_else(|| anyhow::anyhow!("unknown command result"))?;
        let mut stored = self.entries[index].as_ref().unwrap().stored.clone();
        if stored.outcome == Some(outcome) || stored.outcome.is_some_and(Outcome::terminal) {
            return Ok(());
        }
        stored.outcome = Some(outcome);
        stored.delivered = false;
        self.write(index, stored)
    }

    fn find(&self, id: &str) -> Option<usize> {
        self.entries.iter().position(|entry| {
            entry
                .as_ref()
                .is_some_and(|entry| entry.stored.command.command_id == id)
        })
    }

    fn write(&mut self, index: usize, stored: Stored) -> anyhow::Result<()> {
        persist(&self.nvs, index, &stored)?;
        self.entries[index].as_mut().unwrap().stored = stored;
        Ok(())
    }
}

impl Entry {
    fn reclaimable(&self, now_utc: Option<u64>, now: Instant) -> bool {
        if !self.stored.delivered || !self.stored.outcome.is_some_and(Outcome::terminal) {
            return false;
        }
        match self.stored.command.expires_at_ms {
            Some(expires) => now_utc.is_some_and(|now| now >= expires.saturating_add(RETENTION_MS)),
            None => now >= self.reclaim_local_after,
        }
    }
}

fn key(index: usize) -> String {
    format!("cmd{index:02}")
}

fn persist(nvs: &EspDefaultNvs, index: usize, stored: &Stored) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec(stored)?;
    nvs.set_blob(&key(index), &bytes)?;
    Ok(())
}

#[cfg(all(test, not(target_os = "espidf")))]
mod tests {
    use super::*;
    use crate::gateway::command::Failure;
    use crate::gateway::device::DeviceId;

    fn command() -> SetPower {
        SetPower {
            command_id: "gateway-test".into(),
            device: DeviceId::from_ieee([1; 8]).unwrap(),
            endpoint: 2,
            on: true,
            expires_at_ms: None,
        }
    }

    #[test]
    fn recovers_inflight_records_and_keeps_unacknowledged_final_results() {
        let partition = EspDefaultNvsPartition::take().unwrap();
        let command = command();
        {
            let mut journal = Journal::open(partition.clone()).unwrap();
            journal
                .enqueue(command.clone(), Some(Instant::now()))
                .unwrap();
            journal
                .record(&command.command_id, Outcome::Accepted)
                .unwrap();
        }
        let terminal = Outcome::Failed(Failure::ExecutionUncertain);
        {
            let mut journal = Journal::open(partition.clone()).unwrap();
            assert!(journal.next_work(false).is_none());
            assert!(journal.next_work(true).unwrap().was_accepted);
            journal.record(&command.command_id, terminal).unwrap();
            journal
                .acknowledge(&ResultEvent {
                    command: command.clone(),
                    outcome: Outcome::Accepted,
                })
                .unwrap();
        }
        let mut journal = Journal::open(partition).unwrap();
        assert!(journal.next_work(true).is_none());
        let result = ResultEvent {
            command: command.clone(),
            outcome: terminal,
        };
        assert_eq!(journal.pending_results(), vec![result.clone()]);
        journal.acknowledge(&result).unwrap();
        assert!(journal.pending_results().is_empty());
        assert!(matches!(
            journal.enqueue(
                SetPower {
                    on: false,
                    ..command.clone()
                },
                None
            ),
            Err(SubmitError::Conflict)
        ));
        journal.enqueue(command, None).unwrap();
        assert_eq!(journal.pending_results(), vec![result]);
    }
}
