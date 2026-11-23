//! The coordinator task owns hardware, device routes and in-flight transactions.
mod peer;
mod transaction;

use crate::drivers::radio::zigbee::{Coordinator, Event, Status, Target};
use crate::gateway::bus;
use crate::gateway::command::{self, Failure, Outcome, Work};
use crate::gateway::device::{DeviceId, Value};
use crate::gateway::event::GatewayEvent;
use embassy_time::{Duration, Instant, Timer};
use log::{info, warn};
use peer::{ATTRIBUTE_VALUE, POLL_INTERVAL, PRESENCE_TIMEOUT, PROFILE_HA, Peer, Setup};
use std::collections::HashMap;
use transaction::{Operation, Receipt, RequestIds, Transaction};

const TICK: Duration = Duration::from_millis(20);
const DISCOVERY_RETRY: Duration = Duration::from_secs(5);
const SETUP_TIMEOUT: Duration = Duration::from_secs(4);
const RECOVERY_GRACE: Duration = Duration::from_secs(45);
const JOIN_WINDOW_SECONDS: u8 = 60;
const MAX_PEERS: usize = 16;
const EVENTS_PER_TICK: usize = 16;

#[embassy_executor::task]
pub async fn coordinator_service() {
    let mut radio = Coordinator::new()
        .map_err(|err| warn!("Zigbee init: {err:#}"))
        .ok();
    let mut service = Service::new();
    loop {
        if let Some(radio) = radio.as_mut() {
            match radio.status() {
                Ok(status) => service.status(status, radio),
                Err(err) => warn!("Zigbee status: {err:#}"),
            }
            for _ in 0..EVENTS_PER_TICK {
                match radio.poll() {
                    Ok(Some(event)) => service.event(event),
                    Ok(None) => break,
                    Err(err) => {
                        warn!("Zigbee poll: {err:#}");
                        break;
                    }
                }
            }
        }
        service.tick(radio.as_mut());
        Timer::after(TICK).await;
    }
}

struct Service {
    ready: bool,
    join_opened: bool,
    dropped: u32,
    peers: HashMap<DeviceId, Peer>,
    unresolved: HashMap<u16, Instant>,
    ids: RequestIds,
    setup: Option<Maintenance>,
    active: Option<Active>,
    completion: Option<(String, Outcome)>,
    recovery_until: Instant,
    recovery_probe_at: Instant,
    retry_result_at: Instant,
}

struct Active {
    work: Work,
    target: Target,
    deadline: Instant,
    write: Option<Transaction>,
    write_sent: bool,
    read: Option<Transaction>,
}

struct Maintenance {
    device: DeviceId,
    target: Target,
    phase: Setup,
    id: u32,
    deadline: Instant,
    zcl: Option<Transaction>,
}

#[derive(Clone, Copy)]
enum Sighting {
    Announced,
    Resolved,
    Traffic,
}

