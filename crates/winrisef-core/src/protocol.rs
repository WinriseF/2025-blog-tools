pub const PROTOCOL_VERSION: u16 = 1;
pub const HELLO_LEN: usize = 48;
pub const ACK_LEN: usize = 32;
pub const LANE_HEADER_LEN: usize = 32;
pub const EXTENT_HEADER_LEN: usize = 16;
pub const RESULT_LEN: usize = 32;

const HELLO_MAGIC: [u8; 8] = *b"WRNFHEL1";
const ACK_MAGIC: [u8; 8] = *b"WRNFACK1";
const LANE_MAGIC: [u8; 8] = *b"WRNFLAN1";
const RESULT_MAGIC: [u8; 8] = *b"WRNFDON1";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum TransferDirection {
    BrowserToAgentMemory = 1,
    AgentToBrowserMemory = 2,
}

impl TryFrom<u8> for TransferDirection {
    type Error = ProtocolError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::BrowserToAgentMemory),
            2 => Ok(Self::AgentToBrowserMemory),
            _ => Err(ProtocolError::UnknownDirection(value)),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Hello {
    pub direction: TransferDirection,
    pub lanes: u8,
    pub block_size: u32,
    pub extent_size: u64,
    pub total_size: u64,
    pub token: [u8; 16],
}

impl Hello {
    pub fn encode(self) -> [u8; HELLO_LEN] {
        let mut bytes = [0; HELLO_LEN];
        bytes[0..8].copy_from_slice(&HELLO_MAGIC);
        bytes[8..10].copy_from_slice(&PROTOCOL_VERSION.to_be_bytes());
        bytes[10] = self.direction as u8;
        bytes[11] = self.lanes;
        bytes[12..16].copy_from_slice(&self.block_size.to_be_bytes());
        bytes[16..24].copy_from_slice(&self.extent_size.to_be_bytes());
        bytes[24..32].copy_from_slice(&self.total_size.to_be_bytes());
        bytes[32..48].copy_from_slice(&self.token);
        bytes
    }

    pub fn decode(bytes: [u8; HELLO_LEN]) -> Result<Self, ProtocolError> {
        check_magic(&bytes[0..8], &HELLO_MAGIC)?;
        check_version(u16::from_be_bytes([bytes[8], bytes[9]]))?;
        let hello = Self {
            direction: TransferDirection::try_from(bytes[10])?,
            lanes: bytes[11],
            block_size: u32::from_be_bytes(bytes[12..16].try_into().expect("fixed range")),
            extent_size: u64::from_be_bytes(bytes[16..24].try_into().expect("fixed range")),
            total_size: u64::from_be_bytes(bytes[24..32].try_into().expect("fixed range")),
            token: bytes[32..48].try_into().expect("fixed range"),
        };
        hello.validate()?;
        Ok(hello)
    }

