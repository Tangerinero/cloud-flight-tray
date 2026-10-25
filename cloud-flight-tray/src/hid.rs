use crate::battery::{parse_battery_bytes, BatteryReading};
use hidapi::{HidApi, HidDevice};
use std::time::Duration;

pub const KINGSTON_VID: u16 = 0x0951;
pub const HP_VID: u16 = 0x03F0;
pub const STATUS_USAGE: u16 = 771;

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

const POLL_READ_TIMEOUT: i32 = 250;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Connection {
    DongleMissing,
    HeadsetOff,
    Connected,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RawStatus {
    pub connection: Connection,
    pub charging: bool,
    pub percent: Option<f32>,
    pub raw: Option<u16>,
}

impl RawStatus {
    pub fn disconnected() -> Self {
        Self {
            connection: Connection::DongleMissing,
            charging: false,
            percent: None,
            raw: None,
        }
    }
}

pub fn is_supported(vid: u16, pid: u16) -> bool {
    KNOWN_PIDS.contains(&(vid, pid))
}

pub fn poll_once(api: &HidApi) -> RawStatus {
    let Some(mut device) = open_status_device(api) else {
        return RawStatus::disconnected();
    };

    if send_status_request(&mut device).is_err() {
        return RawStatus::disconnected();
    }

    let mut buf = [0u8; 64];
    if let Ok(n) = device.get_feature_report(&mut buf) {
        if let Some(parsed) = parse_report(&buf[..n.max(5)]) {
            return parsed;
        }
    }

    let mut buf = [0u8; 64];
    buf[0] = 0x21;
    if let Ok(n) = device.get_feature_report(&mut buf) {
        if let Some(parsed) = parse_report(&buf[..n.max(5)]) {
            return parsed;
        }
    }

    match device.read_timeout(&mut buf, POLL_READ_TIMEOUT) {
        Ok(n) if n > 0 => parse_report(&buf[..n]).unwrap_or(RawStatus {
            connection: Connection::Connected,
            charging: false,
            percent: None,
            raw: None,
        }),
        _ => RawStatus {
            connection: Connection::Connected,
            charging: false,
            percent: None,
            raw: None,
        },
    }
}

fn open_status_device(api: &HidApi) -> Option<HidDevice> {
    let mut candidates: Vec<_> = api
        .device_list()
        .filter(|info| is_supported(info.vendor_id(), info.product_id()))
        .collect();
    candidates.sort_by_key(|info| {
        if info.usage() == STATUS_USAGE {
            0
        } else {
            1
        }
    });

    for info in candidates {
        if let Ok(dev) = api.open_path(info.path()) {
            return Some(dev);
        }
        if let Ok(dev) = api.open(info.vendor_id(), info.product_id()) {
            return Some(dev);
        }
    }
    None
}

fn send_status_request(device: &mut HidDevice) -> hidapi::HidResult<()> {
    if device.send_feature_report(&GET_BATTERY).is_ok() {
        std::thread::sleep(Duration::from_millis(40));
        return Ok(());
    }
    device.write(&GET_BATTERY).map(|_| ())?;
    std::thread::sleep(Duration::from_millis(40));
    Ok(())
}

pub fn parse_report(data: &[u8]) -> Option<RawStatus> {
    if data.len() >= 2 && data[0] == 0x64 {
        return Some(match data[1] {
            0x03 => RawStatus {
                connection: Connection::HeadsetOff,
                charging: false,
                percent: None,
                raw: None,
            },
            0x01 => RawStatus {
                connection: Connection::Connected,
                charging: false,
                percent: None,
                raw: None,
            },
            _ => return None,
        });
    }

    if data.len() < 5 {
        return None;
    }

    let (upper, lower) = if data[0] == 0x21 && data[1] == 0xFF && data[2] == 0x05 {
        (data[3], data[4])
    } else {
        (data[3], data[4])
    };

    match parse_battery_bytes(upper, lower) {
        BatteryReading::Charging => Some(RawStatus {
            connection: Connection::Connected,
            charging: true,
            percent: None,
            raw: Some(((upper as u16) << 8) | lower as u16),
        }),
        BatteryReading::Percent { percent, raw } => Some(RawStatus {
            connection: Connection::Connected,
            charging: false,
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
    fn parse_battery_feature() {
        let mut pkt = [0u8; 20];
        pkt[0] = 0x21;
        pkt[1] = 0xFF;
        pkt[2] = 0x05;
        pkt[3] = 0x0E;
        pkt[4] = 200;
        let s = parse_report(&pkt).unwrap();
        assert_eq!(s.connection, Connection::Connected);
        assert!(!s.charging);
        assert!(s.percent.is_some());
    }

    #[test]
    fn parse_charging() {
        let pkt = [0x21, 0xFF, 0x05, 0x10, 0x20];
        let s = parse_report(&pkt).unwrap();
        assert!(s.charging);
        assert!(s.percent.is_none());
    }
}