impl Service {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            ready: false,
            join_opened: false,
            dropped: 0,
            peers: HashMap::new(),
            unresolved: HashMap::new(),
            ids: RequestIds::default(),
            setup: None,
            active: None,
            completion: None,
            recovery_until: now + RECOVERY_GRACE,
            recovery_probe_at: now,
            retry_result_at: now,
        }
    }

    fn status(&mut self, status: Status, radio: &mut Coordinator) {
        if self.ready != status.ready {
            self.ready = status.ready;
            bus::publish(GatewayEvent::RadioZigbee {
                ready: status.ready,
            });
            if status.ready {
                info!(
                    "Zigbee ready PAN={:04X} channel={}",
                    status.pan_id, status.channel
                );
            } else {
                self.resynchronize();
                for (device, peer) in &mut self.peers {
                    mark_offline(*device, peer);
                }
            }
        }
        if status.dropped_events != self.dropped {
            warn!("Zigbee event overflow: {} dropped", status.dropped_events);
            self.dropped = status.dropped_events;
            self.resynchronize();
            self.unresolved(status.last_peer);
        }
        if self.ready && !self.join_opened {
            match radio.permit_join(JOIN_WINDOW_SECONDS) {
                Ok(()) => self.join_opened = true,
                Err(err) => {
                    self.join_opened = true;
                    warn!("Zigbee join window: {err:#}");
                }
            }
        }
    }

    fn resynchronize(&mut self) {
        self.finish_active(Outcome::Failed(Failure::ExecutionUncertain));
        self.setup = None;
        let now = Instant::now();
        for peer in self.peers.values_mut() {
            peer.discover_at = now;
            for endpoint in &mut peer.endpoints {
                endpoint.setup = Setup::Read;
                endpoint.next_at = now;
            }
        }
    }

    fn event(&mut self, event: Event) {
        let now = Instant::now();
        match event {
            Event::Join { address, ieee } => {
                self.observe(address, ieee, now, Sighting::Announced);
            }
            Event::Identity {
                address,
                ieee,
                status: 0,
            } => {
                self.observe(address, ieee, now, Sighting::Resolved);
            }
            Event::Endpoint(source) if source.profile == PROFILE_HA => {
                if let Some(device) =
                    self.observe(source.target.address, source.ieee, now, Sighting::Traffic)
                {
                    self.peers
                        .get_mut(&device)
                        .unwrap()
                        .add_endpoint(source.target, now);
                }
            }
            Event::Attribute(attribute) => {
                if let Some((property, value)) = peer::decode(&attribute) {
                    let target = attribute.source.target;
                    if let Some(device) = self.observe(
                        target.address,
                        attribute.source.ieee,
                        now,
                        Sighting::Traffic,
                    ) {
                        // Discovery owns endpoint configuration. Early valid reports are useful
                        // even if the corresponding descriptor has not arrived yet.
                        bus::publish(GatewayEvent::Property {
                            device,
                            endpoint: target.endpoint,
                            property,
                            value,
                        });
                    }
                } else if !peer::supported(attribute.source.target.cluster) {
                    log::debug!("Unmapped Zigbee attribute: {:?}", attribute);
                }
            }
            Event::Leave { address, ieee } => {
                if let Some(device) = DeviceId::from_ieee(ieee).or_else(|| self.device_at(address))
                {
                    if let Some(peer) = self
                        .peers
                        .get_mut(&device)
                        .filter(|peer| peer.address == address)
                    {
                        mark_offline(device, peer);
                        peer.address = 0xffff;
                        peer.endpoints.clear();
                    }
                }
            }
            Event::Error { address, status } => warn!("Zigbee error at {address:#06x}: {status}"),
            _ => {}
        }
        self.command_event(event, now);
        self.maintenance_event(event);
    }

    fn observe(
        &mut self,
        address: u16,
        ieee: [u8; 8],
        now: Instant,
        sighting: Sighting,
    ) -> Option<DeviceId> {
        if !self.ready || !peer::is_peer_address(address) {
            return None;
        }
        let previous = self.device_at(address);
        let Some(device) = DeviceId::from_ieee(ieee).or(previous) else {
            self.unresolved(address);
            return None;
        };
        if matches!(sighting, Sighting::Traffic)
            && self
                .peers
                .get(&device)
                .is_some_and(|peer| peer::is_peer_address(peer.address) && peer.address != address)
        {
            self.unresolved(address);
            return None;
        }
        if let Some(previous) = previous.filter(|previous| *previous != device) {
            let peer = self.peers.get_mut(&previous).unwrap();
            mark_offline(previous, peer);
            peer.address = 0xffff;
            peer.endpoints.clear();
        }
        self.unresolved.remove(&address);
        if !self.peers.contains_key(&device) {
            if self.peers.len() >= MAX_PEERS {
                let oldest = self
                    .peers
                    .iter()
                    .filter(|(_, peer)| !peer.online)
                    .min_by_key(|(_, peer)| peer.last_seen)
                    .map(|(device, _)| *device);
                self.peers.remove(&oldest?);
            }
            self.peers.insert(device, Peer::new(address, now));
        }
        let peer = self.peers.get_mut(&device).unwrap();
        if peer.address != address {
            mark_offline(device, peer);
            *peer = Peer::new(address, now);
        }
        let live = !matches!(sighting, Sighting::Resolved);
        if live && !peer.online {
            peer.online = true;
            bus::publish(GatewayEvent::PeerPresence {
                device,
                address,
                online: true,
            });
        }
        if live {
            peer.last_seen = now;
        }
        Some(device)
    }

    fn unresolved(&mut self, address: u16) {
        if peer::is_peer_address(address) && self.unresolved.len() < MAX_PEERS {
            self.unresolved.entry(address).or_insert_with(Instant::now);
        }
    }

    fn device_at(&self, address: u16) -> Option<DeviceId> {
        self.peers
            .iter()
            .find(|(_, peer)| peer.address == address)
            .map(|(device, _)| *device)
    }

    fn tick(&mut self, radio: Option<&mut Coordinator>) {
        let now = Instant::now();
        for (device, peer) in &mut self.peers {
            if peer.online && now - peer.last_seen >= PRESENCE_TIMEOUT {
                mark_offline(*device, peer);
                for endpoint in &mut peer.endpoints {
                    endpoint.setup = Setup::Poll;
                    endpoint.next_at = now;
                }
            }
        }
        if let Some(setup) = &self.setup {
            if now >= setup.deadline {
                self.finish_maintenance(false);
            }
        }
        if let Some(active) = &self.active {
            if active.expired(now) {
                let outcome = active.expiration();
                self.finish_active(outcome);
            } else if self.power_target(&active.work) != Ok(active.target) {
                self.finish_active(Outcome::Failed(Failure::ExecutionUncertain));
            }
        }

        if let Some((id, outcome)) = &self.completion {
            if now >= self.retry_result_at {
                match command::record(id, *outcome) {
                    Ok(()) => self.completion = None,
                    Err(err) => {
                        self.retry_result_at = now + Duration::from_secs(1);
                        warn!("Could not persist command result: {err:#}");
                    }
                }
            }
        }

        if let Some(radio) = radio.filter(|_| self.ready) {
            if self
                .active
                .as_ref()
                .is_some_and(|active| active.write_sent && active.read.is_none())
            {
                self.start_readback(radio);
            }
            if self.active.is_none() && self.completion.is_none() {
                self.start_command(Some(&mut *radio), now);
            }
            if self.active.is_none() {
                self.maintain(radio, now);
            }
        } else if self.active.is_none() && self.completion.is_none() {
            self.start_command(None, now);
        }
    }

    fn next_id(&mut self) -> u32 {
        let active = self.active.as_ref();
        self.ids.next(&[
            self.setup.as_ref().map_or(0, |setup| setup.id),
            active
                .and_then(|active| active.write.as_ref())
                .map_or(0, Transaction::id),
            active
                .and_then(|active| active.read.as_ref())
                .map_or(0, Transaction::id),
        ])
    }

    fn power_target(&self, work: &Work) -> Result<Target, Failure> {
        if !self.ready {
            return Err(Failure::DeviceOffline);
        }
        let peer = self
            .peers
            .get(&work.command.device)
            .filter(|peer| peer.online)
            .ok_or(Failure::DeviceOffline)?;
        peer.power_target(work.command.endpoint)
            .ok_or(Failure::UnsupportedAction)
    }

    fn start_command(&mut self, radio: Option<&mut Coordinator>, now: Instant) {
        let Some(work) = command::next_work(false).or_else(|| command::next_work(true)) else {
            return;
        };
        let target = self.power_target(&work);
        let deadline = work.deadline();
        if work.recovery {
            if !work.was_accepted {
                self.completion = Some((
                    work.command.command_id,
                    Outcome::Failed(Failure::DeviceOffline),
                ));
                return;
            }
            if let Err(Outcome::Expired) = deadline {
                self.completion = Some((work.command.command_id, Outcome::Expired));
                return;
            }
            if target.is_err() || deadline.is_err() || radio.is_none() {
                if now >= self.recovery_until {
                    self.completion = Some((
                        work.command.command_id,
                        Outcome::Failed(Failure::ExecutionUncertain),
                    ));
                } else if target.is_err() && now >= self.recovery_probe_at {
                    self.recovery_probe_at = now + DISCOVERY_RETRY;
                    if let Some(radio) = radio {
                        let ieee = work.command.device.ieee();
                        if let Ok(address) = radio.address_for(ieee) {
                            self.observe(address, ieee, now, Sighting::Resolved);
                        }
                    }
                }
                return;
            }
        }
        let target = match deadline.and_then(|deadline| {
            target
                .map_err(Outcome::Failed)
                .map(|target| (target, deadline))
        }) {
            Ok((target, deadline)) if now < deadline => (target, deadline),
            Ok(_) => {
                self.completion = Some((work.command.command_id, Outcome::Expired));
                return;
            }
            Err(outcome) => {
                self.completion = Some((work.command.command_id, outcome));
                return;
            }
        };
        let Some(radio) = radio else { return };
        let (target, deadline) = target;
        if !work.recovery {
            if let Err(err) = command::record(&work.command.command_id, Outcome::Accepted) {
                self.completion = Some((
                    work.command.command_id,
                    Outcome::Failed(Failure::ExecutionUncertain),
                ));
                warn!("Could not persist accepted command: {err:#}");
                return;
            }
        }
        let recovery = work.recovery;
        self.active = Some(Active {
            work,
            target,
            deadline,
            write: None,
            write_sent: recovery,
            read: None,
        });
        if recovery {
            self.start_readback(radio);
            return;
        }
        let id = self.next_id();
        let active = self.active.as_mut().unwrap();
        if active.expired(Instant::now()) {
            let outcome = active.expiration();
            self.finish_active(outcome);
            return;
        }
        let on = active.work.command.on;
        if let Err(err) = radio.onoff(target, active.work.command.device.ieee(), on, id) {
            warn!("Could not submit Zigbee command: {err:#}");
            self.finish_active(Outcome::Failed(Failure::Rejected));
        } else {
            active.write = Some(Transaction::new(
                id,
                target,
                active.work.command.device.ieee(),
                Operation::SetPower(on),
            ));
        }
    }

    fn start_readback(&mut self, radio: &mut Coordinator) {
        let id = self.next_id();
        let active = self.active.as_mut().unwrap();
        if active.expired(Instant::now()) {
            let outcome = active.expiration();
            self.finish_active(outcome);
            return;
        }
        if let Err(err) = radio.read(active.target, ATTRIBUTE_VALUE, id) {
            warn!("Could not read back Zigbee command: {err:#}");
            self.finish_active(Outcome::Failed(Failure::ExecutionUncertain));
        } else {
            active.read = Some(Transaction::new(
                id,
                active.target,
                active.work.command.device.ieee(),
                Operation::Read(ATTRIBUTE_VALUE),
            ));
        }
    }

    fn command_event(&mut self, event: Event, now: Instant) {
        let Some(active) = &self.active else { return };
        if active.expired(now) {
            let outcome = active.expiration();
            self.finish_active(outcome);
            return;
        }
        if self.power_target(&active.work) != Ok(active.target) {
            self.finish_active(Outcome::Failed(Failure::ExecutionUncertain));
            return;
        }
        let active = self.active.as_mut().unwrap();
        if let Some(receipt) = active.write.as_mut().and_then(|tx| tx.feed(event)) {
            match receipt {
                Receipt::Sent => active.write_sent = true,
                Receipt::Rejected => {
                    self.finish_active(Outcome::Failed(Failure::Rejected));
                    return;
                }
                Receipt::TransmissionFailed => {
                    self.finish_active(Outcome::Failed(Failure::ExecutionUncertain));
                    return;
                }
                _ => {}
            }
        }
        let result = match active.read.as_mut().and_then(|tx| tx.feed(event)) {
            Some(Receipt::Read(attribute)) => match peer::decode(&attribute) {
                Some((_, Value::Bool(on))) if on == active.work.command.on => Outcome::Succeeded,
                _ => Outcome::Failed(Failure::ExecutionUncertain),
            },
            Some(Receipt::Rejected | Receipt::TransmissionFailed) => {
                Outcome::Failed(Failure::ExecutionUncertain)
            }
            _ => return,
        };
        self.finish_active(result);
    }

    fn finish_active(&mut self, result: Outcome) {
        if let Some(active) = self.active.take() {
            self.completion = Some((active.work.command.command_id, result));
        }
    }

    fn maintain(&mut self, radio: &mut Coordinator, now: Instant) {
        if self.setup.is_some() {
            return;
        }
        if let Some((&address, due)) = self.unresolved.iter_mut().find(|(_, due)| **due <= now) {
            *due = now + DISCOVERY_RETRY;
            if let Err(err) = radio.resolve(address) {
                log::debug!("Resolve {address:#06x}: {err:#}");
            }
            self.unresolved.remove(&address);
            return;
        }
        if let Some(peer) = self.peers.values_mut().find(|peer| {
            peer::is_peer_address(peer.address)
                && (peer.online || peer.endpoints.is_empty())
                && peer.discover_at <= now
        }) {
            peer.discover_at = now
                + if peer.endpoints.is_empty() {
                    DISCOVERY_RETRY
                } else {
                    POLL_INTERVAL
                };
            if let Err(err) = radio.discover(peer.address) {
                log::debug!("Discover {:#06x}: {err:#}", peer.address);
            }
            return;
        }
        let due = self
            .peers
            .iter()
            .flat_map(|(device, peer)| {
                peer.endpoints
                    .iter()
                    .map(move |endpoint| (*device, endpoint))
            })
            .filter(|(_, endpoint)| endpoint.setup != Setup::Subscribed && endpoint.next_at <= now)
            .min_by_key(|(_, endpoint)| endpoint.next_at)
            .map(|(device, endpoint)| (device, endpoint.target, endpoint.setup));
        let Some((device, target, phase)) = due else {
            return;
        };
        let id = self.next_id();
        let (sent, operation) = match phase {
            Setup::Read | Setup::Poll => (
                radio.read(target, ATTRIBUTE_VALUE, id),
                Some(Operation::Read(ATTRIBUTE_VALUE)),
            ),
            Setup::Bind => (radio.bind(target, device.ieee(), id), None),
            Setup::Configure => (
                radio.reporting(target, peer::reporting(target.cluster), id),
                Some(Operation::Configure(ATTRIBUTE_VALUE)),
            ),
            Setup::Subscribed => return,
        };
        self.setup = Some(Maintenance {
            device,
            target,
            phase,
            id,
            deadline: now + SETUP_TIMEOUT,
            zcl: operation.map(|operation| Transaction::new(id, target, device.ieee(), operation)),
        });
        if let Err(err) = sent {
            log::debug!("Zigbee {phase:?} for {target:?}: {err:#}");
            self.finish_maintenance(false);
        }
    }

    fn maintenance_event(&mut self, event: Event) {
        let Some(setup) = &mut self.setup else { return };
        let success = match event {
            Event::Bound { request_id, status }
                if request_id == setup.id && setup.phase == Setup::Bind =>
            {
                status == 0
            }
            _ => match setup.zcl.as_mut().and_then(|zcl| zcl.feed(event)) {
                Some(Receipt::Read(attribute)) => peer::decode(&attribute).is_some(),
                Some(Receipt::Configured) => true,
                Some(Receipt::Rejected | Receipt::TransmissionFailed) => false,
                _ => return,
            },
        };
        self.finish_maintenance(success);
    }

    fn finish_maintenance(&mut self, success: bool) {
        let Some(setup) = self.setup.take() else {
            return;
        };
        let Some(endpoint) = self
            .peers
            .get_mut(&setup.device)
            .and_then(|peer| peer.endpoint_mut(setup.target))
            .filter(|endpoint| endpoint.setup == setup.phase)
        else {
            return;
        };
        endpoint.setup = match (setup.phase, success) {
            (Setup::Read, _) => Setup::Bind,
            (Setup::Bind, true) => Setup::Configure,
            (Setup::Configure, true) => Setup::Subscribed,
            _ => Setup::Poll,
        };
        endpoint.next_at = Instant::now()
            + if endpoint.setup == Setup::Poll {
                POLL_INTERVAL
            } else {
                TICK
            };
    }
}

