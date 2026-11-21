//! Our versioned application protocol. It does not implement AnyDesk's wire format.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

use crate::platform::RemoteInput;

pub const VERSION: u16 = 1;
pub const MAX_DIMENSION: u32 = 8192;
pub const MAX_PIXELS: u64 = 33_554_432;
pub const MAX_INPUT_BATCH: usize = 128;
pub const MAX_JPEG_BYTES: usize = 12 * 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
pub enum ClientMessage {
    Hello {
        version: u16,
        name: String,
        request_control: bool,
    },
    Poll {
        inputs: Vec<RemoteInput>,
        release_inputs: bool,
    },
    Disconnect,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ServerMessage {
    Accepted {
        version: u16,
        control: bool,
    },
    Rejected {
        reason: String,
    },
    Frame {
        sequence: u64,
        width: u32,
        height: u32,
        jpeg: Vec<u8>,
    },
    Unchanged {
        sequence: u64,
    },
}

pub fn validate_hello(version: u16, name: &str) -> Result<()> {
    ensure!(version == VERSION, "Protocol version is not supported");
    ensure!(
        !name.trim().is_empty() && name.len() <= 80,
        "Viewer name must contain 1–80 UTF-8 bytes"
    );
    ensure!(
        !name.chars().any(char::is_control),
        "Viewer name contains control characters"
    );
    Ok(())
}

pub fn validate_dimensions(width: u32, height: u32) -> Result<usize> {
    ensure!(width > 0 && height > 0, "Frame dimensions must be nonzero");
    ensure!(
        width <= MAX_DIMENSION && height <= MAX_DIMENSION,
        "Frame dimension exceeds 8192 pixels"
    );
    let pixels = u64::from(width) * u64::from(height);
    ensure!(
        pixels <= MAX_PIXELS,
        "Frame exceeds the 32-megapixel allocation budget"
    );
    Ok(usize::try_from(
        pixels
            .checked_mul(4)
            .ok_or_else(|| anyhow::anyhow!("Frame size overflow"))?,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_incompatible_or_ambiguous_greetings() {
        assert!(validate_hello(1, "Linux viewer").is_ok());
        assert!(validate_hello(2, "Linux viewer").is_err());
        assert!(validate_hello(1, " ").is_err());
        assert!(validate_hello(1, "peer\nAllow control").is_err());
        assert!(validate_hello(1, &"a".repeat(81)).is_err());
    }

    #[test]
    fn rejects_frame_bombs_before_allocation() {
        assert_eq!(validate_dimensions(1920, 1080).unwrap(), 8_294_400);
        assert!(validate_dimensions(0, 1080).is_err());
        assert!(validate_dimensions(u32::MAX, 1080).is_err());
        assert!(validate_dimensions(8192, 8192).is_err());
    }
}
