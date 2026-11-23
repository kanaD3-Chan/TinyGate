//! Facts the gateway publishes on its internal bus.
//!
//! An event is deliberately protocol-neutral: access services translate
//! Zigbee/BLE observations into this vocabulary, and consumers (UI today;
//! uplink and rules later) never see C types.
use crate::gateway::device::{DeviceId, Property, Value};

/// A radio observation worth showing, and later, uploading.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum GatewayEvent {
    /// Coordinator lifecycle; BLE joins this in its milestone.
    RadioZigbee { ready: bool },
    /// A known peer joined, left or passed its reporting deadline.
    PeerPresence {
        device: DeviceId,
        address: u16,
        online: bool,
    },
    /// Observed property value (`endpoint` is the protocol endpoint).
    Property {
        device: DeviceId,
        endpoint: u8,
        property: Property,
        value: Value,
    },
}