impl Active {
    fn expired(&self, now: Instant) -> bool {
        now >= self.deadline || self.gateway_deadline_passed()
    }

    fn gateway_deadline_passed(&self) -> bool {
        self.work
            .command
            .expires_at_ms
            .zip(command::utc_now())
            .is_some_and(|(expires, now)| now >= expires)
    }

    fn expiration(&self) -> Outcome {
        if self.work.command.expires_at_ms.is_none() || self.gateway_deadline_passed() {
            Outcome::Expired
        } else {
            Outcome::Failed(Failure::DeviceTimeout)
        }
    }
}

fn mark_offline(device: DeviceId, peer: &mut Peer) {
    if peer.online {
        peer.online = false;
        bus::publish(GatewayEvent::PeerPresence {
            device,
            address: peer.address,
            online: false,
        });
    }
}

#[cfg(all(test, not(target_os = "espidf")))]
mod tests {
    use super::*;
    use crate::drivers::radio::zigbee_event::{Attribute, Payload, Source, VALUE_CAPACITY};
    use crate::gateway::command::{ResultEvent, SetPower};
    use esp_idf_svc::nvs::EspDefaultNvsPartition;
    use peer::CLUSTER_ONOFF;

    const IEEE: [u8; 8] = [8, 7, 6, 5, 4, 3, 2, 1];
    const TARGET: Target = Target {
        address: 0x1234,
        endpoint: 2,
        cluster: CLUSTER_ONOFF,
    };
    const HEALTHY: Status = Status {
        ready: true,
        channel: 15,
        pan_id: 123,
        last_peer: 0xffff,
        dropped_events: 0,
    };

