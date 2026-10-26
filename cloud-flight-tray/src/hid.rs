use crate::battery::{parse_battery_bytes, BatteryReading};
use hidapi::{HidApi, HidDevice};
use std::time::Duration;

pub const KINGSTON_VID: u16 = 0x0951;
pub const HP_VID: u16 = 0x03F0;
pub const STATUS_USAGE: u16 = 771;
pub const POWER_USAGE_PAGE: u16 = 65472;

const KNOWN_PIDS: &[(u16, u16)] = &[
    (KINGSTON_VID, 0x16C4),
    (KINGSTON_VID, 0x1723),
    (HP_VID, 0x059B),
    (HP_VID, 0x098A),
];

const GET_BATTERY: [u8; 20] = {
    let mut pkt = [0u8; 20];
    pkt[0] = 0x21;
    pkt[1] = 0xFF;
    pkt[2] = 0x05;
    pkt
};

const POLL_READ_TIMEOUT: i32 = 500;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Connection {
    DongleMissing,
    HeadsetOff,
    Connected,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RawStatus {
    pub connection: Connection,
    /// `Some(true)` only for a charging-range status report. `None` means the report did not say.
    pub charging: Option<bool>,
    pub percent: Option<f32>,
    pub raw: Option<u16>,
}

/// Keep a measured percent. A report with no percent must not invent one, including 100%.
pub fn keep_measured_percent(previous: Option<f32>, measured: Option<f32>) -> Option<f32> {
    measured.or(previous)
}

impl RawStatus {
    pub fn disconnected() -> Self {
        Self {
            connection: Connection::DongleMissing,
            charging: None,
            percent: None,
            raw: None,
        }
    }
}

pub fn is_supported(vid: u16, pid: u16) -> bool {
    KNOWN_PIDS.contains(&(vid, pid))
}

pub fn open_status_device(api: &HidApi) -> Option<HidDevice> {
    open_matching(api, |info| info.usage() == STATUS_USAGE)
}

pub fn open_power_device(api: &HidApi) -> Option<HidDevice> {
    open_matching(api, |info| info.usage_page() == POWER_USAGE_PAGE)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum QueryOutcome {
    Status(RawStatus),
    Timeout,
    DeviceError,
}

pub fn query_battery(device: &mut HidDevice) -> QueryOutcome {
    let mut flush_buf = [0u8; 64];
    while let Ok(n) = device.read_timeout(&mut flush_buf, 0) {
        if n == 0 {
            break;
        }
    }

    if send_status_request(device).is_err() {
        return QueryOutcome::DeviceError;
    }

    let deadline = std::time::Instant::now() + Duration::from_millis(POLL_READ_TIMEOUT as u64);
    let mut buf = [0u8; 64];

    while let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) {
        let timeout_ms = (remaining.as_millis() as i32).max(1);
        match device.read_timeout(&mut buf, timeout_ms) {
            Ok(0) => break,
            Ok(n) => {
                if let Some(status) = parse_report(&buf[..n]) {
                    return QueryOutcome::Status(status);
                }
            }
            Err(_) => return QueryOutcome::DeviceError,
        }
    }

    QueryOutcome::Timeout
}

/// Power on/off arrives as a 2-byte input report on the power interface.
pub fn read_power(
    device: &mut HidDevice,
    timeout_ms: i32,
) -> hidapi::HidResult<Option<Connection>> {
    let mut buf = [0u8; 8];
    let n = device.read_timeout(&mut buf, timeout_ms)?;
    if n < 2 {
        return Ok(None);
    }
    Ok(parse_report(&buf[..n]).map(|status| status.connection))
}

fn open_matching(api: &HidApi, pred: impl Fn(&hidapi::DeviceInfo) -> bool) -> Option<HidDevice> {
    for info in api.device_list() {
        if !is_supported(info.vendor_id(), info.product_id()) || !pred(info) {
            continue;
        }
        if let Ok(dev) = api.open_path(info.path()) {
            return Some(dev);
        }
    }
    None
}

fn send_status_request(device: &mut HidDevice) -> hidapi::HidResult<()> {
    device.write(&GET_BATTERY)?;
    std::thread::sleep(Duration::from_millis(40));
    Ok(())
}

pub fn parse_report(data: &[u8]) -> Option<RawStatus> {
    if data.len() >= 2 && data[0] == 0x64 {
        return Some(match data[1] {
            0x03 => RawStatus {
                connection: Connection::HeadsetOff,
                charging: None,
                percent: None,
                raw: None,
            },
            0x01 => RawStatus {
                connection: Connection::Connected,
                charging: None,
                percent: None,
                raw: None,
            },
            _ => return None,
        });
    }

    if data.len() < 5 || data[0] != 0x21 || data[1] != 0xFF || data[2] != 0x05 {
        return None;
    }

    let (upper, lower) = (data[3], data[4]);
    // Same bytes as the status request. An echo is not a power-off or a battery level.
    if upper == 0 && lower == 0 {
        return None;
    }

    match parse_battery_bytes(upper, lower) {
        BatteryReading::Charging { raw, .. } => Some(RawStatus {
            connection: Connection::Connected,
            charging: Some(true),
            percent: None,
            raw: Some(raw),
        }),
        BatteryReading::Percent { percent, raw } => Some(RawStatus {
            connection: Connection::Connected,
            charging: Some(false),
            percent: Some(percent),
            raw: Some(raw),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_ids() {
        assert!(is_supported(0x0951, 0x16C4));
        assert!(is_supported(0x03F0, 0x059B));
        assert!(is_supported(0x03F0, 0x098A));
        assert!(!is_supported(0x1234, 0x5678));
    }

    #[test]
    fn parse_power_off() {
        let s = parse_report(&[0x64, 0x03]).unwrap();
        assert_eq!(s.connection, Connection::HeadsetOff);
    }

    #[test]
    fn zero_status_is_not_a_battery_level() {
        assert!(parse_report(&[0x21, 0xFF, 0x05, 0x00, 0x00]).is_none());
        let kept = keep_measured_percent(Some(65.0), None);
        assert_eq!(kept, Some(65.0));
    }

    #[test]
    fn unknown_header_is_ignored() {
        assert!(parse_report(&[0x01, 0x02, 0x03, 0x0E, 0xC8]).is_none());
    }

    #[test]
    fn parse_power_on() {
        let s = parse_report(&[0x64, 0x01]).unwrap();
        assert_eq!(s.connection, Connection::Connected);
        assert!(s.percent.is_none());
        assert!(s.charging.is_none());
    }

    #[test]
    fn parse_battery_feature() {
        let mut pkt = [0u8; 20];
        pkt[0] = 0x21;
        pkt[1] = 0xFF;
        pkt[2] = 0x05;
        pkt[3] = 0x0E;
        pkt[4] = 200;
        let s = parse_report(&pkt).unwrap();
        assert_eq!(s.connection, Connection::Connected);
        assert_eq!(s.charging, Some(false));
        assert!(s.percent.is_some());
        assert_ne!(s.percent, Some(100.0));
    }

    #[test]
    fn parse_charging() {
        let pkt = [0x21, 0xFF, 0x05, 0x10, 0x20];
        let s = parse_report(&pkt).unwrap();
        assert_eq!(s.charging, Some(true));
        assert!(s.percent.is_none());
        assert_eq!(s.raw, Some(0x1020));
        assert_eq!(keep_measured_percent(Some(65.0), s.percent), Some(65.0));
    }

    #[test]
    fn parse_full_charge_does_not_invent_percent() {
        let pkt = [0x21, 0xFF, 0x05, 0x10, 0xC8];
        let s = parse_report(&pkt).unwrap();
        assert_eq!(s.charging, Some(true));
        assert!(s.percent.is_none());
        assert_eq!(s.raw, Some(0x10C8));
        assert_eq!(s.connection, Connection::Connected);
        assert_eq!(keep_measured_percent(Some(65.0), s.percent), Some(65.0));
    }

    #[test]
    fn low_charging_byte_is_not_one_hundred_percent() {
        let s = parse_report(&[0x21, 0xFF, 0x05, 0x10, 0x00]).unwrap();
        assert_eq!(s.charging, Some(true));
        assert!(s.percent.is_none());
        assert_ne!(keep_measured_percent(Some(40.0), s.percent), Some(100.0));
    }
}
