//! Control requests on endpoint 0.
//!
//! All requests are described as plain data ([`ControlRequest`]) so the USB
//! layer can issue them and this crate stays free of I/O.

use thiserror::Error;

/// `bmRequestType` values. Bit 7 = direction, bits 6..5 = type, bits 4..0 = recipient.
pub mod request_type {
    /// Device-to-host, vendor, recipient device.
    pub const VENDOR_IN: u8 = 0xC0;
    /// Host-to-device, vendor, recipient device.
    pub const VENDOR_OUT: u8 = 0x40;
    /// Host-to-device, class, recipient endpoint (UAC1 `SET_CUR` to an endpoint).
    pub const CLASS_OUT_ENDPOINT: u8 = 0x22;
    /// Device-to-host, class, recipient endpoint (UAC1 `GET_CUR` from an endpoint).
    pub const CLASS_IN_ENDPOINT: u8 = 0xA2;
    /// Host-to-device, standard, recipient interface.
    pub const STANDARD_OUT_INTERFACE: u8 = 0x01;
}

/// A control transfer, ready for the USB layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlRequest {
    /// `bmRequestType`.
    pub request_type: u8,
    /// `bRequest`.
    pub request: u8,
    /// `wValue`.
    pub value: u16,
    /// `wIndex`.
    pub index: u16,
    /// Payload for OUT requests, or the number of bytes expected for IN requests.
    pub data: RequestData,
}

/// Direction-specific payload of a [`ControlRequest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestData {
    /// Host-to-device payload (may be empty).
    Out(Vec<u8>),
    /// Device-to-host: number of bytes to read.
    In(u16),
}

/// Vendor request `'V'` (0x56): firmware version / channel topology. **confirmed**
/// (`PGDevice::scanFirmwareVersion`).
pub const REQ_FIRMWARE: u8 = 0x56;
/// Vendor request `'I'` (0x49): hardware status register. **family** (Ozzy, Jockey 3).
pub const REQ_STATUS: u8 = 0x49;
/// Vendor request `'B'` (0x42): sent once by `PGDevice::startStreaming` on
/// some devices. Purpose unknown; the NS7II path may not need it. **assumed**.
pub const REQ_START_B: u8 = 0x42;
/// UAC1 `SET_CUR`. **confirmed** (`PGKernelDeviceNUMARKNS7II::setFrequency`).
pub const UAC_SET_CUR: u8 = 0x01;
/// UAC1 `GET_CUR`. **family**.
pub const UAC_GET_CUR: u8 = 0x81;
/// UAC1 endpoint control selector `SAMPLING_FREQ_CONTROL` in the high byte of `wValue`.
pub const UAC_SAMPLING_FREQ_CONTROL: u16 = 0x0100;
/// Status bit the Ozzy/Jockey 3 drivers set to arm streaming. **family**.
pub const STATUS_READY: u8 = 0x20;

/// First firmware probe: `0xC0 0x56 wValue=0 wIndex=0 wLength=8`. **confirmed**.
///
/// The kext decodes the reply as
/// `version = b[0] | (b[1]_decimal_tens << 16 | b[1]_decimal_ones << 8) | b[2] << 24`
/// where `b[1]` is split into tens and ones (`b[1] / 10`, `b[1] % 10`).
pub fn firmware_version() -> ControlRequest {
    ControlRequest {
        request_type: request_type::VENDOR_IN,
        request: REQ_FIRMWARE,
        value: 0,
        index: 0,
        data: RequestData::In(8),
    }
}

/// Second firmware probe: `0xC0 0x56 wValue=0 wIndex=0 wLength=5`. **confirmed**
/// for Numark PIDs 0x26, 0x2C, 0x40, 0x70..0x79 and Alesis 0x13B2:0x0030.
///
/// The kext reads `in_channels = (b[2] & 0x7F) * 2` and
/// `out_channels = (b[3] & 0x7F) * 2` from the reply.
pub fn firmware_topology() -> ControlRequest {
    ControlRequest {
        request_type: request_type::VENDOR_IN,
        request: REQ_FIRMWARE,
        value: 0,
        index: 0,
        data: RequestData::In(5),
    }
}

/// Decoded reply of [`firmware_version`] and [`firmware_topology`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FirmwareInfo {
    /// Raw first byte (chip or board ID in Ozzy's notes).
    pub chip_id: u8,
    /// Version as the kext packs it (`Scanned Firmware Version is :%08X`).
    pub packed_version: u32,
    /// Human-readable `major.minor.patch` guess: `b[2].b[1]/10.b[1]%10`.
    pub version_string: String,
    /// Capture channel count reported by the device (0 if the 5-byte probe failed).
    pub in_channels: u8,
    /// Playback channel count reported by the device (0 if the 5-byte probe failed).
    pub out_channels: u8,
}

/// Error decoding a device reply.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum DecodeError {
    /// The reply was shorter than the request asked for.
    #[error("reply too short: got {got} bytes, need {need}")]
    TooShort {
        /// Bytes received.
        got: usize,
        /// Bytes required.
        need: usize,
    },
}

/// Decode the 8-byte reply to [`firmware_version`] the way the kext does.
pub fn decode_firmware_version(reply: &[u8]) -> Result<FirmwareInfo, DecodeError> {
    if reply.len() < 3 {
        return Err(DecodeError::TooShort { got: reply.len(), need: 3 });
    }
    let tens = reply[1] / 10;
    let ones = reply[1] % 10;
    let packed = (reply[0] as u32)
        | ((ones as u32) << 8)
        | ((tens as u32) << 16)
        | ((reply[2] as u32) << 24);
    Ok(FirmwareInfo {
        chip_id: reply[0],
        packed_version: packed,
        version_string: format!("{}.{}.{}", reply[2], tens, ones),
        in_channels: 0,
        out_channels: 0,
    })
}