    fn source(target: Target, ieee: [u8; 8], tsn: u8) -> Source {
        Source {
            target,
            ieee,
            tsn,
            profile: PROFILE_HA,
            manufacturer: 0xffff,
        }
    }

    fn attribute(target: Target, ieee: [u8; 8], tsn: u8, report: bool) -> Event {
        let mut bytes = [0; VALUE_CAPACITY];
        bytes[0] = 1;
        Event::Attribute(Attribute {
            source: source(target, ieee, tsn),
            id: ATTRIBUTE_VALUE,
            value_type: 0x10,
            status: 0,
            is_report: report,
            payload: Payload::Complete { bytes, len: 1 },
        })
    }

    fn sent(id: u32, tsn: u8) -> Event {
        Event::Transmitted {
            target: TARGET,
            request_id: id,
            tsn,
            status: 0,
        }
    }

    fn command_for(id: &str, on: bool) -> SetPower {
        SetPower {
            command_id: id.to_owned(),
            device: DeviceId::from_ieee(IEEE).unwrap(),
            endpoint: TARGET.endpoint,
            on,
            expires_at_ms: None,
        }
    }

    fn outcome(id: &str) -> Outcome {
        command::pending_results()
            .into_iter()
            .find(|event| event.command.command_id == id)
            .unwrap()
            .outcome
    }

