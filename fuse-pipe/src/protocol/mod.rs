//! Wire protocol for FUSE-over-pipe/socket communication.
//!
//! This module provides the serialization format and types for
//! communication between FUSE clients and filesystem servers.
//!
//! # Architecture
//!
//! The protocol supports a single multiplexed connection where multiple
//! FUSE reader threads can share one socket. Each request includes a
//! `reader_id` that identifies which thread sent the request, allowing
//! responses to be routed back correctly.
//!
//! # Frame Format
//!
//! ```text
//! +----------+---------+
//! |  length  | payload |
//! | (4 bytes)| (N bytes)|
//! +----------+---------+
//! ```
//!
//! Messages are length-prefixed with a big-endian u32, followed by
//! a bincode-serialized `WireRequest` or `WireResponse`.

mod request;
mod response;
mod types;
mod wire;

pub use request::VolumeRequest;
pub use response::VolumeResponse;
pub use types::{file_type, DirEntry, DirEntryPlus, FileAttr};
pub use wire::{
    now_nanos, read_message, read_message_async, write_message, write_message_async, Span,
    WireRequest, WireResponse, MAX_MESSAGE_SIZE,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_roundtrip() {
        // Test a complete request/response roundtrip
        let req = WireRequest::new(
            1,
            0,
            VolumeRequest::Read {
                ino: 2,
                fh: 3,
                offset: 0,
                size: 4096,
                uid: 0,
                gid: 0,
                pid: 0,
            },
        );

        let encoded = req.encode().unwrap();
        let decoded = WireRequest::decode(&encoded[4..]).unwrap();

        assert_eq!(req.unique, decoded.unique);
        assert_eq!(req.reader_id, decoded.reader_id);

        let resp = WireResponse::new(
            1,
            0,
            VolumeResponse::Data {
                data: vec![1, 2, 3, 4],
            },
        );

        let encoded = resp.encode().unwrap();
        let decoded = WireResponse::decode(&encoded[4..]).unwrap();

        assert_eq!(resp.unique, decoded.unique);
        assert_eq!(resp.response.data(), decoded.response.data());
    }
}
