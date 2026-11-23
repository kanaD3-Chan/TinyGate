//! Device vocabulary shared by every access protocol and by the uplink.
//!
//! Shapes align with `contracts/schemas` v1: snake_case property names,
//! string endpoints, bool/number values. Wire-format concerns (`event_id`,
//! `seq`, JSON encoding) belong to the event and uplink milestones.

/// Stable Zigbee identity in SDK byte order. Short addresses are only routes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct DeviceId([u8; 8]);

impl DeviceId {
    pub fn from_ieee(ieee: [u8; 8]) -> Option<Self> {
        (ieee != [0; 8] && ieee != [0xff; 8]).then_some(Self(ieee))
    }

    pub fn ieee(self) -> [u8; 8] {
        self.0
    }
}

impl std::fmt::Display for DeviceId {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.write_str("zb_")?;
        for byte in self.0.iter().rev() {
            write!(out, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl std::str::FromStr for DeviceId {
    type Err = &'static str;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        const INVALID: &str = "invalid Zigbee device ID";
        let hex = text
            .strip_prefix("zb_")
            .filter(|hex| hex.len() == 16 && hex.is_ascii())
            .ok_or(INVALID)?;
        let mut ieee = [0; 8];
        for (index, byte) in ieee.iter_mut().rev().enumerate() {
            *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).map_err(|_| INVALID)?;
        }
        Self::from_ieee(ieee).ok_or(INVALID)
    }
}

/// Access protocol of a device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    Zigbee,
    Ble,
}

/// Known observable properties (contracts: `temperature` degC, `power` bool).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Property {
    Temperature,
    Power,
}

impl Property {
    /// Contract field name (`snake_case`, stable).
    pub fn name(&self) -> &'static str {
        match self {
            Property::Temperature => "temperature",
            Property::Power => "power",
        }
    }
}

/// A property value; contracts allow `boolean|number|string`, never null.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Value {
    Bool(bool),
    Number(f32),
    Text(&'static str),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn property_names_match_the_contract() {
        assert_eq!(Property::Temperature.name(), "temperature");
        assert_eq!(Property::Power.name(), "power");
    }

    #[test]
    fn device_id_round_trips_in_canonical_ieee_order() {
        let device = DeviceId::from_ieee([8, 7, 6, 5, 4, 3, 2, 1]).unwrap();
        assert_eq!(device.to_string(), "zb_0102030405060708");
        assert_eq!(device.to_string().parse(), Ok(device));
        assert!("zb_ffffffffffffffff".parse::<DeviceId>().is_err());
    }
}
