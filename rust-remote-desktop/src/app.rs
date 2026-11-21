//! Native host and viewer UI. Network, capture and encoding run off the UI thread.

use std::{
    collections::VecDeque,
    net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use eframe::egui::{
    self, Key, Modifiers, PointerButton as EguiButton, Sense, TextureHandle, TextureOptions, Vec2,
};

use crate::{
    platform::{CapturedFrame, Desktop, PointerButton, RemoteInput, RemoteKey},
    protocol::{ClientMessage, MAX_INPUT_BATCH, ServerMessage, VERSION},
    session::{self, Permission, Request},
    transport,
};

type SocketHandle = Arc<Mutex<Option<TcpStream>>>;
type LatestFrame = Arc<Mutex<Option<CapturedFrame>>>;
type ShortcutQueue = Arc<Mutex<Option<RemoteShortcut>>>;

#[derive(Clone, Copy)]
enum RemoteShortcut {
    Copy,
    Cut,
    Paste,
    SystemMenu,
}

impl RemoteShortcut {
    fn inputs(self) -> Vec<RemoteInput> {
        let tap = |key| {
            [
                RemoteInput::Key { key, pressed: true },
                RemoteInput::Key {
                    key,
                    pressed: false,
                },
            ]
        };
        match self {
            Self::SystemMenu => tap(RemoteKey::Meta).to_vec(),
            _ => {
                let key = match self {
                    Self::Copy => RemoteKey::C,
                    Self::Cut => RemoteKey::X,
                    Self::Paste => RemoteKey::V,
                    Self::SystemMenu => unreachable!(),
                };
                let mut inputs = vec![RemoteInput::Key {
                    key: RemoteKey::Control,
                    pressed: true,
                }];
                inputs.extend(tap(key));
                inputs.push(RemoteInput::Key {
                    key: RemoteKey::Control,
                    pressed: false,
                });
                inputs
            }
        }
    }
}

struct Worker {
    stop: Arc<AtomicBool>,
    socket: SocketHandle,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Ok(socket) = self.socket.lock()
            && let Some(stream) = socket.as_ref()
        {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }

    fn finished(&self) -> bool {
        self.thread.as_ref().is_none_or(JoinHandle::is_finished)
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

enum Event {
    HostStatus(String),
    HostRequest {
        peer: String,
        request: Request,
        answer: SyncSender<Permission>,
    },
    HostRequestClosed,
    ViewerStatus(String),
    ViewerConnected {
        control: bool,
    },
    ViewerEnded,
}

struct PendingRequest {
    peer: String,
    request: Request,
    answer: SyncSender<Permission>,
}

pub struct RemoteDesktopApp {
    events_tx: mpsc::Sender<Event>,
    events_rx: Receiver<Event>,
    host: Option<Worker>,
    host_bind: String,
    host_relay: bool,
    host_secret: String,
    host_status: String,
    pending: Option<PendingRequest>,
    viewer: Option<Worker>,
    viewer_address: String,
    viewer_relay: bool,
    viewer_secret: String,
    viewer_name: String,
    viewer_status: String,
    request_control: bool,
    control_granted: bool,
    remote_focused: bool,
    input_tx: Option<SyncSender<RemoteInput>>,
    release_inputs: Arc<AtomicBool>,
    shortcut: ShortcutQueue,
    latest_frame: LatestFrame,
    texture: Option<TextureHandle>,
    dimensions: [u32; 2],
    modifiers: Modifiers,
    log: VecDeque<String>,
    close_at: Option<Instant>,
}

impl RemoteDesktopApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let (events_tx, events_rx) = mpsc::channel();
        Self {
            events_tx,
            events_rx,
            host: None,
            host_bind: "127.0.0.1:47832".into(),
            host_relay: false,
            host_secret: String::new(),
            host_status: "Stopped".into(),
            pending: None,
            viewer: None,
            viewer_address: "127.0.0.1:47832".into(),
            viewer_relay: false,
            viewer_secret: String::new(),
            viewer_name: "Remote viewer".into(),
            viewer_status: "Disconnected".into(),
            request_control: false,
            control_granted: false,
            remote_focused: false,
            input_tx: None,
            release_inputs: Arc::new(AtomicBool::new(false)),
            shortcut: Arc::new(Mutex::new(None)),
            latest_frame: Arc::new(Mutex::new(None)),
            texture: None,
            dimensions: [0, 0],
            modifiers: Modifiers::NONE,
            log: VecDeque::new(),
            close_at: None,
        }
    }

    pub fn close_after(&mut self, duration: Duration) {
        self.close_at = Some(Instant::now() + duration);
    }

    fn record(&mut self, message: String) {
        if self.log.len() == 32 {
            self.log.pop_front();
        }
        self.log.push_back(message);
    }

    fn drain_events(&mut self) {
        while let Ok(event) = self.events_rx.try_recv() {
            match event {
                Event::HostStatus(message) => {
                    self.host_status = message.clone();
                    self.record(message);
                }
                Event::HostRequest {
                    peer,
                    request,
                    answer,
                } => {
                    self.pending = Some(PendingRequest {
                        peer,
                        request,
                        answer,
                    });
                }
                Event::HostRequestClosed => self.pending = None,
                Event::ViewerStatus(message) => {
                    self.viewer_status = message.clone();
                    self.record(message);
                }
                Event::ViewerConnected { control } => {
                    self.control_granted = control;
                    self.viewer_status = if control {
                        "Connected · control allowed"
                    } else {
                        "Connected · view only"
                    }
                    .into();
                }
                Event::ViewerEnded => {
                    self.control_granted = false;
                    self.remote_focused = false;
                    self.input_tx = None;
                }
            }
        }
        if self.host.as_ref().is_some_and(Worker::finished) {
            self.host = None;
            self.host_secret.clear();
        }
        if self.viewer.as_ref().is_some_and(Worker::finished) {
            self.viewer = None;
        }
    }

    fn start_host(&mut self, ctx: &egui::Context) {
        if self.host_relay {
            self.start_relay_host(ctx);
            return;
        }
        let result = (|| -> Result<Worker> {
            let address: SocketAddr = self
                .host_bind
                .parse()
                .context("Enter a numeric listen address, such as 0.0.0.0:47832")?;
            let listener =
                TcpListener::bind(address).context("Unable to bind the listen address")?;
            listener.set_nonblocking(true)?;
            let secret = transport::new_pairing_secret()?;
            self.host_secret = secret.clone();
            let stop = Arc::new(AtomicBool::new(false));
            let socket = Arc::new(Mutex::new(None));
            let thread_stop = stop.clone();
            let thread_socket = socket.clone();
            let events = self.events_tx.clone();
            let context = ctx.clone();
            let thread = thread::spawn(move || {
                notify(
                    &events,
                    &context,
                    Event::HostStatus(format!(
                        "Listening on {address}; every session needs your approval"
                    )),
                );
                while !thread_stop.load(Ordering::Acquire) {
                    let accepted = match listener.accept() {
                        Ok(connection) => connection,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(25));
                            continue;
                        }
                        Err(error) => {
                            notify(
                                &events,
                                &context,
                                Event::HostStatus(format!("Listener stopped: {error}")),
                            );
                            break;
                        }
                    };
                    if let Ok(clone) = accepted.0.try_clone()
                        && let Ok(mut handle) = thread_socket.lock()
                    {
                        *handle = Some(clone);
                    }
                    let peer = accepted.1.to_string();
                    let result = host_connection(
                        accepted.0,
                        &secret,
                        &peer,
                        &thread_stop,
                        &events,
                        &context,
                    );
                    if let Ok(mut handle) = thread_socket.lock() {
                        *handle = None;
                    }
                    notify(&events, &context, Event::HostRequestClosed);
                    if thread_stop.load(Ordering::Acquire) {
                        break;
                    }
                    match result {
                        Ok(()) => notify(
                            &events,
                            &context,
                            Event::HostStatus(format!("Session ended; listening on {address}")),
                        ),
                        Err(error) => notify(
                            &events,
                            &context,
                            Event::HostStatus(format!("Connection ended: {error:#}")),
                        ),
                    }
                }
                notify(&events, &context, Event::HostStatus("Host stopped".into()));
            });
            Ok(Worker {
                stop,
                socket,
                thread: Some(thread),
            })
        })();
        match result {
            Ok(worker) => self.host = Some(worker),
            Err(error) => self.host_status = format!("{error:#}"),
        }
    }

    fn start_relay_host(&mut self, ctx: &egui::Context) {
        let secret = match transport::new_pairing_secret() {
            Ok(secret) => secret,
            Err(error) => {
                self.host_status = format!("{error:#}");
                return;
            }
        };
        self.host_secret = secret.clone();
        let address = self.host_bind.trim().to_owned();
        let stop = Arc::new(AtomicBool::new(false));
        let socket = Arc::new(Mutex::new(None));
        let thread_stop = stop.clone();
        let thread_socket = socket.clone();
        let events = self.events_tx.clone();
        let context = ctx.clone();
        let thread = thread::spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                notify(
                    &events,
                    &context,
                    Event::HostStatus(format!("Waiting for a viewer through relay {address}")),
                );
                let result = (|| -> Result<()> {
                    let stream = connect_stream(&address, &thread_stop)?;
                    *thread_socket
                        .lock()
                        .map_err(|_| anyhow::anyhow!("Socket lock was poisoned"))? =
                        Some(stream.try_clone()?);
                    let stream = crate::relay::join(stream, crate::relay::Role::Host, &secret)?;
                    host_connection(
                        stream,
                        &secret,
                        &format!("relayed through {address}"),
                        &thread_stop,
                        &events,
                        &context,
                    )
                })();
                if let Ok(mut handle) = thread_socket.lock() {
                    *handle = None;
                }
                notify(&events, &context, Event::HostRequestClosed);
                if let Err(error) = result {
                    if !thread_stop.load(Ordering::Acquire) {
                        notify(
                            &events,
                            &context,
                            Event::HostStatus(format!(
                                "Relay session ended: {error:#}; start sharing again to retry"
                            )),
                        );
                    }
                    return;
                }
            }
            notify(&events, &context, Event::HostStatus("Host stopped".into()));
        });
        self.host = Some(Worker {
            stop,
            socket,
            thread: Some(thread),
        });
    }

    fn connect_viewer(&mut self, ctx: &egui::Context) {
        self.texture = None;
        self.dimensions = [0, 0];
        self.control_granted = false;
        self.release_inputs.store(false, Ordering::Release);
        self.modifiers = Modifiers::NONE;
        if let Ok(mut shortcut) = self.shortcut.lock() {
            *shortcut = None;
        }
        if let Ok(mut frame) = self.latest_frame.lock() {
            *frame = None;
        }
        let address = self.viewer_address.trim().to_owned();
        let secret = self.viewer_secret.trim().to_owned();
        let name = self.viewer_name.trim().to_owned();
        let request_control = self.request_control;
        let use_relay = self.viewer_relay;
        let (input_tx, input_rx) = mpsc::sync_channel(256);
        self.input_tx = Some(input_tx);
        let stop = Arc::new(AtomicBool::new(false));
        let socket = Arc::new(Mutex::new(None));
        let thread_stop = stop.clone();
        let thread_socket = socket.clone();
        let events = self.events_tx.clone();
        let release = self.release_inputs.clone();
        let frames = self.latest_frame.clone();
        let shortcut = self.shortcut.clone();
        let context = ctx.clone();
        let thread = thread::spawn(move || {
            notify(
                &events,
                &context,
                Event::ViewerStatus("Connecting and authenticating…".into()),
            );
            let result = viewer_connection(
                &address,
                &secret,
                &name,
                request_control,
                use_relay,
                &thread_stop,
                &thread_socket,
                input_rx,
                release,
                shortcut,
                frames,
                &events,
                &context,
            );
            let message = match result {
                Ok(()) => "Disconnected".into(),
                Err(error) => format!("Connection ended: {error:#}"),
            };
            notify(&events, &context, Event::ViewerStatus(message));
            notify(&events, &context, Event::ViewerEnded);
        });
        self.viewer = Some(Worker {
            stop,
            socket,
            thread: Some(thread),
        });
    }

    fn send_input(&self, input: RemoteInput) {
        if let Some(sender) = &self.input_tx
            && sender.try_send(input).is_err()
        {
            // On overflow, release state instead of risking a dropped key-up event.
            self.release_inputs.store(true, Ordering::Release);
        }
    }

    fn release_control(&mut self) {
        self.release_inputs.store(true, Ordering::Release);
        self.remote_focused = false;
        self.modifiers = Modifiers::NONE;
    }

    fn submit_shortcut(&mut self, command: RemoteShortcut) {
        if let Ok(mut pending) = self.shortcut.lock() {
            *pending = Some(command);
        }
        self.release_control();
    }

    fn connection_controls(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.heading("Rust Remote Desktop");
        ui.label("Windows and Linux · encrypted, attended sessions");
        ui.add_space(12.0);
        ui.columns(2, |columns| {
            columns[0].group(|ui| {
                ui.heading("Share this computer");
                ui.add_enabled_ui(self.host.is_none(), |ui| {
                    if ui
                        .checkbox(&mut self.host_relay, "Use a self-hosted relay")
                        .changed()
                    {
                        self.host_bind = if self.host_relay {
                            "127.0.0.1:47833"
                        } else {
                            "127.0.0.1:47832"
                        }
                        .into();
                    }
                });
                ui.label(if self.host_relay {
                    "Relay server address"
                } else {
                    "Listen address"
                });
                ui.add_enabled(
                    self.host.is_none(),
                    egui::TextEdit::singleline(&mut self.host_bind).desired_width(f32::INFINITY),
                );
                ui.small(if self.host_relay {
                    "Both computers connect outward to the same relay."
                } else {
                    "Use your LAN IP or 0.0.0.0:47832 to accept a connection from another computer."
                });
                ui.horizontal(|ui| {
                    if self.host.is_none() {
                        if ui.button("Start sharing").clicked() {
                            self.start_host(ctx);
                        }
                    } else if ui.button("Stop sharing and disconnect").clicked()
                        && let Some(host) = &mut self.host
                    {
                        host.stop();
                    }
                });
                if !self.host_secret.is_empty() {
                    ui.label("Pairing secret · share privately with your viewer");
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.host_secret)
                                .interactive(false)
                                .desired_width(ui.available_width() - 55.0),
                        );
                        if ui.button("Copy").clicked() {
                            ctx.copy_text(self.host_secret.clone());
                        }
                    });
                    ui.small("Stopping and restarting sharing generates a new secret.");
                }
                ui.label(&self.host_status);
            });
            columns[1].group(|ui| {
                ui.heading("Connect to a computer");
                ui.add_enabled_ui(self.viewer.is_none(), |ui| {
                    if ui
                        .checkbox(&mut self.viewer_relay, "Use a self-hosted relay")
                        .changed()
                    {
                        self.viewer_address = if self.viewer_relay {
                            "127.0.0.1:47833"
                        } else {
                            "127.0.0.1:47832"
                        }
                        .into();
                    }
                    ui.label(if self.viewer_relay {
                        "Relay server address"
                    } else {
                        "Host IP address and port"
                    });
                    ui.add(
                        egui::TextEdit::singleline(&mut self.viewer_address)
                            .desired_width(f32::INFINITY),
                    );
                    ui.label("Pairing secret");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.viewer_secret)
                            .password(true)
                            .desired_width(f32::INFINITY),
                    );
                    ui.horizontal(|ui| {
                        ui.label("Your name");
                        ui.text_edit_singleline(&mut self.viewer_name);
                    });
                    ui.checkbox(
                        &mut self.request_control,
                        "Request mouse and keyboard control",
                    );
                });
                if self.viewer.is_none() {
                    if ui.button("Connect").clicked() {
                        self.connect_viewer(ctx);
                    }
                } else if ui.button("Disconnect").clicked() {
                    self.release_control();
                    if let Some(viewer) = &mut self.viewer {
                        viewer.stop();
                    }
                }
                ui.label(&self.viewer_status);
            });
        });
    }

    fn remote_screen(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        if let Ok(mut latest) = self.latest_frame.lock()
            && let Some(frame) = latest.take()
        {
            self.dimensions = [frame.width, frame.height];
            let image = egui::ColorImage::from_rgba_unmultiplied(
                [frame.width as usize, frame.height as usize],
                &frame.rgba,
            );
            if let Some(texture) = &mut self.texture {
                texture.set(image, TextureOptions::LINEAR);
            } else {
                self.texture =
                    Some(ctx.load_texture("remote-desktop", image, TextureOptions::LINEAR));
            }
        }
        ui.separator();
        ui.horizontal(|ui| {
            ui.strong("Remote screen");
            if self.dimensions[0] != 0 {
                ui.label(format!("{} × {}", self.dimensions[0], self.dimensions[1]));
            }
            if self.control_granted {
                ui.label(if self.remote_focused {
                    "Control active · Escape releases the keyboard"
                } else {
                    "Click the remote screen to control it"
                });
                if ui.button("Release keys").clicked() {
                    self.release_control();
                }
            }
        });
        if self.control_granted && self.viewer.is_some() {
            ui.horizontal(|ui| {
                ui.small("Remote shortcuts");
                for (label, command) in [
                    ("Copy", RemoteShortcut::Copy),
                    ("Cut", RemoteShortcut::Cut),
                    ("Paste", RemoteShortcut::Paste),
                    ("Windows / Super", RemoteShortcut::SystemMenu),
                ] {
                    if ui.button(label).clicked() {
                        self.submit_shortcut(command);
                    }
                }
            });
        }
        let Some(texture) = &self.texture else {
            ui.centered_and_justified(|ui| {
                ui.label("An approved connection will appear here.");
            });
            return;
        };
        let available = ui.available_size().max(Vec2::splat(1.0));
        let native = Vec2::new(self.dimensions[0] as f32, self.dimensions[1] as f32);
        let factor = (available.x / native.x)
            .min(available.y / native.y)
            .min(1.0);
        let response = ui.add(
            egui::Image::new(texture)
                .fit_to_exact_size(native * factor)
                .sense(Sense::click_and_drag()),
        );
        if !self.control_granted || self.viewer.is_none() {
            return;
        }
        if self.pending.is_some() {
            self.release_control();
            return;
        }
        if response.is_pointer_button_down_on() || response.clicked() || response.drag_started() {
            self.remote_focused = true;
        }
        let input = ctx.input(|input| {
            (
                input.events.clone(),
                input.modifiers,
                input.focused,
                input.pointer.any_pressed(),
                input.pointer.interact_pos(),
            )
        });
        if !input.2 || (input.3 && input.4.is_some_and(|point| !response.rect.contains(point))) {
            self.release_control();
        }
        if !self.remote_focused {
            return;
        }
        if input.0.iter().any(|event| {
            matches!(
                event,
                egui::Event::Key {
                    key: Key::Escape,
                    pressed: true,
                    ..
                }
            )
        }) {
            self.release_control();
            return;
        }
        for (old, new, key) in [
            (self.modifiers.shift, input.1.shift, RemoteKey::Shift),
            (self.modifiers.ctrl, input.1.ctrl, RemoteKey::Control),
            (self.modifiers.alt, input.1.alt, RemoteKey::Alt),
            (self.modifiers.mac_cmd, input.1.mac_cmd, RemoteKey::Meta),
        ] {
            if old != new {
                self.send_input(RemoteInput::Key { key, pressed: new });
            }
        }
        self.modifiers = input.1;
        for event in input.0 {
            match event {
                egui::Event::Copy => {
                    self.submit_shortcut(RemoteShortcut::Copy);
                    break;
                }
                egui::Event::Cut => {
                    self.submit_shortcut(RemoteShortcut::Cut);
                    break;
                }
                egui::Event::Paste(_) => {
                    // Forward the keystroke action only; never transfer local clipboard text.
                    self.submit_shortcut(RemoteShortcut::Paste);
                    break;
                }
                egui::Event::PointerMoved(point) if response.rect.contains(point) => {
                    let (x, y) = map_pointer(point, response.rect, self.dimensions);
                    self.send_input(RemoteInput::PointerMove { x, y });
                }
                egui::Event::PointerButton {
                    pos,
                    button,
                    pressed,
                    ..
                } if response.rect.contains(pos) || !pressed => {
                    if let Some(button) = map_button(button) {
                        if pressed {
                            let (x, y) = map_pointer(pos, response.rect, self.dimensions);
                            self.send_input(RemoteInput::PointerMove { x, y });
                        }
                        self.send_input(RemoteInput::Button { button, pressed });
                    }
                }
                egui::Event::Key {
                    key,
                    physical_key,
                    pressed,
                    repeat,
                    ..
                } if !repeat => {
                    if let Some(key) = map_key(physical_key.unwrap_or(key)) {
                        self.send_input(RemoteInput::Key { key, pressed });
                    }
                }
                egui::Event::MouseWheel { delta, .. } if response.hovered() && delta.y != 0.0 => {
                    self.send_input(RemoteInput::Scroll {
                        vertical: if delta.y > 0.0 { 1 } else { -1 },
                    });
                }
                _ => {}
            }
        }
    }
}

