#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod battery;
mod drain;
mod hid;
mod icon;
mod startup;

use drain::{DrainStats, DrainTracker};
use hid::{Connection, RawStatus};
use icon::{make_icon, IconKind};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder, TrayIconEvent};
use winit::application::ApplicationHandler;
use winit::event::StartCause;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::WindowId;

const POLL_INTERVAL: Duration = Duration::from_secs(60);
const LINK_WAIT: Duration = Duration::from_millis(400);

#[derive(Debug, Clone, Copy)]
enum PollCmd {
    Refresh,
    Quit,
}

#[derive(Debug, Clone)]
struct HeadsetUpdate {
    connection: Connection,
    charging: bool,
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
            status: MenuItem::new("Status: looking for dongle…", false, None),
            refresh: MenuItem::new("Refresh", true, None),
            startup: CheckMenuItem::new(
                "Start with Windows",
                true,
                startup::is_enabled(),
                None,
            ),
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
            .with_tooltip("Cloud Flight — starting")
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
                        let enabled = startup::set_enabled(!items.startup.is_checked());
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
        let mut api = HidApiOrNone::new();
        let mut drain = DrainTracker::load();
        let mut last_percent: Option<f32> = None;
        let mut last_raw: Option<u16> = None;
        let mut link = Connection::DongleMissing;
        let mut power: Option<hidapi::HidDevice> = None;
        let mut status_dev: Option<hidapi::HidDevice> = None;
        let mut next_battery = std::time::Instant::now();

        loop {
            let mut poll_battery = false;
            match cmds.try_recv() {
                Ok(PollCmd::Refresh) => poll_battery = true,
                Ok(PollCmd::Quit) | Err(mpsc::TryRecvError::Disconnected) => break,
                Err(mpsc::TryRecvError::Empty) => {}
            }

            api.refresh();
            if power.is_none() {
                if let Some(hidapi) = api.inner.as_ref() {
                    power = hid::open_power_device(hidapi);
                }
            }
            if status_dev.is_none() {
                if let Some(hidapi) = api.inner.as_ref() {
                    status_dev = hid::open_status_device(hidapi);
                }
            }
            if power.is_none() && status_dev.is_none() && link != Connection::DongleMissing {
                link = Connection::DongleMissing;
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
                        link = Connection::HeadsetOff;
                        saw_power_off = true;
                        publish(
                            &proxy,
                            &mut drain,
                            RawStatus {
                                connection: Connection::HeadsetOff,
                                charging: false,
                                percent: last_percent,
                                raw: last_raw,
                            },
                            &mut last_percent,
                            &mut last_raw,
                        );
                    }
                    Ok(Some(Connection::Connected)) => {
                        link = Connection::Connected;
                        poll_battery = true;
                    }
                    Ok(_) => {}
                    Err(_) => drop_power = true,
                }
            } else {
                thread::sleep(LINK_WAIT);
            }
            if drop_power {
                power = None;
            }

            if poll_battery || next_battery <= std::time::Instant::now() {
                let mut reading = if let Some(device) = status_dev.as_mut() {
                    hid::query_battery(device)
                } else if power.is_some() || saw_power_off {
                    RawStatus {
                        connection: Connection::HeadsetOff,
                        charging: false,
                        percent: None,
                        raw: None,
                    }
                } else {
                    RawStatus::disconnected()
                };
                if reading.connection == Connection::DongleMissing {
                    status_dev = None;
                    if power.is_none() {
                        link = Connection::DongleMissing;
                    }
                } else if saw_power_off || reading.connection == Connection::HeadsetOff {
                    link = Connection::HeadsetOff;
                    reading.connection = Connection::HeadsetOff;
                    reading.charging = false;
                } else if reading.connection == Connection::Connected {
                    link = Connection::Connected;
                }
                publish(&proxy, &mut drain, reading, &mut last_percent, &mut last_raw);
                next_battery = std::time::Instant::now() + POLL_INTERVAL;
            }
        }
    });
}

fn publish(
    proxy: &EventLoopProxy<UserEvent>,
    drain: &mut DrainTracker,
    mut status: RawStatus,
    last_percent: &mut Option<f32>,
    last_raw: &mut Option<u16>,
) {
    if status.connection == Connection::Connected {
        if status.percent.is_none() {
            status.percent = *last_percent;
            status.raw = status.raw.or(*last_raw);
        } else {
            *last_percent = status.percent;
            *last_raw = status.raw;
        }
    } else if status.connection == Connection::HeadsetOff {
        status.percent = status.percent.or(*last_percent);
        status.raw = status.raw.or(*last_raw);
        status.charging = false;
    }

    let drain_stats = match (status.connection, status.percent, status.charging) {
        (Connection::Connected, Some(percent), charging) => drain.record(percent, charging, true),
        _ => drain.latest_stats(),
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
        Connection::Connected if update.charging => IconKind::Charging { percent: rounded },
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
            let level = match (update.charging, update.percent) {
                (true, Some(p)) => format!("Connected, charging  |  {p:.1}%"),
                (true, None) => "Connected, charging".to_string(),
                (false, Some(p)) => format!("Connected  |  {p:.1}%"),
                (false, None) => "Connected".to_string(),
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
            return format!("Cloud Flight  {level}  |  {drain}  |  {eta}");
        }
    };
    format!("Cloud Flight  {state}")
}

fn battery_line(update: &HeadsetUpdate) -> String {
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
    match drain.percent_per_hour {
        Some(r) => format!("Drain: {r:.2}%/h"),
        None => "Drain: need ~10 min of use".into(),
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

fn status_line(update: &HeadsetUpdate) -> String {
    match update.connection {
        Connection::DongleMissing => "Headset: Dongle disconnected".into(),
        Connection::HeadsetOff => "Headset: Turned off".into(),
        Connection::Connected if update.charging => "Headset: Connected, charging".into(),
        Connection::Connected => "Headset: Connected".into(),
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
