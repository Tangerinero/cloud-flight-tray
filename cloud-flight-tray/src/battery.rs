//! Maps the Cloud Flight 16-bit HID status value to a finer percentage.

const THRESHOLDS: [u16; 20] = [
    3328, 3584, 3674, 3704, 3732, 3744, 3754, 3764, 3774, 3784, 3794, 3804, 3824, 3840, 3860, 3890,
    3910, 3940, 3960, 3970,
];

/// Headset hardware cuts off near 3.20 V, not at a raw value of 0.
const CUTOFF_RAW: u16 = 3200;

const PERCENTAGES: [f32; 20] = [
    5.0, 10.0, 15.0, 20.0, 25.0, 30.0, 35.0, 40.0, 45.0, 50.0, 55.0, 60.0, 65.0, 70.0, 75.0, 80.0,
    85.0, 90.0, 95.0, 100.0,
];

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BatteryReading {
    Charging { full: bool, raw: u16 },
    Percent { percent: f32, raw: u16 },
}

pub fn is_charging(upper: u8, _lower: u8) -> bool {
    // Discharge samples top out at raw 3970 (upper byte 0x0F). 0x10 and above is the charge range.
    upper >= 0x10
}

pub fn parse_battery_bytes(upper: u8, lower: u8) -> BatteryReading {
    let raw = ((upper as u16) << 8) | (lower as u16);
    if is_charging(upper, lower) {
        let full = upper == 0x10 && lower >= 200;
        return BatteryReading::Charging { full, raw };
    }
    BatteryReading::Percent {
        percent: interpolate_percent(raw),
        raw,
    }
}

pub fn interpolate_percent(raw: u16) -> f32 {
    if raw >= *THRESHOLDS.last().unwrap() {
        return 100.0;
    }
    if raw <= CUTOFF_RAW {
        return 0.0;
    }
    if raw <= THRESHOLDS[0] {
        let span = (THRESHOLDS[0] - CUTOFF_RAW) as f32;
        let t = (raw - CUTOFF_RAW) as f32 / span;
        return (PERCENTAGES[0] * t).clamp(0.0, PERCENTAGES[0]);
    }

    let idx = match THRESHOLDS.binary_search(&raw) {
        Ok(i) => return PERCENTAGES[i],
        Err(i) => i,
    };

    let lo_raw = THRESHOLDS[idx - 1] as f32;
    let hi_raw = THRESHOLDS[idx] as f32;
    let lo_pct = PERCENTAGES[idx - 1];
    let hi_pct = PERCENTAGES[idx];
    let t = (raw as f32 - lo_raw) / (hi_raw - lo_raw);
    lo_pct + t * (hi_pct - lo_pct)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cutoff_floor() {
        assert_eq!(interpolate_percent(3200), 0.0);
        assert_eq!(interpolate_percent(3000), 0.0);
        let mid = interpolate_percent(3264);
        assert!((mid - 2.5).abs() < 0.01, "got {mid}");
        assert_eq!(interpolate_percent(3328), 5.0);
    }

    #[test]
    fn exact_thresholds() {
        assert_eq!(interpolate_percent(3328), 5.0);
        assert_eq!(interpolate_percent(3970), 100.0);
        assert_eq!(interpolate_percent(3784), 50.0);
    }

    #[test]
    fn interpolates_between_points() {
        let mid = (3328 + 3584) / 2;
        let pct = interpolate_percent(mid);
        assert!((pct - 7.5).abs() < 0.01, "got {pct}");
    }

    #[test]
    fn clamps_above_full() {
        assert_eq!(interpolate_percent(u16::MAX), 100.0);
    }

    #[test]
    fn charging_detection() {
        assert!(is_charging(0x10, 20));
        assert!(is_charging(0x10, 0));
        assert!(is_charging(0x11, 0));
        assert!(!is_charging(0x0E, 200));
        assert_eq!(
            parse_battery_bytes(0x10, 0),
            BatteryReading::Charging {
                full: false,
                raw: 0x1000
            }
        );
        assert_eq!(
            parse_battery_bytes(0x10, 0x20),
            BatteryReading::Charging {
                full: false,
                raw: 0x1020
            }
        );
        assert_eq!(
            parse_battery_bytes(0x10, 200),
            BatteryReading::Charging {
                full: true,
                raw: 0x10C8
            }
        );
        match parse_battery_bytes(0x0E, 0xC8) {
            BatteryReading::Percent { percent, raw } => {
                assert_eq!(raw, 0x0EC8);
                assert!(percent > 0.0 && percent < 100.0);
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
