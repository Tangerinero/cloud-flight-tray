#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod battery;
mod drain;
mod hid;
mod icon;

use drain::{DrainStats, DrainTracker};
use hid::{Connection, RawStatus};
use icon::{make_icon, IconKind};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder, TrayIconEvent};
use winit::application::ApplicationHandler;
use winit::event::StartCause;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::WindowId;

const POLL_INTERVAL: Duration = Duration::from_secs(60);

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
        let items = StatusItems {
            battery: MenuItem::new("Battery: —", false, None),
            drain: MenuItem::new("Drain: —", false, None),
            remaining: MenuItem::new("Remaining: —", false, None),
            session: MenuItem::new("Session: —", false, None),
            status: MenuItem::new("Status: looking for dongle…", false, None),
            refresh: MenuItem::new("Refresh", true, None),
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
        loop {
            api.refresh();
            let mut status = api.poll();
            if status.connection == Connection::Connected {
                if status.percent.is_none() {
                    status.percent = last_percent;
                } else {
                    last_percent = status.percent;
                }
            }

            let drain_stats = match (status.connection, status.percent, status.charging) {
                (Connection::Connected, Some(percent), charging) => {
                    drain.record(percent, charging, true)
                }
                _ => drain.latest_stats(),
            };

            let _ = proxy.send_event(UserEvent::Headset(HeadsetUpdate {
                connection: status.connection,
                charging: status.charging,
                percent: status.percent,
                raw: status.raw,
                drain: drain_stats,
            }));

            match cmds.recv_timeout(POLL_INTERVAL) {
                Ok(PollCmd::Refresh) => continue,
                Ok(PollCmd::Quit) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
            }
        }
    });
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

    fn poll(&self) -> RawStatus {
        match &self.inner {
            Some(api) => hid::poll_once(api),
            None => RawStatus::disconnected(),
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
    let battery = match (update.connection, update.charging, update.percent) {
        (Connection::DongleMissing, _, _) => "dongle missing".to_string(),
        (Connection::HeadsetOff, _, _) => "headset off".to_string(),
        (_, true, Some(p)) => format!("charging · {:.1}%", p),
        (_, true, None) => "charging".to_string(),
        (_, false, Some(p)) => format!("{:.1}%", p),
        _ => "connected".to_string(),
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
    format!("Cloud Flight  {battery}  |  {drain}  |  {eta}")
}

fn battery_line(update: &HeadsetUpdate) -> String {
    match (update.percent, update.raw) {
        (Some(p), Some(raw)) => format!("Battery: {p:.1}%  (raw {raw:#06x})"),
        (Some(p), None) => format!("Battery: {p:.1}%"),
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
        Connection::DongleMissing => "Status: dongle not found".into(),
        Connection::HeadsetOff => "Status: headset powered off".into(),
        Connection::Connected if update.charging => "Status: charging".into(),
        Connection::Connected => "Status: discharging".into(),
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
