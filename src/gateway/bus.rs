//! UI reads a snapshot. Uplink receives an independent, bounded observation feed.
//! Command delivery uses the durable command journal, separate from telemetry.
use crate::gateway::device::{Property, Value};
use crate::gateway::event::GatewayEvent;
use core::cell::Cell;
use embassy_sync::blocking_mutex::{Mutex, raw::CriticalSectionRawMutex};
use embassy_sync::channel::{Channel, Receiver};
use std::sync::atomic::{AtomicU32, Ordering};

pub const CAPACITY: usize = 32;
pub type UplinkReceiver = Receiver<'static, CriticalSectionRawMutex, GatewayEvent, CAPACITY>;

#[derive(Clone, Copy, Default)]
pub struct UiSnapshot {
    pub zigbee_ready: bool,
    pub peer: Option<(u16, bool)>,
    pub latest: Option<(Property, Value)>,
}

static UI: Mutex<CriticalSectionRawMutex, Cell<UiSnapshot>> = Mutex::new(Cell::new(UiSnapshot {
    zigbee_ready: false,
    peer: None,
    latest: None,
}));
static UPLINK: Channel<CriticalSectionRawMutex, GatewayEvent, CAPACITY> = Channel::new();
static UPLINK_DROPPED: AtomicU32 = AtomicU32::new(0);

#[cfg(test)]
pub(crate) static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub(crate) fn publish(event: GatewayEvent) {
    UI.lock(|snapshot| {
        let mut state = snapshot.get();
        match event {
            GatewayEvent::RadioZigbee { ready } => state.zigbee_ready = ready,
            GatewayEvent::PeerPresence {
                address, online, ..
            } => state.peer = Some((address, online)),
            GatewayEvent::Property {
                property, value, ..
            } => state.latest = Some((property, value)),
        }
        snapshot.set(state);
    });
    if UPLINK.try_send(event).is_err() {
        UPLINK_DROPPED.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn ui_snapshot() -> UiSnapshot {
    UI.lock(Cell::get)
}

/// Intended for the single uplink task. A slow consumer can lose observations.
pub fn uplink_receiver() -> UplinkReceiver {
    UPLINK.receiver()
}

pub fn uplink_dropped() -> u32 {
    UPLINK_DROPPED.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn independent_ui_snapshot_keeps_up_with_a_stalled_uplink() {
        let _lock = TEST_LOCK.lock().unwrap();
        let receiver = uplink_receiver();
        while receiver.try_receive().is_ok() {}
        let before = uplink_dropped();
        let device = crate::gateway::device::DeviceId::from_ieee([0x5a; 8]).unwrap();
        for index in 0..=(CAPACITY + 1) {
            publish(GatewayEvent::Property {
                device,
                endpoint: 1,
                property: Property::Temperature,
                value: Value::Number(index as f32),
            });
        }
        assert_eq!(
            ui_snapshot().latest,
            Some((Property::Temperature, Value::Number((CAPACITY + 1) as f32)))
        );
        assert!(uplink_dropped().wrapping_sub(before) >= 2);
        assert!(matches!(
            receiver.try_receive(),
            Ok(GatewayEvent::Property { .. })
        ));
    }
}
