//! Single-owner coordinator facade. The SDK lifetime is the firmware lifetime.
use crate::drivers::lease::{Lease, check};
use anyhow::{Result, bail};
use esp_idf_svc::sys;
use std::sync::atomic::AtomicBool;

pub use super::zigbee_event::{Attribute, Event, Payload, Reporting, Source, Target};

static OWNED: AtomicBool = AtomicBool::new(false);

pub struct Coordinator {
    _lease: Lease,
}

#[derive(Clone, Copy)]
pub struct Status {
    pub ready: bool,
    pub channel: u8,
    pub pan_id: u16,
    pub last_peer: u16,
    pub dropped_events: u32,
}

impl Coordinator {
    pub fn new() -> Result<Self> {
        let mut lease = Lease::acquire(&OWNED, "Zigbee coordinator")?;
        if let Err(error) = check(unsafe { sys::tg_zigbee_init() }, "Zigbee init") {
            lease.release();
            return Err(error);
        }
        Ok(Self { _lease: lease })
    }

    pub fn status(&self) -> Result<Status> {
        let mut raw = unsafe { std::mem::zeroed() };
        check(
            unsafe { sys::tg_zigbee_get_status(&mut raw) },
            "Zigbee status",
        )?;
        Ok(Status {
            ready: raw.ready,
            channel: raw.channel,
            pan_id: raw.pan_id,
            last_peer: raw.peer_address,
            dropped_events: raw.dropped_events,
        })
    }

    pub fn poll(&mut self) -> Result<Option<Event>> {
        let mut raw = unsafe { std::mem::zeroed() };
        let code = unsafe { sys::tg_zigbee_poll_event(&mut raw) };
        if code == sys::ESP_ERR_NOT_FOUND {
            return Ok(None);
        }
        check(code, "Zigbee poll")?;
        let address = raw.short_address;
        let ieee = raw.ieee_address;
        let status = raw.status;
        let target = Target {
            address,
            endpoint: raw.endpoint,
            cluster: raw.cluster,
        };
        let source = Source {
            target,
            ieee,
            profile: raw.profile,
            manufacturer: raw.manufacturer,
            tsn: raw.tsn,
        };
        let event = match u32::from(raw.kind) {
            sys::TG_ZB_NETWORK => Event::Network,
            sys::TG_ZB_JOIN => Event::Join { address, ieee },
            sys::TG_ZB_LEAVE => Event::Leave { address, ieee },
            sys::TG_ZB_IDENTITY => Event::Identity {
                address,
                ieee,
                status,
            },
            sys::TG_ZB_ENDPOINT => Event::Endpoint(source),
            sys::TG_ZB_ATTRIBUTE => {
                let payload = match u32::from(raw.value_status) {
                    sys::TG_ZB_VALUE_COMPLETE => Payload::Complete {
                        bytes: raw.value,
                        len: raw.value_len,
                    },
                    sys::TG_ZB_VALUE_TRUNCATED => Payload::Truncated { prefix: raw.value },
                    _ => Payload::Unavailable,
                };
                Event::Attribute(Attribute {
                    source,
                    id: raw.attribute,
                    value_type: raw.value_type,
                    status,
                    is_report: raw.is_report,
                    payload,
                })
            }
            sys::TG_ZB_TX_CONFIRM => Event::Transmitted {
                target,
                request_id: raw.request_id,
                tsn: raw.tsn,
                status,
            },
            sys::TG_ZB_DEFAULT_RESPONSE => Event::DefaultResponse {
                source,
                command: raw.command,
                status,
            },
            sys::TG_ZB_REPORT_CONFIG => Event::ReportingConfigured {
                source,
                attribute: raw.attribute,
                status,
            },
            sys::TG_ZB_BIND => Event::Bound {
                request_id: raw.request_id,
                status,
            },
            sys::TG_ZB_ERROR => Event::Error { address, status },
            kind => bail!("Unknown Zigbee event kind {kind}"),
        };
        Ok(Some(event))
    }

    pub fn permit_join(&mut self, seconds: u8) -> Result<()> {
        check(
            unsafe { sys::tg_zigbee_permit_join(seconds) },
            "Zigbee permit join",
        )
    }

    pub fn resolve(&mut self, address: u16) -> Result<()> {
        check(
            unsafe { sys::tg_zigbee_resolve(address) },
            "Zigbee resolve identity",
        )
    }

    pub fn address_for(&mut self, ieee: [u8; 8]) -> Result<u16> {
        let mut address = 0xffff;
        check(
            unsafe { sys::tg_zigbee_address_for(ieee.as_ptr(), &mut address) },
            "Zigbee find address",
        )?;
        Ok(address)
    }

    pub fn discover(&mut self, address: u16) -> Result<()> {
        check(
            unsafe { sys::tg_zigbee_discover(address) },
            "Zigbee discover",
        )
    }

    pub fn read(&mut self, target: Target, attribute: u16, id: u32) -> Result<()> {
        check(
            unsafe {
                sys::tg_zigbee_read_attribute(
                    target.address,
                    target.endpoint,
                    target.cluster,
                    attribute,
                    id,
                )
            },
            "Zigbee read",
        )
    }

    pub fn onoff(&mut self, target: Target, ieee: [u8; 8], on: bool, id: u32) -> Result<()> {
        check(
            unsafe {
                sys::tg_zigbee_send_onoff(
                    target.address,
                    ieee.as_ptr(),
                    target.endpoint,
                    u8::from(on),
                    id,
                )
            },
            "Zigbee on/off",
        )
    }

    pub fn bind(&mut self, target: Target, ieee: [u8; 8], id: u32) -> Result<()> {
        check(
            unsafe {
                sys::tg_zigbee_bind(
                    target.address,
                    ieee.as_ptr(),
                    target.endpoint,
                    target.cluster,
                    id,
                )
            },
            "Zigbee bind",
        )
    }

    pub fn reporting(&mut self, target: Target, config: Reporting, id: u32) -> Result<()> {
        let mut raw = sys::tg_zigbee_reporting_t {
            attribute: config.attribute,
            value_type: config.value_type,
            min_interval_s: config.min_interval_s,
            max_interval_s: config.max_interval_s,
            change_len: 0,
            change: [0; sys::TG_ZB_REPORT_CHANGE_CAPACITY as usize],
        };
        let len = config.change.len();
        let Some(change) = raw.change.get_mut(..len) else {
            bail!("Zigbee reporting change too long");
        };
        change.copy_from_slice(config.change);
        raw.change_len = len as u8;
        check(
            unsafe {
                sys::tg_zigbee_configure_reporting(
                    target.address,
                    target.endpoint,
                    target.cluster,
                    &raw,
                    id,
                )
            },
            "Zigbee configure reporting",
        )
    }
}