/// Decode the 5-byte reply to [`firmware_topology`] into channel counts.
pub fn decode_firmware_topology(reply: &[u8]) -> Result<(u8, u8), DecodeError> {
    if reply.len() < 4 {
        return Err(DecodeError::TooShort { got: reply.len(), need: 4 });
    }
    Ok(((reply[2] & 0x7F) * 2, (reply[3] & 0x7F) * 2))
}

/// Read the status register: `0xC0 0x49 wValue=0 wIndex=idx wLength=1`. **family**.
pub fn read_status(index: u16) -> ControlRequest {
    ControlRequest {
        request_type: request_type::VENDOR_IN,
        request: REQ_STATUS,
        value: 0,
        index,
        data: RequestData::In(1),
    }
}

/// Write the status register: `0x40 0x49 wValue=byte wIndex=idx`, no data. **family**.
///
/// The official driver sign-extends the byte into `wValue`
/// (`(short)(char)byte`), which is reproduced here.
pub fn write_status(index: u16, byte: u8) -> ControlRequest {
    ControlRequest {
        request_type: request_type::VENDOR_OUT,
        request: REQ_STATUS,
        value: (byte as i8 as i16) as u16,
        index,
        data: RequestData::Out(Vec::new()),
    }
}

/// `SET_CUR SAMPLING_FREQ_CONTROL` on an endpoint, 3-byte little-endian rate.
/// **confirmed** (kext `setFrequency`: `bmRequestType 0x22, bRequest 1, wValue 0x0100,
/// wIndex = endpoint address`).
pub fn set_sample_rate(endpoint: u8, rate: u32) -> ControlRequest {
    ControlRequest {
        request_type: request_type::CLASS_OUT_ENDPOINT,
        request: UAC_SET_CUR,
        value: UAC_SAMPLING_FREQ_CONTROL,
        index: endpoint as u16,
        data: RequestData::Out(vec![
            (rate & 0xFF) as u8,
            ((rate >> 8) & 0xFF) as u8,
            ((rate >> 16) & 0xFF) as u8,
        ]),
    }
}

/// `GET_CUR SAMPLING_FREQ_CONTROL` from an endpoint. **family**.
pub fn get_sample_rate(endpoint: u8) -> ControlRequest {
    ControlRequest {
        request_type: request_type::CLASS_IN_ENDPOINT,
        request: UAC_GET_CUR,
        value: UAC_SAMPLING_FREQ_CONTROL,
        index: endpoint as u16,
        data: RequestData::In(3),
    }
}

/// Decode a 3-byte `GET_CUR` reply. The official driver also maps the
/// 16-bit truncations `0x5888 -> 88200` and `0x7700 -> 96000`.
pub fn decode_sample_rate(reply: &[u8]) -> Result<u32, DecodeError> {
    if reply.len() < 3 {
        return Err(DecodeError::TooShort { got: reply.len(), need: 3 });
    }
    let raw = (reply[0] as u32) | ((reply[1] as u32) << 8) | ((reply[2] as u32) << 16);
    Ok(match raw & 0xFFFF {
        0x5888 if raw < 0x1_0000 => 88_200,
        0x7700 if raw < 0x1_0000 => 96_000,
        _ => raw,
    })
}

/// Vendor request `'B'` (0x42): `0x40 0x42 wValue=0 wIndex=0`, no data. **assumed**.
pub fn start_b() -> ControlRequest {
    ControlRequest {
        request_type: request_type::VENDOR_OUT,
        request: REQ_START_B,
        value: 0,
        index: 0,
        data: RequestData::Out(Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn firmware_version_packing_matches_kext() {
        // b[0]=0x12 chip, b[1]=37 -> 3.7, b[2]=1 -> "1.3.7"
        let info = decode_firmware_version(&[0x12, 37, 1, 0, 0, 0, 0, 0]).unwrap();
        assert_eq!(info.version_string, "1.3.7");
        assert_eq!(info.packed_version, 0x0103_0712);
    }

    #[test]
    fn topology_doubles_masked_bytes() {
        assert_eq!(decode_firmware_topology(&[0, 0, 0x86, 0x02, 0]).unwrap(), (12, 4));
    }

    #[test]
    fn set_rate_is_three_bytes_le() {
        let r = set_sample_rate(0x05, 44_100);
        assert_eq!(r.request_type, 0x22);
        assert_eq!(r.request, 0x01);
        assert_eq!(r.value, 0x0100);
        assert_eq!(r.index, 0x0005);
        assert_eq!(r.data, RequestData::Out(vec![0x44, 0xAC, 0x00]));
    }

    #[test]
    fn get_rate_decodes_truncations() {
        assert_eq!(decode_sample_rate(&[0x44, 0xAC, 0x00]).unwrap(), 44_100);
        assert_eq!(decode_sample_rate(&[0x88, 0x58, 0x00]).unwrap(), 88_200);
        assert_eq!(decode_sample_rate(&[0x00, 0x77, 0x00]).unwrap(), 96_000);
        assert_eq!(decode_sample_rate(&[0x88, 0x58, 0x01]).unwrap(), 0x0001_5888);
    }

    #[test]
    fn status_write_sign_extends() {
        assert_eq!(write_status(0, 0x92).value, 0xFF92);
        assert_eq!(write_status(0, 0x12).value, 0x0012);
    }

    #[test]
    fn short_replies_are_errors() {
        assert!(decode_firmware_version(&[1, 2]).is_err());
        assert!(decode_sample_rate(&[1]).is_err());
    }
}