    fn add_node(service: &mut Service, target: Target, ieee: [u8; 8]) {
        service.event(Event::Join {
            address: target.address,
            ieee,
        });
        service.event(Event::Endpoint(source(target, ieee, 0)));
    }

    #[test]
    fn set_power_requires_fresh_matching_readback_and_survives_faults() {
        let _lock = bus::TEST_LOCK.lock().unwrap();
        command::init(EspDefaultNvsPartition::take().unwrap()).unwrap();
        let mut service = Service::new();
        let mut radio = Coordinator::default();
        service.status(HEALTHY, &mut radio);
        add_node(&mut service, TARGET, IEEE);
        add_node(
            &mut service,
            Target {
                address: 0x1235,
                ..TARGET
            },
            [2; 8],
        );

        let request = command_for("on", true);
        command::submit_local(request.clone(), 5000).unwrap();
        service.tick(Some(&mut radio));
        let (target, ieee, on, write_id) = radio.writes[0];
        assert_eq!((target, ieee, on, write_id), (TARGET, IEEE, true, 1));
        service.event(sent(write_id, 1));
        service.tick(Some(&mut radio));
        let (_, _, read_id) = radio.reads[0];
        assert_ne!(read_id, write_id);

        // Same value from the wrong peer or an unsolicited report cannot complete the command.
        service.event(attribute(
            Target {
                address: 0x1235,
                ..TARGET
            },
            [2; 8],
            2,
            false,
        ));
        service.event(attribute(TARGET, IEEE, 2, true));
        service.event(sent(read_id, 2));
        assert_eq!(outcome("on"), Outcome::Accepted);
        service.event(attribute(TARGET, IEEE, 2, false));
        service.tick(Some(&mut radio));
        assert_eq!(outcome("on"), Outcome::Succeeded);

        // Replaying the same command cannot send a second radio write.
        command::submit_local(request, 5000).unwrap();
        service.tick(Some(&mut radio));
        assert_eq!(radio.writes.len(), 1);

        command::submit_local(command_for("reject", false), 5000).unwrap();
        service.tick(Some(&mut radio));
        let reject_id = radio.writes[1].3;
        service.event(Event::DefaultResponse {
            source: source(TARGET, IEEE, 3),
            command: 0,
            status: 1,
        });
        service.event(sent(reject_id, 3));
        service.tick(Some(&mut radio));
        assert_eq!(outcome("reject"), Outcome::Failed(Failure::Rejected));

        command::submit_local(command_for("late", true), 5000).unwrap();
        service.tick(Some(&mut radio));
        let too_late = Instant::now() + Duration::from_secs(6);
        service.command_event(sent(radio.writes[2].3, 4), too_late);
        service.tick(Some(&mut radio));
        assert_eq!(outcome("late"), Outcome::Expired);
        service.event(attribute(TARGET, IEEE, 4, false));
        assert_eq!(outcome("late"), Outcome::Expired);

        command::submit_local(command_for("overflow", false), 5000).unwrap();
        service.tick(Some(&mut radio));
        service.status(
            Status {
                dropped_events: 1,
                ..HEALTHY
            },
            &mut radio,
        );
        service.tick(Some(&mut radio));
        assert_eq!(
            outcome("overflow"),
            Outcome::Failed(Failure::ExecutionUncertain)
        );

        let new_target = Target {
            address: 0x1236,
            ..TARGET
        };
        add_node(&mut service, new_target, IEEE);
        service.event(attribute(TARGET, IEEE, 5, true));
        command::submit_local(command_for("rejoin", true), 5000).unwrap();
        service.tick(Some(&mut radio));
        assert_eq!(radio.writes.last().unwrap().0, new_target);

        // There is no route that would allow an old ACK to consume a new result.
        command::acknowledge(&ResultEvent {
            command: command_for("on", true),
            outcome: Outcome::Accepted,
        })
        .unwrap();
        assert_eq!(outcome("on"), Outcome::Succeeded);
    }

