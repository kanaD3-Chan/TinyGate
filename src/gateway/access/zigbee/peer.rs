//! Translate supported ZCL attributes into the gateway's product vocabulary.
use crate::drivers::radio::zigbee_event::{Attribute, Reporting, Target};
use crate::gateway::device::{Property, Value};
use embassy_time::{Duration, Instant};

pub(super) const PROFILE_HA: u16 = 0x0104;
pub(super) const CLUSTER_TEMPERATURE: u16 = 0x0402;
pub(super) const CLUSTER_ONOFF: u16 = 0x0006;
pub(super) const ATTRIBUTE_VALUE: u16 = 0x0000;
const ZCL_BOOLEAN: u8 = 0x10;
const ZCL_INT16: u8 = 0x29;
const TEMPERATURE_CHANGE: [u8; 2] = 50i16.to_le_bytes();
const REPORT_INTERVAL_S: u16 = 60;

pub(super) const PRESENCE_TIMEOUT: Duration = Duration::from_secs(REPORT_INTERVAL_S as u64 * 3);
pub(super) const POLL_INTERVAL: Duration = Duration::from_secs(REPORT_INTERVAL_S as u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Setup {
    Read,
    Bind,
    Configure,
    Subscribed,
    Poll,
}

pub(super) struct Endpoint {
    pub target: Target,
    pub setup: Setup,
    pub next_at: Instant,
}

pub(super) struct Peer {
    pub address: u16,
    pub online: bool,
    pub last_seen: Instant,
    pub discover_at: Instant,
    pub endpoints: Vec<Endpoint>,
}

impl Peer {
    pub fn new(address: u16, now: Instant) -> Self {
        Self {
            address,
            online: false,
            last_seen: now,
            discover_at: now,
            endpoints: Vec::new(),
        }
    }

    pub fn add_endpoint(&mut self, target: Target, now: Instant) {
        if supported(target.cluster) && self.endpoint_mut(target).is_none() {
            self.endpoints.push(Endpoint {
                target,
                setup: Setup::Read,
                next_at: now,
            });
        }
    }

    pub fn endpoint_mut(&mut self, target: Target) -> Option<&mut Endpoint> {
        self.endpoints
            .iter_mut()
            .find(|endpoint| endpoint.target == target)
    }

    pub fn power_target(&self, endpoint: u8) -> Option<Target> {
        self.endpoints
            .iter()
            .map(|endpoint| endpoint.target)
            .find(|target| target.cluster == CLUSTER_ONOFF && target.endpoint == endpoint)
    }
}

pub(super) fn supported(cluster: u16) -> bool {
    matches!(cluster, CLUSTER_TEMPERATURE | CLUSTER_ONOFF)
}

pub(super) fn decode(attribute: &Attribute) -> Option<(Property, Value)> {
    // Manufacturer-specific attributes may reuse a standard cluster/attribute ID.
    if attribute.id != ATTRIBUTE_VALUE
        || attribute.status != 0
        || attribute.source.profile != PROFILE_HA
        || !matches!(attribute.source.manufacturer, 0 | 0xffff)
    {
        return None;
    }
    match (
        attribute.source.target.cluster,
        attribute.value_type,
        attribute.payload.complete()?,
    ) {
        (CLUSTER_TEMPERATURE, ZCL_INT16, &[low, high]) => {
            let temperature = i16::from_le_bytes([low, high]);
            (temperature != i16::MIN).then_some((
                Property::Temperature,
                Value::Number(f32::from(temperature) / 100.0),
            ))
        }
        (CLUSTER_ONOFF, ZCL_BOOLEAN, &[on @ 0..=1]) => {
            Some((Property::Power, Value::Bool(on == 1)))
        }
        _ => None,
    }
}

pub(super) fn reporting(cluster: u16) -> Reporting {
    let (value_type, min_interval_s, change): (u8, u16, &[u8]) = match cluster {
        CLUSTER_TEMPERATURE => (ZCL_INT16, 5, &TEMPERATURE_CHANGE),
        _ => (ZCL_BOOLEAN, 1, &[]),
    };
    Reporting {
        attribute: ATTRIBUTE_VALUE,
        value_type,
        min_interval_s,
        max_interval_s: REPORT_INTERVAL_S,
        change,
    }
}

pub(super) fn is_peer_address(address: u16) -> bool {
    (1..0xfff8).contains(&address)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::radio::zigbee_event::{Payload, Source, VALUE_CAPACITY};

    fn raw(cluster: u16, value_type: u8, data: &[u8]) -> Attribute {
        let mut bytes = [0; VALUE_CAPACITY];
        bytes[..data.len()].copy_from_slice(data);
        Attribute {
            source: Source {
                target: Target {
                    address: 0x1234,
                    endpoint: 1,
                    cluster,
                },
                ieee: [1; 8],
                profile: PROFILE_HA,
                manufacturer: 0xffff,
                tsn: 1,
            },
            id: 0,
            value_type,
            status: 0,
            is_report: true,
            payload: Payload::Complete {
                bytes,
                len: data.len() as u8,
            },
        }
    }

    #[test]
    fn turns_only_valid_standard_attributes_into_observations() {
        let temp = raw(CLUSTER_TEMPERATURE, ZCL_INT16, &(-1250i16).to_le_bytes());
        let power = raw(CLUSTER_ONOFF, ZCL_BOOLEAN, &[1]);
        assert_eq!(
            decode(&temp),
            Some((Property::Temperature, Value::Number(-12.5)))
        );
        assert_eq!(decode(&power), Some((Property::Power, Value::Bool(true))));
        assert_eq!(decode(&Attribute { status: 1, ..power }), None);
        assert_eq!(decode(&Attribute { id: 0xf000, ..temp }), None);
        assert_eq!(decode(&raw(CLUSTER_ONOFF, ZCL_BOOLEAN, &[2])), None);
        assert_eq!(
            decode(&raw(
                CLUSTER_TEMPERATURE,
                ZCL_INT16,
                &i16::MIN.to_le_bytes()
            )),
            None
        );
        assert_eq!(
            decode(&Attribute {
                payload: Payload::Truncated {
                    prefix: [0; VALUE_CAPACITY]
                },
                ..temp
            }),
            None
        );
    }

    #[test]
    fn unknown_cluster_keeps_the_zcl_type_and_payload() {
        let humidity = raw(0x0405, 0x21, &5000u16.to_le_bytes());
        assert_eq!(humidity.value_type, 0x21);
        assert_eq!(
            humidity.payload.complete(),
            Some(5000u16.to_le_bytes().as_slice())
        );
        assert_eq!(decode(&humidity), None);
    }
}
