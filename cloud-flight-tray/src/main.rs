#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod battery;
mod drain;
mod hid;
mod icon;
mod startup;

use drain::{DrainStats, DrainTracker};
use hid::{keep_measured_percent, Connection, QueryOutcome, RawStatus};
use icon::{make_icon, IconKind};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder, TrayIconEvent};
use winit::application::ApplicationHandler;
use winit::event::StartCause;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::WindowId;

const POLL_INTERVAL: Duration = Duration::from_secs(60);
/// While charging. Set this to a day to test whether status polls power the headset off.
const CHARGING_POLL_INTERVAL: Duration = Duration::from_secs(300);
const ENUM_INTERVAL: Duration = Duration::from_secs(3);
const LINK_WAIT: Duration = Duration::from_millis(400);
const MIN_STATUS_QUERY: Duration = Duration::from_secs(1);
const CLOSED_RECHECK: Duration = Duration::from_secs(60);

#[derive(Clone, Copy)]
struct Experiment {
    no_query: bool,
    no_status: bool,
    no_power: bool,
    no_hid: bool,
    no_connect_query: bool,
    close_when_charging: bool,
}

impl Experiment {
    fn from_env() -> Self {
        Self {
            no_query: env_on("CFT_NO_QUERY"),
            no_status: env_on("CFT_NO_STATUS"),
            no_power: env_on("CFT_NO_POWER"),
            no_hid: env_on("CFT_NO_HID"),
            no_connect_query: env_on("CFT_NO_CONNECT_QUERY"),
            close_when_charging: env_on("CFT_CLOSE_WHEN_CHARGING"),
        }
    }

    fn label(&self) -> String {
        let mut on = Vec::new();
        if self.no_hid {
            on.push("NO_HID");
        }
        if self.no_query {
            on.push("NO_QUERY");
        }
        if self.no_status {
            on.push("NO_STATUS");
        }
        if self.no_power {
            on.push("NO_POWER");
        }
        if self.no_connect_query {
            on.push("NO_CONNECT_QUERY");
        }
        if self.close_when_charging {
            on.push("CLOSE_WHEN_CHARGING");
        }
        on.join(",")
    }
}

fn env_on(name: &str) -> bool {
    std::env::var(name).ok().as_deref() == Some("1")
}

fn experiment() -> &'static Experiment {
    static EXP: OnceLock<Experiment> = OnceLock::new();
    EXP.get_or_init(Experiment::from_env)
}

#[derive(Debug, Clone, Copy)]
enum PollCmd {
    Refresh,
    Quit,
}

#[derive(Debug, Clone)]
struct HeadsetUpdate {
    connection: Connection,
    charging: Option<bool>,
    percent: Option<f32>,
    raw: Option<u16>,
    drain: DrainStats,
}

enum UserEvent {
    Tray,
    Menu(MenuEvent),
    Headset(HeadsetUpdate),
}

struct StatusItems {
    battery: MenuItem,
    drain: MenuItem,
    remaining: MenuItem,
    session: MenuItem,
    status: MenuItem,
    refresh: MenuItem,
    startup: CheckMenuItem,
    quit: MenuItem,
}

struct App {
    tray: Option<TrayIcon>,
    items: Option<StatusItems>,
    cmd_tx: Sender<PollCmd>,
    last: Option<HeadsetUpdate>,
}

fn main() {
    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .expect("event loop");
    event_loop.set_control_flow(ControlFlow::Wait);

    let proxy = event_loop.create_proxy();
    TrayIconEvent::set_event_handler({
        let proxy = proxy.clone();
        Some(move |event| {
            let _ = event;
            let _ = proxy.send_event(UserEvent::Tray);
        })
    });
    MenuEvent::set_event_handler({
        let proxy = proxy.clone();
        Some(move |event| {
            let _ = proxy.send_event(UserEvent::Menu(event));
        })
    });

    let (cmd_tx, cmd_rx) = mpsc::channel();
    spawn_hid_thread(proxy, cmd_rx);

    let mut app = App {
        tray: None,
        items: None,
        cmd_tx,
        last: None,
    };

    event_loop.run_app(&mut app).expect("tray event loop");
}