    #[test]
    fn marks_reporting_subscribed_only_after_bind_and_configuration_ack() {
        let _lock = bus::TEST_LOCK.lock().unwrap();
        let mut service = Service::new();
        let mut radio = Coordinator::default();
        service.status(HEALTHY, &mut radio);
        add_node(&mut service, TARGET, IEEE);
        let now = Instant::now();
        service.maintain(&mut radio, now);
        service.maintain(&mut radio, now);
        let read = service.setup.as_ref().unwrap();
        assert_eq!(read.phase, Setup::Read);
        service.maintenance_event(sent(read.id, 1));
        service.maintenance_event(attribute(TARGET, IEEE, 1, false));

        let later = now + Duration::from_secs(1);
        service.maintain(&mut radio, later);
        let bind = service.setup.as_ref().unwrap();
        assert_eq!(bind.phase, Setup::Bind);
        service.maintenance_event(Event::Bound {
            request_id: bind.id,
            status: 0,
        });
        service.maintain(&mut radio, later);
        let config = service.setup.as_ref().unwrap();
        assert_eq!(config.phase, Setup::Configure);
        service.maintenance_event(sent(config.id, 2));
        let dev = DeviceId::from_ieee(IEEE).unwrap();
        assert_eq!(service.peers[&dev].endpoints[0].setup, Setup::Configure);
        service.maintenance_event(Event::ReportingConfigured {
            source: source(TARGET, IEEE, 2),
            attribute: u16::MAX,
            status: 0,
        });
        assert_eq!(service.peers[&dev].endpoints[0].setup, Setup::Subscribed);
    }
}
