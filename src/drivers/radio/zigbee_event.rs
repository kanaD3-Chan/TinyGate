//! Value copies of SDK events. Application property decoding lives in gateway access.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Target {
    pub address: u16,
    pub endpoint: u8,
    pub cluster: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Source {
    pub target: Target,
    pub ieee: [u8; 8],
    pub profile: u16,
    pub manufacturer: u16,
    pub tsn: u8,
}

pub const VALUE_CAPACITY: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Payload {
    Unavailable,
    Complete {
        bytes: [u8; VALUE_CAPACITY],
        len: u8,
    },
    Truncated {
        prefix: [u8; VALUE_CAPACITY],
    },
}

impl Payload {
    pub fn complete(&self) -> Option<&[u8]> {
        match self {
            Self::Complete { bytes, len } => bytes.get(..usize::from(*len)),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attribute {
    pub source: Source,
    pub id: u16,
    pub value_type: u8,
    pub status: i32,
    pub is_report: bool,
    pub payload: Payload,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Network,
    Join {
        address: u16,
        ieee: [u8; 8],
    },
    Leave {
        address: u16,
        ieee: [u8; 8],
    },
    Identity {
        address: u16,
        ieee: [u8; 8],
        status: i32,
    },
    Endpoint(Source),
    Attribute(Attribute),
    Transmitted {
        target: Target,
        request_id: u32,
        tsn: u8,
        status: i32,
    },
    DefaultResponse {
        source: Source,
        command: u8,
        status: i32,
    },
    ReportingConfigured {
        source: Source,
        attribute: u16,
        status: i32,
    },
    Bound {
        target: Target,
        ieee: [u8; 8],
        request_id: u32,
        status: i32,
    },
    Error {
        address: u16,
        status: i32,
    },
}

#[derive(Clone, Copy, Debug)]
pub struct Reporting {
    pub attribute: u16,
    pub value_type: u8,
    pub min_interval_s: u16,
    pub max_interval_s: u16,
    pub change: &'static [u8],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnOffError {
    RouteUnknown,
    RouteChanged,
    Busy,
    Submit(i32),
}