impl eframe::App for RemoteDesktopApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self
            .close_at
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        self.drain_events();
        ctx.request_repaint_after(Duration::from_millis(100));
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.small("Prototype · X11 Linux / Windows · direct or self-hosted relay · each session needs approval");
        });
        egui::CentralPanel::default().show(ctx, |ui| {
            self.connection_controls(ui, ctx);
            self.remote_screen(ui, ctx);
        });
        let mut answer = None;
        if let Some(pending) = &self.pending {
            egui::Window::new("Allow a remote session?")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
                .show(ctx, |ui| {
                    ui.heading(&pending.request.name);
                    ui.label(format!("Network peer: {}", pending.peer));
                    ui.label("This viewer has the pairing secret. Choose what they may do.");
                    ui.horizontal(|ui| {
                        if ui.button("Decline").clicked() {
                            answer = Some(Permission::Decline);
                        }
                        if ui.button("Allow viewing").clicked() {
                            answer = Some(Permission::View);
                        }
                        if pending.request.request_control
                            && ui.button("Allow mouse and keyboard").clicked()
                        {
                            answer = Some(Permission::Control);
                        }
                    });
                    ui.small(
                        "The request expires after 60 seconds. Stop sharing to end the session.",
                    );
                });
        }
        if let Some(permission) = answer
            && let Some(pending) = self.pending.take()
        {
            let _ = pending.answer.send(permission);
        }
    }
}

