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

- **Drain** is the net drop from the median of the oldest samples to the median of the newest ones, over up to 8 hours. Medians ignore the 1–2% jumps the dongle reports between polls. Until the level has fallen by about 2%, the rate stays near the pack rating of 3.33%/h (1500 mAh, about 30 hours).
- **Remaining** is `current percent / drain rate`. After a real drop it follows this session’s pace, which can be shorter or longer than 30 hours depending on volume. A fresh full charge starts near 30 hours instead of a huge number from a flat reading.
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
