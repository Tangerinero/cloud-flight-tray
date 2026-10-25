# Cloud Flight Tray

Windows system-tray app that reads the battery of a HyperX Cloud Flight headset from its USB dongle.

Hover the tray icon for a summary. Right-click for battery, drain rate, estimated time remaining, session drop, connection state, **Refresh**, and **Quit**. The release build has no console window.

## Run

Use the release binary:

```text
cloud-flight-tray\target\release\cloud-flight-tray.exe
```

Plug in the USB receiver and power the headset on. The app polls about once a minute and again when you choose **Refresh**.

## What the numbers mean

The dongle does not report milliamp-hours or a fuel-gauge percentage. It returns a status value, which this app maps onto a percent and interpolates between the known 5% steps so the tooltip can show a tenth of a percent (for example `97.5%`).

Drain and time remaining come from how that percent changes while the headset is on and not charging:

- **Drain** is the fitted drop over roughly the last 90 minutes, in percent per hour. It stays blank until there are at least 10 minutes of discharging samples, so a short noisy stretch does not invent a huge rate.
- **Remaining** is `current percent / drain rate`. It assumes that recent rate continues. It does not use the 1500 mAh pack size or HyperX’s “up to 30 hours” rating. Near a full charge the status value moves slowly, so the estimate can read longer than 30 hours. It gets more meaningful after a longer stretch of real use.
- **Session** is how much the percent fell since this run started.

Samples are stored in `%LOCALAPPDATA%\cloud-flight-tray\history.json` and kept for about 24 hours. Charging and disconnect gaps are left out of the slope.

## Supported receivers

| Vendor | Vendor ID | Product IDs |
| --- | --- | --- |
| Kingston / HyperX | `0x0951` | `0x16C4`, `0x1723` |
| HP | `0x03F0` | `0x059B`, `0x098A` |

The app opens the status interface (HID usage `771`) and sends `[0x21, 0xFF, 0x05, ...]`. On the Cloud Flight dongle the reply arrives as an input report, not a feature report.

## Build

Requires the MSVC Rust toolchain on Windows.

```powershell
cd cloud-flight-tray
cargo build --release
```

Debug builds keep a console. Release builds use the Windows subsystem and strip symbols.

## License

MIT