impl Drop for RemoteDesktopApp {
    fn drop(&mut self) {
        // Shutdown both sockets before joining either worker, including self-connect sessions.
        if let Some(host) = &mut self.host {
            host.stop();
        }
        if let Some(viewer) = &mut self.viewer {
            viewer.stop();
        }
    }
}

fn notify(events: &mpsc::Sender<Event>, ctx: &egui::Context, event: Event) {
    let _ = events.send(event);
    ctx.request_repaint();
}

fn host_connection(
    stream: TcpStream,
    secret: &str,
    peer: &str,
    stop: &AtomicBool,
    events: &mpsc::Sender<Event>,
    ctx: &egui::Context,
) -> Result<()> {
    let mut connection = transport::accept(stream, secret)?;
    let request = session::read_request(&mut connection)?;
    let (answer, response) = mpsc::sync_channel(1);
    notify(
        events,
        ctx,
        Event::HostRequest {
            peer: peer.into(),
            request: request.clone(),
            answer,
        },
    );
    let expires = Instant::now() + Duration::from_secs(60);
    let permission = loop {
        if stop.load(Ordering::Acquire) || Instant::now() >= expires {
            break Permission::Decline;
        }
        match response.recv_timeout(Duration::from_millis(100)) {
            Ok(permission) => break permission,
            Err(mpsc::RecvTimeoutError::Disconnected) => break Permission::Decline,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    };
    notify(events, ctx, Event::HostRequestClosed);
    if !session::answer_request(&mut connection, &request, permission)? {
        return Ok(());
    }
    notify(
        events,
        ctx,
        Event::HostStatus(format!(
            "Sharing with {} ({peer}); stop sharing to disconnect",
            request.name
        )),
    );
    let mut desktop = Desktop::new().context("Unable to open this desktop for sharing")?;
    session::serve(
        &mut connection,
        &mut desktop,
        permission == Permission::Control && request.request_control,
    )
}

#[allow(clippy::too_many_arguments)]
fn viewer_connection(
    address: &str,
    secret: &str,
    name: &str,
    request_control: bool,
    use_relay: bool,
    stop: &AtomicBool,
    socket: &SocketHandle,
    inputs: Receiver<RemoteInput>,
    release: Arc<AtomicBool>,
    shortcut: ShortcutQueue,
    frames: LatestFrame,
    events: &mpsc::Sender<Event>,
    ctx: &egui::Context,
) -> Result<()> {
    crate::protocol::validate_hello(VERSION, name)?;
    ensure!(
        secret.len() == 64 && secret.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "Enter the 64-character pairing secret from the host"
    );
    let stream = connect_stream(address, stop)?;
    *socket
        .lock()
        .map_err(|_| anyhow::anyhow!("Socket lock was poisoned"))? = Some(stream.try_clone()?);
    let stream = if use_relay {
        notify(
            events,
            ctx,
            Event::ViewerStatus("Waiting for the host on the relay…".into()),
        );
        crate::relay::join(stream, crate::relay::Role::Viewer, secret)?
    } else {
        stream
    };
    let mut connection = transport::connect(stream, secret)?;
    connection.send(&ClientMessage::Hello {
        version: VERSION,
        name: name.into(),
        request_control,
    })?;
    notify(
        events,
        ctx,
        Event::ViewerStatus("Authenticated; waiting for the host's approval…".into()),
    );
    let control = match connection.receive()? {
        ServerMessage::Accepted { version, control } => {
            ensure!(
                version == VERSION,
                "Host uses an incompatible protocol version"
            );
            control
        }
        ServerMessage::Rejected { reason } => bail!("{reason}"),
        _ => bail!("Host sent a frame before granting permission"),
    };
    connection.set_timeout(Duration::from_secs(10))?;
    notify(events, ctx, Event::ViewerConnected { control });
    while !stop.load(Ordering::Acquire) {
        let started = Instant::now();
        let mut release_inputs = release.swap(false, Ordering::AcqRel);
        let batch: Vec<_> = inputs.try_iter().take(MAX_INPUT_BATCH).collect();
        let mut batch = if control && !release_inputs {
            batch
        } else {
            Vec::new()
        };
        if release_inputs {
            while inputs.try_recv().is_ok() {}
        }
        if let Some(command) = shortcut
            .lock()
            .map_err(|_| anyhow::anyhow!("Shortcut lock was poisoned"))?
            .take()
            && control
        {
            release_inputs = true;
            batch = command.inputs();
        }
        connection.send(&ClientMessage::Poll {
            inputs: batch,
            release_inputs,
        })?;
        match connection.receive()? {
            ServerMessage::Frame {
                width,
                height,
                jpeg,
                ..
            } => {
                let frame = session::decode_frame(width, height, &jpeg)?;
                *frames
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Frame lock was poisoned"))? = Some(frame);
                ctx.request_repaint();
            }
            ServerMessage::Unchanged { .. } => {}
            _ => bail!("Host sent an invalid message inside the screen stream"),
        }
        thread::sleep(Duration::from_millis(67).saturating_sub(started.elapsed()));
    }
    let _ = connection.send(&ClientMessage::Disconnect);
    Ok(())
}

fn connect_stream(address: &str, stop: &AtomicBool) -> Result<TcpStream> {
    for endpoint in address.to_socket_addrs()?.take(4) {
        ensure!(!stop.load(Ordering::Acquire), "Connection cancelled");
        if let Ok(stream) = TcpStream::connect_timeout(&endpoint, Duration::from_secs(5)) {
            return Ok(stream);
        }
    }
    bail!("Could not connect to the selected address")
}

fn map_pointer(point: egui::Pos2, rect: egui::Rect, dimensions: [u32; 2]) -> (u32, u32) {
    let x = ((point.x - rect.min.x) / rect.width() * dimensions[0] as f32)
        .floor()
        .max(0.0) as u32;
    let y = ((point.y - rect.min.y) / rect.height() * dimensions[1] as f32)
        .floor()
        .max(0.0) as u32;
    (
        x.min(dimensions[0].saturating_sub(1)),
        y.min(dimensions[1].saturating_sub(1)),
    )
}

fn map_button(button: EguiButton) -> Option<PointerButton> {
    match button {
        EguiButton::Primary => Some(PointerButton::Left),
        EguiButton::Middle => Some(PointerButton::Middle),
        EguiButton::Secondary => Some(PointerButton::Right),
        _ => None,
    }
}

fn map_key(key: Key) -> Option<RemoteKey> {
    Some(match key {
        Key::A => RemoteKey::A,
        Key::B => RemoteKey::B,
        Key::C => RemoteKey::C,
        Key::D => RemoteKey::D,
        Key::E => RemoteKey::E,
        Key::F => RemoteKey::F,
        Key::G => RemoteKey::G,
        Key::H => RemoteKey::H,
        Key::I => RemoteKey::I,
        Key::J => RemoteKey::J,
        Key::K => RemoteKey::K,
        Key::L => RemoteKey::L,
        Key::M => RemoteKey::M,
        Key::N => RemoteKey::N,
        Key::O => RemoteKey::O,
        Key::P => RemoteKey::P,
        Key::Q => RemoteKey::Q,
        Key::R => RemoteKey::R,
        Key::S => RemoteKey::S,
        Key::T => RemoteKey::T,
        Key::U => RemoteKey::U,
        Key::V => RemoteKey::V,
        Key::W => RemoteKey::W,
        Key::X => RemoteKey::X,
        Key::Y => RemoteKey::Y,
        Key::Z => RemoteKey::Z,
        Key::Num0 => RemoteKey::Digit0,
        Key::Num1 => RemoteKey::Digit1,
        Key::Num2 => RemoteKey::Digit2,
        Key::Num3 => RemoteKey::Digit3,
        Key::Num4 => RemoteKey::Digit4,
        Key::Num5 => RemoteKey::Digit5,
        Key::Num6 => RemoteKey::Digit6,
        Key::Num7 => RemoteKey::Digit7,
        Key::Num8 => RemoteKey::Digit8,
        Key::Num9 => RemoteKey::Digit9,
        Key::Enter => RemoteKey::Enter,
        Key::Escape => RemoteKey::Escape,
        Key::Backspace => RemoteKey::Backspace,
        Key::Tab => RemoteKey::Tab,
        Key::Space => RemoteKey::Space,
        Key::ArrowUp => RemoteKey::ArrowUp,
        Key::ArrowDown => RemoteKey::ArrowDown,
        Key::ArrowLeft => RemoteKey::ArrowLeft,
        Key::ArrowRight => RemoteKey::ArrowRight,
        Key::Home => RemoteKey::Home,
        Key::End => RemoteKey::End,
        Key::PageUp => RemoteKey::PageUp,
        Key::PageDown => RemoteKey::PageDown,
        Key::Delete => RemoteKey::Delete,
        Key::Insert => RemoteKey::Insert,
        Key::F1 => RemoteKey::F1,
        Key::F2 => RemoteKey::F2,
        Key::F3 => RemoteKey::F3,
        Key::F4 => RemoteKey::F4,
        Key::F5 => RemoteKey::F5,
        Key::F6 => RemoteKey::F6,
        Key::F7 => RemoteKey::F7,
        Key::F8 => RemoteKey::F8,
        Key::F9 => RemoteKey::F9,
        Key::F10 => RemoteKey::F10,
        Key::F11 => RemoteKey::F11,
        Key::F12 => RemoteKey::F12,
        Key::Minus => RemoteKey::Minus,
        Key::Equals => RemoteKey::Equal,
        Key::OpenBracket => RemoteKey::LeftBracket,
        Key::CloseBracket => RemoteKey::RightBracket,
        Key::Backslash => RemoteKey::Backslash,
        Key::Semicolon => RemoteKey::Semicolon,
        Key::Quote => RemoteKey::Quote,
        Key::Comma => RemoteKey::Comma,
        Key::Period => RemoteKey::Period,
        Key::Slash => RemoteKey::Slash,
        Key::Backtick => RemoteKey::Backtick,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn remote_shortcuts_release_their_modifiers() {
        for shortcut in [
            RemoteShortcut::Copy,
            RemoteShortcut::Cut,
            RemoteShortcut::Paste,
            RemoteShortcut::SystemMenu,
        ] {
            let mut held = std::collections::HashSet::new();
            for event in shortcut.inputs() {
                if let RemoteInput::Key { key, pressed } = event {
                    if pressed {
                        held.insert(key);
                    } else {
                        assert!(held.remove(&key));
                    }
                }
            }
            assert!(held.is_empty());
        }
    }
    #[test]
    fn pointer_mapping_uses_remote_pixels_and_clamps_edges() {
        let rect = egui::Rect::from_min_size(egui::pos2(10.0, 20.0), Vec2::new(960.0, 540.0));
        assert_eq!(
            map_pointer(egui::pos2(490.0, 290.0), rect, [1920, 1080]),
            (960, 540)
        );
        assert_eq!(map_pointer(rect.max, rect, [1920, 1080]), (1919, 1079));
        assert_eq!(map_pointer(egui::Pos2::ZERO, rect, [1920, 1080]), (0, 0));
    }
}