    pub fn validate(self) -> Result<(), ProtocolError> {
        if self.lanes == 0 || self.block_size == 0 || self.extent_size == 0 || self.total_size == 0
        {
            return Err(ProtocolError::InvalidHello);
        }
        if !self.extent_size.is_multiple_of(u64::from(self.block_size)) {
            return Err(ProtocolError::InvalidHello);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum AckStatus {
    Accepted = 0,
    AuthenticationFailed = 1,
    InvalidConfiguration = 2,
    Busy = 3,
    TransferFailed = 4,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HelloAck {
    pub status: AckStatus,
    pub lanes: u8,
    pub block_size: u32,
    pub extent_size: u64,
    pub total_size: u64,
}

impl HelloAck {
    pub fn encode(self) -> [u8; ACK_LEN] {
        let mut bytes = [0; ACK_LEN];
        bytes[0..8].copy_from_slice(&ACK_MAGIC);
        bytes[8..10].copy_from_slice(&PROTOCOL_VERSION.to_be_bytes());
        bytes[10] = self.status as u8;
        bytes[11] = self.lanes;
        bytes[12..16].copy_from_slice(&self.block_size.to_be_bytes());
        bytes[16..24].copy_from_slice(&self.extent_size.to_be_bytes());
        bytes[24..32].copy_from_slice(&self.total_size.to_be_bytes());
        bytes
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LaneHeader {
    pub lane_id: u16,
    pub lane_count: u16,
    pub total_size: u64,
    pub extent_size: u64,
}

impl LaneHeader {
    pub fn encode(self) -> [u8; LANE_HEADER_LEN] {
        let mut bytes = [0; LANE_HEADER_LEN];
        bytes[0..8].copy_from_slice(&LANE_MAGIC);
        bytes[8..10].copy_from_slice(&PROTOCOL_VERSION.to_be_bytes());
        bytes[10..12].copy_from_slice(&self.lane_id.to_be_bytes());
        bytes[12..14].copy_from_slice(&self.lane_count.to_be_bytes());
        bytes[16..24].copy_from_slice(&self.total_size.to_be_bytes());
        bytes[24..32].copy_from_slice(&self.extent_size.to_be_bytes());
        bytes
    }

    pub fn decode(bytes: [u8; LANE_HEADER_LEN]) -> Result<Self, ProtocolError> {
        check_magic(&bytes[0..8], &LANE_MAGIC)?;
        check_version(u16::from_be_bytes([bytes[8], bytes[9]]))?;
        let header = Self {
            lane_id: u16::from_be_bytes([bytes[10], bytes[11]]),
            lane_count: u16::from_be_bytes([bytes[12], bytes[13]]),
            total_size: u64::from_be_bytes(bytes[16..24].try_into().expect("fixed range")),
            extent_size: u64::from_be_bytes(bytes[24..32].try_into().expect("fixed range")),
        };
        if header.lane_count == 0 || header.lane_id >= header.lane_count {
            return Err(ProtocolError::InvalidLane);
        }
        Ok(header)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExtentHeader {
    pub offset: u64,
    pub len: u64,
}

impl ExtentHeader {
    pub const END: Self = Self { offset: 0, len: 0 };

    pub fn encode(self) -> [u8; EXTENT_HEADER_LEN] {
        let mut bytes = [0; EXTENT_HEADER_LEN];
        bytes[0..8].copy_from_slice(&self.offset.to_be_bytes());
        bytes[8..16].copy_from_slice(&self.len.to_be_bytes());
        bytes
    }

    pub fn decode(bytes: [u8; EXTENT_HEADER_LEN]) -> Result<Self, ProtocolError> {
        let header = Self {
            offset: u64::from_be_bytes(bytes[0..8].try_into().expect("fixed range")),
            len: u64::from_be_bytes(bytes[8..16].try_into().expect("fixed range")),
        };
        if header == Self::END {
            return Ok(header);
        }
        if header.len == 0 || header.offset.checked_add(header.len).is_none() {
            return Err(ProtocolError::InvalidExtent);
        }
        Ok(header)
    }

    pub const fn is_end(self) -> bool {
        self.offset == 0 && self.len == 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransferResult {
    pub status: AckStatus,
    pub bytes: u64,
    pub elapsed_nanos: u64,
}

impl TransferResult {
    pub fn encode(self) -> [u8; RESULT_LEN] {
        let mut bytes = [0; RESULT_LEN];
        bytes[0..8].copy_from_slice(&RESULT_MAGIC);
        bytes[8..10].copy_from_slice(&PROTOCOL_VERSION.to_be_bytes());
        bytes[10] = self.status as u8;
        bytes[12..20].copy_from_slice(&self.bytes.to_be_bytes());
        bytes[20..28].copy_from_slice(&self.elapsed_nanos.to_be_bytes());
        bytes
    }
}

fn check_magic(actual: &[u8], expected: &[u8; 8]) -> Result<(), ProtocolError> {
    if actual == expected {
        Ok(())
    } else {
        Err(ProtocolError::InvalidMagic)
    }
}

fn check_version(version: u16) -> Result<(), ProtocolError> {
    if version == PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(ProtocolError::UnsupportedVersion(version))
    }
}

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum ProtocolError {
    #[error("invalid protocol magic")]
    InvalidMagic,
    #[error("unsupported protocol version {0}")]
    UnsupportedVersion(u16),
    #[error("unknown transfer direction {0}")]
    UnknownDirection(u8),
    #[error("invalid hello parameters")]
    InvalidHello,
    #[error("invalid lane header")]
    InvalidLane,
    #[error("invalid extent header")]
    InvalidExtent,
}

#[cfg(test)]
mod tests {
    use super::{Hello, LaneHeader, TransferDirection};

    #[test]
    fn hello_round_trip_is_stable() {
        let hello = Hello {
            direction: TransferDirection::AgentToBrowserMemory,
            lanes: 4,
            block_size: 4 * 1024 * 1024,
            extent_size: 64 * 1024 * 1024,
            total_size: 50 * 1024 * 1024 * 1024,
            token: [7; 16],
        };
        assert_eq!(Hello::decode(hello.encode()).unwrap(), hello);
    }

    #[test]
    fn lane_round_trip_is_stable() {
        let header = LaneHeader {
            lane_id: 3,
            lane_count: 4,
            total_size: 1024,
            extent_size: 256,
        };
        assert_eq!(LaneHeader::decode(header.encode()).unwrap(), header);
    }
}