impl ApplicationHandler<UserEvent> for App {
    fn new_events(&mut self, _event_loop: &ActiveEventLoop, cause: StartCause) {
        if cause != StartCause::Init || self.tray.is_some() {
            return;
        }
        startup::ensure_default_enabled();
        let items = StatusItems {
            battery: MenuItem::new("Battery: —", false, None),
            drain: MenuItem::new("Drain: —", false, None),
            remaining: MenuItem::new("Remaining: —", false, None),
            session: MenuItem::new("Session: —", false, None),
            status: MenuItem::new(decorate("Status: looking for dongle…"), false, None),
            refresh: MenuItem::new("Refresh", true, None),
            startup: CheckMenuItem::new("Start with Windows", true, startup::is_enabled(), None),
            quit: MenuItem::new("Quit", true, None),
        };
        let menu = Menu::new();
        let _ = menu.append(&items.battery);
        let _ = menu.append(&items.drain);
        let _ = menu.append(&items.remaining);
        let _ = menu.append(&items.session);
        let _ = menu.append(&items.status);
        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&items.refresh);
        let _ = menu.append(&items.startup);
        let _ = menu.append(&items.quit);

        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip(decorate("Cloud Flight — starting"))
            .with_icon(make_icon(IconKind::Disconnected))
            .build()
            .expect("tray icon");

        self.items = Some(items);
        self.tray = Some(tray);
        let _ = self.cmd_tx.send(PollCmd::Refresh);
    }

    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {}

    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        _event: winit::event::WindowEvent,
    ) {
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Headset(update) => self.apply_update(update),
            UserEvent::Menu(event) => {
                if let Some(items) = &self.items {
                    if event.id == items.quit.id() {
                        let _ = self.cmd_tx.send(PollCmd::Quit);
                        event_loop.exit();
                    } else if event.id == items.refresh.id() {
                        let _ = self.cmd_tx.send(PollCmd::Refresh);
                    } else if event.id == items.startup.id() {
                        // muda already flipped the check before this event.
                        let enabled = startup::set_enabled(items.startup.is_checked());
                        items.startup.set_checked(enabled);
                    }
                }
            }
            UserEvent::Tray => {}
        }
    }
}

impl App {
    fn apply_update(&mut self, update: HeadsetUpdate) {
        if self.last.as_ref().is_some_and(|prev| {
            prev.connection == update.connection
                && prev.charging == update.charging
                && prev.percent.map(|p| (p * 10.0) as i32)
                    == update.percent.map(|p| (p * 10.0) as i32)
                && prev.drain.percent_per_hour.map(|r| (r * 10.0) as i32)
                    == update.drain.percent_per_hour.map(|r| (r * 10.0) as i32)
                && prev.drain.remaining_secs == update.drain.remaining_secs
                && prev.drain.estimated_health_pct.map(|h| h.round() as i32)
                    == update.drain.estimated_health_pct.map(|h| h.round() as i32)
        }) {
            self.last = Some(update);
            return;
        }
        let kind = icon_kind(&update);
        let tooltip = tooltip_text(&update);
        if let Some(tray) = &self.tray {
            let _ = tray.set_icon(Some(make_icon(kind)));
            let _ = tray.set_tooltip(Some(tooltip));
        }
        if let Some(items) = &self.items {
            items.battery.set_text(battery_line(&update));
            items.drain.set_text(drain_line(&update.drain));
            items.remaining.set_text(remaining_line(&update.drain));
            items.session.set_text(session_line(&update.drain));
            items.status.set_text(status_line(&update));
        }
        self.last = Some(update);
    }
}

