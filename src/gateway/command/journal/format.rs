//! Compact, versioned command records. Existing JSON records remain readable.
use super::Stored;
use crate::gateway::command::{Failure, MAX_COMMAND_ID_BYTES, Outcome, SetPower};
use crate::gateway::device::DeviceId;
use anyhow::{Context, bail, ensure};

const VERSION: u8 = 2;
const HEADER_BYTES: usize = 20;
const ON: u8 = 1 << 0;
const DELIVERED: u8 = 1 << 1;
const HAS_EXPIRY: u8 = 1 << 2;

impl Stored {
    pub(super) fn encode(&self) -> anyhow::Result<Vec<u8>> {
        let id = self.command.command_id.as_bytes();
        ensure!(
            !id.is_empty() && id.len() <= MAX_COMMAND_ID_BYTES,
            "invalid command ID length"
        );
        let mut flags = 0;
        if self.command.on {
            flags |= ON;
        }
        if self.delivered {
            flags |= DELIVERED;
        }
        if self.command.expires_at_ms.is_some() {
            flags |= HAS_EXPIRY;
        }

        let mut bytes = Vec::with_capacity(HEADER_BYTES + id.len());
        bytes.extend_from_slice(&[
            VERSION,
            flags,
            self.command.endpoint,
            encode_outcome(self.outcome),
        ]);
        bytes.extend_from_slice(&self.command.device.ieee());
        bytes.extend_from_slice(&self.command.expires_at_ms.unwrap_or_default().to_le_bytes());
        bytes.extend_from_slice(id);
        Ok(bytes)
    }

    pub(super) fn decode(bytes: &[u8]) -> anyhow::Result<Self> {
        if bytes.first() == Some(&b'{') {
            return Ok(serde_json::from_slice(bytes)?);
        }
        ensure!(
            (HEADER_BYTES + 1..=HEADER_BYTES + MAX_COMMAND_ID_BYTES).contains(&bytes.len())
                && bytes[0] == VERSION,
            "invalid command record version or length"
        );

        let flags = bytes[1];
        ensure!(
            flags & !(ON | DELIVERED | HAS_EXPIRY) == 0,
            "invalid command flags"
        );
        let endpoint = bytes[2];
        ensure!((1..=240).contains(&endpoint), "invalid command endpoint");
        let outcome = decode_outcome(bytes[3])?;
        let device =
            DeviceId::from_ieee(bytes[4..12].try_into()?).context("invalid command device ID")?;
        let expiry = u64::from_le_bytes(bytes[12..20].try_into()?);
        let command_id = std::str::from_utf8(&bytes[HEADER_BYTES..])?.to_owned();
        Ok(Self {
            command: SetPower {
                command_id,
                device,
                endpoint,
                on: flags & ON != 0,
                expires_at_ms: (flags & HAS_EXPIRY != 0).then_some(expiry),
            },
            outcome,
            delivered: flags & DELIVERED != 0,
        })
    }
}

fn encode_outcome(outcome: Option<Outcome>) -> u8 {
    match outcome {
        None => 0,
        Some(Outcome::Accepted) => 1,
        Some(Outcome::Succeeded) => 2,
        Some(Outcome::Expired) => 3,
        Some(Outcome::Failed(Failure::ClockUnsynced)) => 4,
        Some(Outcome::Failed(Failure::DeviceOffline)) => 5,
        Some(Outcome::Failed(Failure::UnsupportedAction)) => 6,
        Some(Outcome::Failed(Failure::DeviceTimeout)) => 7,
        Some(Outcome::Failed(Failure::ExecutionUncertain)) => 8,
        Some(Outcome::Failed(Failure::Rejected)) => 9,
    }
}

fn decode_outcome(code: u8) -> anyhow::Result<Option<Outcome>> {
    Ok(match code {
        0 => None,
        1 => Some(Outcome::Accepted),
        2 => Some(Outcome::Succeeded),
        3 => Some(Outcome::Expired),
        4 => Some(Outcome::Failed(Failure::ClockUnsynced)),
        5 => Some(Outcome::Failed(Failure::DeviceOffline)),
        6 => Some(Outcome::Failed(Failure::UnsupportedAction)),
        7 => Some(Outcome::Failed(Failure::DeviceTimeout)),
        8 => Some(Outcome::Failed(Failure::ExecutionUncertain)),
        9 => Some(Outcome::Failed(Failure::Rejected)),
        _ => bail!("invalid command outcome {code}"),
    })
}
