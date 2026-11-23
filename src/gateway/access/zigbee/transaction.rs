//! Correlate transport confirmations and protocol responses, in either arrival order.
use crate::drivers::radio::zigbee_event::{Attribute, Event, Target};
use std::collections::VecDeque;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Operation {
    Read(u16),
    Configure(u16),
    SetPower(bool),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Receipt {
    Sent,
    Read(Attribute),
    Configured,
    Rejected,
    TransmissionFailed,
}

pub(super) struct Transaction {
    id: u32,
    pub target: Target,
    ieee: [u8; 8],
    operation: Operation,
    tsn: Option<u8>,
    early: VecDeque<(u8, Receipt)>,
}

impl Transaction {
    pub fn new(id: u32, target: Target, ieee: [u8; 8], operation: Operation) -> Self {
        Self {
            id,
            target,
            ieee,
            operation,
            tsn: None,
            early: VecDeque::new(),
        }
    }

    pub fn feed(&mut self, event: Event) -> Option<Receipt> {
        if let Event::Transmitted {
            target,
            request_id,
            tsn,
            status,
        } = event
        {
            if target != self.target || request_id != self.id {
                return None;
            }
            if status != 0 {
                return Some(Receipt::TransmissionFailed);
            }
            self.tsn = Some(tsn);
            let early = self
                .early
                .drain(..)
                .find_map(|(seen, receipt)| (seen == tsn).then_some(receipt));
            return early.or_else(|| {
                matches!(self.operation, Operation::SetPower(_)).then_some(Receipt::Sent)
            });
        }

        let (source, receipt) = match event {
            Event::Attribute(attribute)
                if !attribute.is_report && self.operation == Operation::Read(attribute.id) =>
            {
                (attribute.source, Receipt::Read(attribute))
            }
            Event::ReportingConfigured {
                source,
                attribute,
                status,
            } if matches!(self.operation, Operation::Configure(id)
                    if id == attribute || (status == 0 && attribute == u16::MAX)) =>
            {
                let receipt = if status == 0 {
                    Receipt::Configured
                } else {
                    Receipt::Rejected
                };
                (source, receipt)
            }
            Event::DefaultResponse {
                source,
                command,
                status,
            } if status != 0 && command == self.command_id() => (source, Receipt::Rejected),
            _ => return None,
        };
        if source.target != self.target || source.ieee != self.ieee {
            return None;
        }
        let tsn = source.tsn;
        if let Some(expected) = self.tsn {
            return (expected == tsn).then_some(receipt);
        }
        // The SDK may deliver a response before the transmit confirmation.
        if self.early.len() == 8 {
            self.early.pop_front();
        }
        self.early.push_back((tsn, receipt));
        None
    }

    fn command_id(&self) -> u8 {
        match self.operation {
            Operation::Read(_) => 0x00,
            Operation::Configure(_) => 0x06,
            Operation::SetPower(on) => u8::from(on),
        }
    }
}

#[derive(Default)]
pub(super) struct RequestIds(u32);

impl RequestIds {
    pub fn next(&mut self, in_use: &[u32]) -> u32 {
        loop {
            self.0 = self.0.wrapping_add(1);
            if self.0 != 0 && !in_use.contains(&self.0) {
                return self.0;
            }
        }
    }
}

impl Transaction {
    pub fn id(&self) -> u32 {
        self.id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::radio::zigbee_event::{Payload, Source};

    const TARGET: Target = Target {
        address: 1,
        endpoint: 1,
        cluster: 6,
    };
    const IEEE: [u8; 8] = [9; 8];

    #[test]
    fn nonzero_ids_skip_inflight_values_on_wrap() {
        let mut ids = RequestIds(u32::MAX);
        assert_eq!(ids.next(&[1, 2]), 3);
    }

    #[test]
    fn early_read_matches_identity_endpoint_attribute_and_tsn() {
        let mut tx = Transaction::new(42, TARGET, IEEE, Operation::Read(0));
        let source = Source {
            target: TARGET,
            ieee: IEEE,
            tsn: 8,
            manufacturer: 0xffff,
            profile: 0x0104,
        };
        let make = |source| Attribute {
            source,
            id: 0,
            value_type: 0x10,
            status: 0,
            is_report: false,
            payload: Payload::Unavailable,
        };
        assert_eq!(
            tx.feed(Event::Attribute(make(Source {
                ieee: [1; 8],
                ..source
            }))),
            None
        );
        assert_eq!(
            tx.feed(Event::Attribute(make(Source { tsn: 7, ..source }))),
            None
        );
        assert_eq!(tx.feed(Event::Attribute(make(source))), None);
        assert_eq!(
            tx.feed(Event::Transmitted {
                target: TARGET,
                request_id: 42,
                tsn: 8,
                status: 0
            }),
            Some(Receipt::Read(make(source)))
        );
    }

    #[test]
    fn reporting_success_accepts_zcl_all_records_confirmation() {
        let mut tx = Transaction::new(43, TARGET, IEEE, Operation::Configure(0));
        let source = Source {
            target: TARGET,
            ieee: IEEE,
            tsn: 9,
            manufacturer: 0xffff,
            profile: 0x0104,
        };
        assert_eq!(
            tx.feed(Event::Transmitted {
                target: TARGET,
                request_id: 43,
                tsn: 9,
                status: 0
            }),
            None
        );
        assert_eq!(
            tx.feed(Event::ReportingConfigured {
                source,
                attribute: u16::MAX,
                status: 0
            }),
            Some(Receipt::Configured)
        );
    }
}