fn spawn_hid_thread(proxy: EventLoopProxy<UserEvent>, cmds: Receiver<PollCmd>) {
    thread::spawn(move || {
        let exp = *experiment();
        hid::log_event("start", &exp.label());
        let mut api = if exp.no_hid {
            HidApiOrNone { inner: None }
        } else {
            HidApiOrNone::new()
        };
        let mut drain = DrainTracker::load();
        let mut last_percent = drain.last_percent();
        let mut last_raw: Option<u16> = None;
        let mut link = Connection::DongleMissing;
        let mut charging = false;
        let mut power: Option<hidapi::HidDevice> = None;
        let mut status_dev: Option<hidapi::HidDevice> = None;
        let mut next_battery = Instant::now();
        let mut last_status_query: Option<Instant> = None;
        let mut last_enum: Option<Instant> = None;
        let mut closed_until: Option<Instant> = None;

        loop {
            let mut poll_battery = false;
            let mut user_refresh = false;
            let mut force_query = false;
            match cmds.try_recv() {
                Ok(PollCmd::Refresh) => user_refresh = true,
                Ok(PollCmd::Quit) => {
                    hid::log_event("quit", "cmd");
                    break;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    hid::log_event("quit", "disconnect");
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }

            if exp.close_when_charging {
                if let Some(until) = closed_until {
                    if Instant::now() < until {
                        thread::sleep(LINK_WAIT);
                        continue;
                    }
                    hid::log_event("recheck", "charging");
                    closed_until = None;
                    force_query = true;
                }
            }

            // Re-enumerating while both handles are open churns every HID device on the system.
            if !exp.no_hid
                && (power.is_none() || status_dev.is_none())
                && last_enum.is_none_or(|t| t.elapsed() >= ENUM_INTERVAL)
            {
                api.refresh();
                last_enum = Some(Instant::now());
            }
            if !exp.no_hid && !exp.no_power && power.is_none() {
                if let Some(hidapi) = api.inner.as_ref() {
                    power = hid::open_power_device(hidapi);
                }
            }
            if !exp.no_hid && !exp.no_status && status_dev.is_none() {
                if let Some(hidapi) = api.inner.as_ref() {
                    status_dev = hid::open_status_device(hidapi);
                }
            }
            if power.is_none() && status_dev.is_none() && link != Connection::DongleMissing {
                set_link(&mut link, Connection::DongleMissing);
                set_charging(&mut charging, false);
                publish(
                    &proxy,
                    &mut drain,
                    RawStatus::disconnected(),
                    &mut last_percent,
                    &mut last_raw,
                );
            }

            let mut drop_power = false;
            let mut saw_power_off = false;
            if let Some(device) = power.as_mut() {
                match hid::read_power(device, LINK_WAIT.as_millis() as i32) {
                    Ok(Some(Connection::HeadsetOff)) => {
                        set_link(&mut link, Connection::HeadsetOff);
                        set_charging(&mut charging, false);
                        saw_power_off = true;
                        publish(
                            &proxy,
                            &mut drain,
                            RawStatus {
                                connection: Connection::HeadsetOff,
                                charging: Some(false),
                                percent: last_percent,
                                raw: last_raw,
                            },
                            &mut last_percent,
                            &mut last_raw,
                        );
                    }
                    Ok(Some(Connection::Connected)) => {
                        if link != Connection::Connected {
                            poll_battery = true;
                        }
                        set_link(&mut link, Connection::Connected);
                    }
                    Ok(_) => {}
                    Err(err) => {
                        hid::log_event("drop", &format!("power read_err {err}"));
                        drop_power = true;
                    }
                }
            } else {
                thread::sleep(LINK_WAIT);
            }
            if drop_power {
                power = None;
            }

            // Off: never. While charging, only the slow interval (still needs Connected).
            let auto_due = force_query
                || (next_battery <= Instant::now()
                    && if charging {
                        link == Connection::Connected
                    } else {
                        may_auto_query(link, charging)
                    });
            if user_refresh || poll_battery || auto_due {
                let reason = if user_refresh {
                    "user_refresh"
                } else if force_query {
                    "recheck"
                } else if poll_battery {
                    "connect"
                } else {
                    "auto_due"
                };
                if !should_write(&exp, user_refresh, poll_battery, auto_due) {
                    hid::log_event("write", &format!("skip {reason}"));
                    next_battery = Instant::now() + poll_interval(charging);
                } else if last_status_query
                    .is_some_and(|started| started.elapsed() < MIN_STATUS_QUERY)
                {
                    if next_battery <= Instant::now() {
                        next_battery = Instant::now() + MIN_STATUS_QUERY;
                    }
                    continue;
                } else {
                    last_status_query = Some(Instant::now());
                    hid::log_event("write", reason);
                    let outcome = status_dev.as_mut().map(hid::query_battery);
                    match outcome {
                        Some(QueryOutcome::Timeout) => {}
                        Some(QueryOutcome::DeviceError) => {
                            hid::log_event("drop", "status device_error");
                            status_dev = None;
                            if power.is_none() && !saw_power_off {
                                set_link(&mut link, Connection::DongleMissing);
                                set_charging(&mut charging, false);
                                publish(
                                    &proxy,
                                    &mut drain,
                                    RawStatus::disconnected(),
                                    &mut last_percent,
                                    &mut last_raw,
                                );
                            }
                        }
                        Some(QueryOutcome::Status(mut reading)) => {
                            if reading.connection == Connection::DongleMissing {
                                hid::log_event("drop", "status dongle_missing");
                                status_dev = None;
                                if power.is_none() {
                                    set_link(&mut link, Connection::DongleMissing);
                                }
                            } else if saw_power_off || reading.connection == Connection::HeadsetOff
                            {
                                set_link(&mut link, Connection::HeadsetOff);
                                reading.connection = Connection::HeadsetOff;
                                reading.charging = Some(false);
                            } else if reading.connection == Connection::Connected {
                                set_link(&mut link, Connection::Connected);
                            }
                            set_charging(&mut charging, reading.charging == Some(true));
                            publish(
                                &proxy,
                                &mut drain,
                                reading,
                                &mut last_percent,
                                &mut last_raw,
                            );
                        }
                        None => {
                            let connection = connection_when_status_missing(
                                power.is_some(),
                                saw_power_off,
                                link,
                            );
                            set_link(&mut link, connection);
                            if connection != Connection::Connected {
                                set_charging(&mut charging, false);
                            }
                            let status = match connection {
                                Connection::DongleMissing => RawStatus::disconnected(),
                                Connection::HeadsetOff => RawStatus {
                                    connection,
                                    charging: Some(false),
                                    percent: last_percent,
                                    raw: last_raw,
                                },
                                Connection::Connected => RawStatus {
                                    connection,
                                    charging: None,
                                    percent: last_percent,
                                    raw: last_raw,
                                },
                            };
                            publish(&proxy, &mut drain, status, &mut last_percent, &mut last_raw);
                        }
                    }
                    next_battery = Instant::now() + poll_interval(charging);
                }
            }
            if exp.close_when_charging && charging && (power.is_some() || status_dev.is_some()) {
                hid::log_event("drop", "both close_when_charging");
                power = None;
                status_dev = None;
                closed_until = Some(Instant::now() + CLOSED_RECHECK);
            }
        }
    });
}

fn set_link(link: &mut Connection, next: Connection) {
    if *link != next {
        hid::log_event("link", &format!("{link:?}->{next:?}"));
        *link = next;
    }
}

fn set_charging(charging: &mut bool, next: bool) {
    if *charging != next {
        hid::log_event("charge", &format!("{charging}->{next}"));
        *charging = next;
    }
}

fn should_write(exp: &Experiment, user: bool, on_connect: bool, auto: bool) -> bool {
    if exp.no_hid || exp.no_query || exp.no_status {
        return false;
    }
    if exp.no_connect_query && on_connect && !user && !auto {
        return false;
    }
    user || on_connect || auto
}

fn publish(
    proxy: &EventLoopProxy<UserEvent>,
    drain: &mut DrainTracker,
    mut status: RawStatus,
    last_percent: &mut Option<f32>,
    last_raw: &mut Option<u16>,
) {
    if status.connection == Connection::Connected {
        let measured = status.percent;
        status.percent = keep_measured_percent(*last_percent, measured);
        if measured.is_some() {
            *last_percent = measured;
            *last_raw = status.raw;
        } else {
            status.raw = status.raw.or(*last_raw);
        }
    } else if status.connection == Connection::HeadsetOff {
        status.percent = status.percent.or(*last_percent);
        status.raw = status.raw.or(*last_raw);
        status.charging = Some(false);
    }

    let drain_stats = match (status.connection, status.percent, status.charging) {
        (Connection::Connected, _, Some(true)) => drain.record(0.0, true, true),
        (Connection::Connected, Some(percent), Some(false)) => drain.record(percent, false, true),
        (Connection::Connected, _, None) => drain.latest_stats(),
        _ => drain.record(0.0, false, false),
    };

    let _ = proxy.send_event(UserEvent::Headset(HeadsetUpdate {
        connection: status.connection,
        charging: status.charging,
        percent: status.percent,
        raw: status.raw,
        drain: drain_stats,
    }));
}

struct HidApiOrNone {
    inner: Option<hidapi::HidApi>,
}

impl HidApiOrNone {
    fn new() -> Self {
        Self {
            inner: hidapi::HidApi::new().ok(),
        }
    }

    fn refresh(&mut self) {
        hid::log_event("enum", "refresh");
        if let Some(api) = &mut self.inner {
            let _ = api.refresh_devices();
        } else {
            self.inner = hidapi::HidApi::new().ok();
        }
    }
}

fn icon_kind(update: &HeadsetUpdate) -> IconKind {
    let rounded = update.percent.map(|p| p.round().clamp(0.0, 100.0) as u8);
    match update.connection {
        Connection::DongleMissing => IconKind::Disconnected,
        Connection::HeadsetOff => IconKind::HeadsetOff,
        Connection::Connected if update.charging == Some(true) => {
            IconKind::Charging { percent: rounded }
        }
        Connection::Connected => match rounded {
            Some(percent) => IconKind::Battery { percent },
            None => IconKind::Disconnected,
        },
    }
}

fn tooltip_text(update: &HeadsetUpdate) -> String {
    let state = match update.connection {
        Connection::DongleMissing => "Dongle disconnected".to_string(),
        Connection::HeadsetOff => match update.percent {
            Some(p) => format!("Turned off  |  last {p:.1}%"),
            None => "Turned off".to_string(),
        },
        Connection::Connected => {
            let level = match update.charging {
                Some(true) => charging_level_text(update.percent, update.raw),
                _ => match update.percent {
                    Some(p) => format!("{p:.1}%"),
                    None => "Connected".to_string(),
                },
            };
            let drain = update
                .drain
                .percent_per_hour
                .map(|r| format!("{r:.1}%/h"))
                .unwrap_or_else(|| "—%/h".into());
            let eta = update
                .drain
                .remaining_secs
                .map(format_duration)
                .map(|s| format!("~{s} left"))
                .unwrap_or_else(|| "ETA —".into());
            return decorate(format!("Cloud Flight  {level}  |  {drain}  |  {eta}"));
        }
    };
    decorate(format!("Cloud Flight  {state}"))
}

fn charging_level_text(percent: Option<f32>, raw: Option<u16>) -> String {
    if raw.is_some_and(battery::is_full_charge) {
        return "fully charged".to_string();
    }
    match percent {
        Some(p) => format!("⚡ charging (last {p:.0}%)"),
        None => "⚡ charging".to_string(),
    }
}

fn battery_line(update: &HeadsetUpdate) -> String {
    if update.charging == Some(true) {
        let body = charging_level_text(update.percent, update.raw);
        return match update.raw {
            Some(raw) => format!("Battery: {body}  (raw {raw:#06x})"),
            None => format!("Battery: {body}"),
        };
    }
    let suffix = if update.connection == Connection::HeadsetOff {
        " (last)"
    } else {
        ""
    };
    match (update.percent, update.raw) {
        (Some(p), Some(raw)) => format!("Battery: {p:.1}%{suffix}  (raw {raw:#06x})"),
        (Some(p), None) => format!("Battery: {p:.1}%{suffix}"),
        _ => "Battery: —".into(),
    }
}

fn drain_line(drain: &DrainStats) -> String {
    match (drain.percent_per_hour, drain.estimated_health_pct) {
        (Some(r), Some(health)) => format!("Drain: {r:.2}%/h (~{health:.0}% health)"),
        (Some(r), None) => format!("Drain: {r:.2}%/h"),
        _ => "Drain: need ~10 min of use".into(),
    }
}

fn remaining_line(drain: &DrainStats) -> String {
    match drain.remaining_secs {
        Some(s) => format!("Remaining: ~{}", format_duration(s)),
        None => "Remaining: —".into(),
    }
}

fn session_line(drain: &DrainStats) -> String {
    match (drain.session_drop, drain.session_secs) {
        (Some(drop), Some(secs)) => {
            format!("Session: -{drop:.1}% in {}", format_duration(secs))
        }
        _ => "Session: —".into(),
    }
}

fn decorate(line: impl Into<String>) -> String {
    let line = line.into();
    let label = experiment().label();
    if label.is_empty() {
        line
    } else {
        format!("{line} [{label}]")
    }
}

fn status_line(update: &HeadsetUpdate) -> String {
    let line = match update.connection {
        Connection::DongleMissing => "Headset: Dongle disconnected".into(),
        Connection::HeadsetOff => "Headset: Turned off".into(),
        Connection::Connected if update.charging == Some(true) => {
            format!(
                "Headset: {}",
                charging_level_text(update.percent, update.raw)
            )
        }
        Connection::Connected => "Headset: Connected".into(),
    };
    decorate(line)
}

fn connection_when_status_missing(
    power_open: bool,
    saw_power_off: bool,
    link: Connection,
) -> Connection {
    if saw_power_off {
        Connection::HeadsetOff
    } else if power_open {
        match link {
            Connection::DongleMissing => Connection::Connected,
            other => other,
        }
    } else {
        Connection::DongleMissing
    }
}

fn may_auto_query(link: Connection, charging: bool) -> bool {
    // DongleMissing stays allowed so the first probe at startup still happens.
    link != Connection::HeadsetOff && !charging
}

fn poll_interval(charging: bool) -> Duration {
    if charging {
        CHARGING_POLL_INTERVAL
    } else {
        POLL_INTERVAL
    }
}

fn format_duration(secs: u64) -> String {
    let hours = secs / 3600;
    let mins = (secs % 3600) / 60;
    if hours > 0 {
        format!("{hours}h {mins}m")
    } else {
        format!("{mins}m")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_status_interface_is_not_headset_off() {
        assert_eq!(
            connection_when_status_missing(true, false, Connection::Connected),
            Connection::Connected
        );
        assert_ne!(
            connection_when_status_missing(true, false, Connection::Connected),
            Connection::HeadsetOff
        );
        assert_eq!(
            connection_when_status_missing(true, true, Connection::Connected),
            Connection::HeadsetOff
        );
        assert_eq!(
            connection_when_status_missing(false, false, Connection::Connected),
            Connection::DongleMissing
        );
    }

    #[test]
    fn auto_query_skips_off_and_charging() {
        assert!(!may_auto_query(Connection::HeadsetOff, false));
        assert!(!may_auto_query(Connection::Connected, true));
        assert!(may_auto_query(Connection::Connected, false));
        assert!(may_auto_query(Connection::DongleMissing, false));
        assert_eq!(poll_interval(true), CHARGING_POLL_INTERVAL);
        assert_eq!(poll_interval(false), POLL_INTERVAL);
    }

    #[test]
    fn charging_tooltip_is_not_a_live_percent() {
        let update = HeadsetUpdate {
            connection: Connection::Connected,
            charging: Some(true),
            percent: Some(100.0),
            raw: Some(0x1020),
            drain: DrainStats::default(),
        };
        let tip = tooltip_text(&update);
        assert!(tip.contains("⚡ charging (last 100%)"), "{tip}");
        let line = battery_line(&update);
        assert!(line.contains("raw 0x1020"), "{line}");
        assert!(line.contains("last 100%"), "{line}");

        let full = HeadsetUpdate {
            raw: Some(0x10C8),
            ..update
        };
        assert!(tooltip_text(&full).contains("fully charged"));
    }

    #[test]
    fn switches_gate_the_status_write() {
        let mut exp = Experiment {
            no_query: false,
            no_status: false,
            no_power: false,
            no_hid: false,
            no_connect_query: false,
            close_when_charging: false,
        };
        assert!(should_write(&exp, false, true, false));
        exp.no_connect_query = true;
        assert!(!should_write(&exp, false, true, false));
        assert!(should_write(&exp, true, true, false));
        exp.no_query = true;
        assert!(!should_write(&exp, true, false, true));
        exp.no_query = false;
        exp.no_hid = true;
        assert!(!should_write(&exp, true, true, true));
    }

    #[test]
    fn unknown_charging_is_not_confirmed() {
        let status = RawStatus {
            connection: Connection::Connected,
            charging: None,
            percent: Some(65.0),
            raw: Some(3800),
        };
        assert_ne!(status.charging, Some(true));
    }
}
