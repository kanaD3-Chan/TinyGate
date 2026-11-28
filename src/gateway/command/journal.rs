mod format;

use super::{Outcome, ResultEvent, SetPower, SubmitError, Work, utc_now};
use embassy_time::{Duration, Instant};
use esp_idf_svc::nvs::{EspDefaultNvs, EspDefaultNvsPartition};
use serde::{Deserialize, Serialize};

// At most 148 bytes per new record. Budget seven 32-byte NVS entries per
// record, leaving space in the 24 KiB partition for GC and other namespaces.
const SLOTS: usize = 64;
const REMOTE_RETENTION_MS: u64 = 600_000;
const LOCAL_RETENTION: Duration = Duration::from_secs(60);

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
    entries: Box<[Option<Entry>]>,
}

impl Journal {
    pub fn open(partition: EspDefaultNvsPartition) -> anyhow::Result<Self> {
        let nvs = EspDefaultNvs::new(partition, "tg_cmd_v1", true)?;
        let mut entries: Vec<Option<Entry>> = (0..SLOTS).map(|_| None).collect();
        let local_floor = Instant::now() + LOCAL_RETENTION;
        for (index, entry) in entries.iter_mut().enumerate() {
            let key = key(index);
            if let Some(len) = nvs.blob_len(&key)? {
                anyhow::ensure!(len <= 1024, "command journal record too large");
                let mut bytes = vec![0; len];
                let value = nvs
                    .get_blob(&key, &mut bytes)?
                    .ok_or_else(|| anyhow::anyhow!("command journal record disappeared"))?;
                *entry = Some(Entry {
                    stored: Stored::decode(value)?,
                    local_deadline: None,
                    reclaim_local_after: local_floor,
                    recovery: true,
                });
            }
        }
        Ok(Self {
            nvs,
            entries: entries.into_boxed_slice(),
        })
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
            .position(|entry| {
                entry
                    .as_ref()
                    .is_some_and(|entry| entry.reclaimable(now_utc, now))
            })
            .or_else(|| self.entries.iter().position(Option::is_none))
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
            reclaim_local_after: local_deadline.unwrap_or(now) + LOCAL_RETENTION,
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
            Some(expires) => {
                now_utc.is_some_and(|now| now >= expires.saturating_add(REMOTE_RETENTION_MS))
            }
            None => now >= self.reclaim_local_after,
        }
    }
}

fn key(index: usize) -> String {
    format!("cmd{index:02}")
}

fn persist(nvs: &EspDefaultNvs, index: usize, stored: &Stored) -> anyhow::Result<()> {
    write_blob(nvs, &key(index), &stored.encode()?)
}

// EspNvs::set_blob erases the old key first. ESP-IDF's same-key update
// writes a new blob version before retiring the old one.
#[cfg(target_os = "espidf")]
fn write_blob(nvs: &EspDefaultNvs, key: &str, bytes: &[u8]) -> anyhow::Result<()> {
    use esp_idf_svc::handle::RawHandle;
    use esp_idf_svc::sys::{self, EspError};

    let key = std::ffi::CString::new(key)?;
    // The NVS handle, key and bytes remain valid throughout both calls.
    unsafe {
        EspError::convert(sys::nvs_set_blob(
            nvs.handle(),
            key.as_ptr(),
            bytes.as_ptr().cast(),
            bytes.len(),
        ))?;
        EspError::convert(sys::nvs_commit(nvs.handle()))?;
    }
    Ok(())
}

#[cfg(not(target_os = "espidf"))]
fn write_blob(nvs: &EspDefaultNvs, key: &str, bytes: &[u8]) -> anyhow::Result<()> {
    nvs.set_blob(key, bytes)?;
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

    #[test]
    fn reads_a_legacy_record_and_rewrites_it_with_the_size_bound() {
        let partition = EspDefaultNvsPartition::take().unwrap();
        let nvs = EspDefaultNvs::new(partition.clone(), "tg_cmd_v1", true).unwrap();
        let mut legacy = Stored {
            command: command(),
            outcome: Some(Outcome::Accepted),
            delivered: false,
        };
        legacy.command.command_id = "x".repeat(crate::gateway::command::MAX_COMMAND_ID_BYTES);
        nvs.set_blob("cmd00", &serde_json::to_vec(&legacy).unwrap())
            .unwrap();
        let mut journal = Journal::open(partition.clone()).unwrap();
        assert!(journal.next_work(true).unwrap().was_accepted);
        journal
            .record(&legacy.command.command_id, Outcome::Succeeded)
            .unwrap();
        assert_eq!(nvs.blob_len("cmd00").unwrap(), Some(148));
        let reopened = Journal::open(partition).unwrap();
        assert_eq!(reopened.pending_results()[0].outcome, Outcome::Succeeded);
    }

    #[test]
    fn under_capacity_pressure_only_acknowledged_expired_records_are_reused() {
        let partition = EspDefaultNvsPartition::take().unwrap();
        let mut journal = Journal::open(partition.clone()).unwrap();
        let now = Instant::now();
        for index in 0..SLOTS {
            let mut record = command();
            record.command_id = format!("burst-{index}");
            journal.enqueue(record, Some(now)).unwrap();
        }
        let next = command();
        assert!(matches!(
            journal.enqueue(next.clone(), Some(now)),
            Err(SubmitError::Full)
        ));

        let id = "burst-0";
        journal.record(id, Outcome::Succeeded).unwrap();
        let index = journal.find(id).unwrap();
        let entry = journal.entries[index].as_mut().unwrap();
        entry.reclaim_local_after = now;
        assert!(matches!(
            journal.enqueue(next.clone(), Some(now)),
            Err(SubmitError::Full)
        ));

        let ack = journal
            .pending_results()
            .into_iter()
            .find(|result| result.command.command_id == id)
            .unwrap();
        journal.acknowledge(&ack).unwrap();
        journal.enqueue(next.clone(), Some(now)).unwrap();
        let reopened = Journal::open(partition).unwrap();
        assert!(reopened.find(id).is_none());
        assert!(reopened.find("burst-1").is_some());
        assert!(reopened.find(&next.command_id).is_some());
    }

    #[test]
    fn cloud_records_need_both_delivery_and_the_contract_retention_window() {
        let expires = 20_000;
        let now = Instant::now();
        let mut entry = Entry {
            stored: Stored {
                command: SetPower {
                    expires_at_ms: Some(expires),
                    ..command()
                },
                outcome: Some(Outcome::Succeeded),
                delivered: true,
            },
            local_deadline: None,
            reclaim_local_after: now,
            recovery: false,
        };
        assert!(!entry.reclaimable(None, now));
        assert!(!entry.reclaimable(Some(expires + REMOTE_RETENTION_MS - 1), now));
        assert!(entry.reclaimable(Some(expires + REMOTE_RETENTION_MS), now));
        entry.stored.delivered = false;
        assert!(!entry.reclaimable(Some(expires + REMOTE_RETENTION_MS), now));
    }
}
