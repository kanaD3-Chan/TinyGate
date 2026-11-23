//! Run the actual gateway service with simulated radio events and NVS records.
#![allow(dead_code)]

extern crate self as esp_idf_svc;

#[path = "../../../src/gateway/bus.rs"]
pub mod bus;
#[path = "../../../src/gateway/command/mod.rs"]
pub mod command;
#[path = "../../../src/gateway/device.rs"]
pub mod device;
#[path = "../../../src/gateway/event.rs"]
pub mod event;
#[path = "../../../src/gateway/access/zigbee/mod.rs"]
pub mod zigbee;
#[path = "../../../src/drivers/radio/zigbee_event.rs"]
pub mod zigbee_event;

mod gateway {
    pub use crate::{bus, command, device, event};
}

mod drivers {
    pub mod radio {
        pub use crate::zigbee_event;

        pub mod zigbee {
            pub use crate::zigbee_event::*;
            use anyhow::Result;

            #[derive(Clone, Copy)]
            pub struct Status {
                pub ready: bool,
                pub channel: u8,
                pub pan_id: u16,
                pub last_peer: u16,
                pub dropped_events: u32,
            }

            #[derive(Default)]
            pub struct Coordinator {
                pub writes: Vec<(Target, [u8; 8], bool, u32)>,
                pub reads: Vec<(Target, u16, u32)>,
                pub resolutions: Vec<u16>,
                pub onoff_error: Option<OnOffError>,
            }

            impl Coordinator {
                pub fn new() -> Result<Self> {
                    Ok(Self::default())
                }
                pub fn status(&self) -> Result<Status> {
                    anyhow::bail!("fixture supplies status")
                }
                pub fn poll(&mut self) -> Result<Option<Event>> {
                    Ok(None)
                }
                pub fn permit_join(&mut self, _: u8) -> Result<()> {
                    Ok(())
                }
                pub fn resolve(&mut self, address: u16) -> Result<()> {
                    self.resolutions.push(address);
                    Ok(())
                }
                pub fn address_for(&mut self, _: [u8; 8]) -> Result<u16> {
                    anyhow::bail!("unknown address")
                }
                pub fn discover(&mut self, _: u16) -> Result<()> {
                    Ok(())
                }
                pub fn bind(&mut self, _: Target, _: [u8; 8], _: u32) -> Result<()> {
                    Ok(())
                }
                pub fn reporting(&mut self, _: Target, _: Reporting, _: u32) -> Result<()> {
                    Ok(())
                }
                pub fn read(&mut self, target: Target, attribute: u16, id: u32) -> Result<()> {
                    self.reads.push((target, attribute, id));
                    Ok(())
                }
                pub fn onoff(
                    &mut self,
                    target: Target,
                    ieee: [u8; 8],
                    on: bool,
                    id: u32,
                ) -> std::result::Result<(), OnOffError> {
                    if let Some(error) = self.onoff_error.take() {
                        return Err(error);
                    }
                    self.writes.push((target, ieee, on, id));
                    Ok(())
                }
            }
        }
    }
}

pub mod nvs {
    use anyhow::Result;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    pub struct EspDefaultNvsPartition(Arc<Mutex<HashMap<String, Vec<u8>>>>);

    impl EspDefaultNvsPartition {
        pub fn take() -> Result<Self> {
            Ok(Self::default())
        }
    }

    pub struct EspDefaultNvs(EspDefaultNvsPartition);

    impl EspDefaultNvs {
        pub fn new(partition: EspDefaultNvsPartition, _: &str, _: bool) -> Result<Self> {
            Ok(Self(partition))
        }

        pub fn blob_len(&self, key: &str) -> Result<Option<usize>> {
            Ok(self.0.0.lock().unwrap().get(key).map(Vec::len))
        }

        pub fn get_blob<'a>(&self, key: &str, out: &'a mut [u8]) -> Result<Option<&'a [u8]>> {
            let records = self.0.0.lock().unwrap();
            let Some(bytes) = records.get(key) else {
                return Ok(None);
            };
            out.copy_from_slice(bytes);
            Ok(Some(out))
        }

        pub fn set_blob(&self, key: &str, value: &[u8]) -> Result<()> {
            self.0
                .0
                .lock()
                .unwrap()
                .insert(key.to_owned(), value.to_vec());
            Ok(())
        }
    }
}
