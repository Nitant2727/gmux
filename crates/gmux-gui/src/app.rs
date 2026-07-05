//! The windowed app: a winit window + wgpu surface rendering a session's active window (a split
//! tree of panes). Keyboard input goes to the active pane; Ctrl+Shift / Alt chords drive splitting,
//! focus movement, resize, zoom, and window (tab) management.

use std::sync::Arc;
use std::time::{Duration, Instant};

use gmux_mux::{
    FocusDir, Notification, Pane, PaneEvent, PaneSnapshot, ProgressState, PtySize, Session,
    SplitDir, Urgency,
};
use gmux_notify::{
    flash_window, Notifier, ProgressState as NProgress, Taskbar, ToastRequest, Urgency as NUrgency,
};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

use crate::api::{self, ApiCommand};
use crate::renderer::{PaneView, SidebarRow};
use crate::Renderer;

const FONT_PX: f32 = 18.0;
const TOAST_GROUP: &str = "gmux-session";
const TOAST_MIN_INTERVAL: Duration = Duration::from_millis(1000);
const RESIZE_STEP: f32 = 0.05;

pub struct App {
    shell: String,
    mods: ModifiersState,
    state: Option<State>,
    proxy: winit::event_loop::EventLoopProxy<()>,
    cmd_tx: std::sync::mpsc::Sender<ApiCommand>,
    cmd_rx: std::sync::mpsc::Receiver<ApiCommand>,
}

struct State {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    renderer: Renderer,
    session: Session,
    focused: bool,
    hwnd: isize,
    notifier: Option<Notifier>,
    taskbar: Option<Taskbar>,
    last_toast: std::collections::HashMap<u64, Instant>,
    /// Keeps the automation pipe server's accept loop alive.
    _api_server: Option<gmux_pipe::PipeServer>,
}

/// Run the gmux GUI with the given shell command line. Blocks until the window closes.
pub fn run(shell: String) -> Result<(), Box<dyn std::error::Error>> {
    let event_loop = EventLoop::<()>::with_user_event().build()?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
    let mut app = App { shell, mods: ModifiersState::empty(), state: None, proxy, cmd_tx, cmd_rx };
    event_loop.run_app(&mut app)?;
    Ok(())
}

impl ApplicationHandler<()> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }
        let window = Arc::new(
            el.create_window(Window::default_attributes().with_title("gmux")).expect("create window"),
        );
        let size = window.inner_size();

        let instance = wgpu::Instance::default();
        let surface = instance.create_surface(window.clone()).expect("create surface");
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
            apply_limit_buckets: false,
        }))
        .expect("request adapter");
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("gmux-gui"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits()),
            experimental_features: wgpu::ExperimentalFeatures::default(),
            memory_hints: wgpu::MemoryHints::default(),
            trace: wgpu::Trace::Off,
        }))
        .expect("request device");

        let caps = surface.get_capabilities(&adapter);
        let format = caps.formats.iter().copied().find(|f| f.is_srgb()).unwrap_or(caps.formats[0]);
        let mut config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .expect("surface default config");
        config.format = format;
        surface.configure(&device, &config);

        let renderer = Renderer::from_device(device, queue, format, FONT_PX);
        let sidebar_w = renderer.sidebar_width().min(config.width / 3);
        let content_w = config.width.saturating_sub(sidebar_w).max(1);
        let cols = (content_w / renderer.cell_w()).max(1) as u16;
        let rows = (config.height / renderer.cell_h()).max(1) as u16;
        let pane = Pane::spawn(&self.shell, PtySize { cols, rows }).expect("spawn shell");
        let session = Session::start("gmux", pane);

        let hwnd = window_hwnd(&window).unwrap_or(0);
        let notifier = Notifier::new("com.gmux.app", "gmux").ok();
        let taskbar = if hwnd != 0 { Taskbar::new(hwnd) } else { None };

        // Start the automation pipe server (\\.\pipe\gmux.<user>).
        let api_server = match api::start("gmux", self.proxy.clone(), self.cmd_tx.clone()) {
            Ok((server, name)) => {
                eprintln!("gmux: automation API on {name}");
                Some(server)
            }
            Err(e) => {
                eprintln!("gmux: automation API unavailable: {e}");
                None
            }
        };

        self.state = Some(State {
            window,
            surface,
            config,
            renderer,
            session,
            focused: true,
            hwnd,
            notifier,
            taskbar,
            last_toast: std::collections::HashMap::new(),
            _api_server: api_server,
        });
        el.set_control_flow(ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(16)));
    }

    fn user_event(&mut self, _el: &ActiveEventLoop, _ev: ()) {
        // A pipe thread queued API commands; service them all now.
        let shell = self.shell.clone();
        while let Ok(cmd) = self.cmd_rx.try_recv() {
            if let Some(st) = self.state.as_mut() {
                let response = st.handle_api(&shell, &cmd.request);
                let _ = cmd.reply.send(response);
            } else {
                let _ = cmd.reply.send(gmux_proto::Response::err(cmd.request.id, "gmux not ready"));
            }
        }
        if let Some(st) = self.state.as_ref() {
            st.window.request_redraw();
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),
            WindowEvent::Focused(f) => {
                if let Some(st) = self.state.as_mut() {
                    st.focused = f;
                    if f {
                        st.focus_active_pane();
                        st.window.request_redraw();
                    }
                }
            }
            WindowEvent::Resized(sz) => {
                if let Some(st) = self.state.as_mut() {
                    st.config.width = sz.width.max(1);
                    st.config.height = sz.height.max(1);
                    st.surface.configure(&st.renderer.device, &st.config);
                    st.resize_active_window();
                    st.window.request_redraw();
                }
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                if !self.try_shortcut(&event) {
                    if let Some(bytes) = key_to_bytes(&event, self.mods) {
                        if let Some(st) = self.state.as_ref() {
                            if let Some(w) = st.session.active_window() {
                                let _ = w.active_pane().write(&bytes);
                                w.active_pane().focus();
                            }
                        }
                    }
                }
            }
            WindowEvent::RedrawRequested => {
                if let Some(st) = self.state.as_mut() {
                    st.render();
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        let Some(st) = self.state.as_mut() else { return };

        // Collect events across every pane (borrowing the session immutably).
        let mut redraw = false;
        let mut notes: Vec<(u64, Notification)> = Vec::new();
        let mut bells: Vec<u64> = Vec::new();
        let mut progress: Vec<(ProgressState, Option<u8>)> = Vec::new();
        let mut exited: Vec<gmux_mux::PaneId> = Vec::new();
        let mut title: Option<String> = None;
        let active_pane_id = st.session.active_window().map(|w| w.active_id());
        for w in st.session.windows() {
            for p in w.panes() {
                for ev in p.drain_events() {
                    match ev {
                        PaneEvent::Output => redraw = true,
                        PaneEvent::Notification(n) => {
                            redraw = true;
                            notes.push((p.id.0, n));
                        }
                        PaneEvent::Bell => bells.push(p.id.0),
                        PaneEvent::Progress { state, pct } => progress.push((state, pct)),
                        PaneEvent::Title(t) => {
                            if Some(p.id) == active_pane_id {
                                title = Some(t);
                            }
                        }
                        PaneEvent::Cwd(_) => {}
                        PaneEvent::Exited => exited.push(p.id),
                    }
                }
            }
        }

        // Process (session borrow released).
        if let Some(t) = title {
            st.window.set_title(&format!("gmux — {t}"));
        }
        for (pane_id, n) in notes {
            if !st.focused {
                st.fire_toast(pane_id, &n);
            }
        }
        if !bells.is_empty() && !st.focused {
            flash_window(st.hwnd, true);
        }
        for (state, pct) in progress {
            if let Some(tb) = &st.taskbar {
                tb.set_progress(map_progress(state), pct);
            }
        }
        for id in exited {
            st.session.remove_pane(id);
            redraw = true;
        }
        if st.session.pane_count() == 0 {
            el.exit();
            return;
        }

        if let Some(nf) = &st.notifier {
            if !nf.poll_activations().is_empty() {
                st.window.focus_window();
                st.focus_active_pane();
                redraw = true;
            }
        }

        if redraw {
            st.window.request_redraw();
        }
        el.set_control_flow(ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(16)));
    }
}

impl App {
    /// Handle a gmux keyboard chord. Returns whether it was consumed (so it isn't sent to the pane).
    fn try_shortcut(&mut self, event: &KeyEvent) -> bool {
        let shell = self.shell.clone();
        let mods = self.mods;
        let Some(st) = self.state.as_mut() else { return false };

        // Alt+Arrow → move focus between panes.
        if mods.alt_key() && !mods.control_key() {
            if let Key::Named(named) = &event.logical_key {
                let dir = match named {
                    NamedKey::ArrowLeft => Some(FocusDir::Left),
                    NamedKey::ArrowRight => Some(FocusDir::Right),
                    NamedKey::ArrowUp => Some(FocusDir::Up),
                    NamedKey::ArrowDown => Some(FocusDir::Down),
                    _ => None,
                };
                if let Some(d) = dir {
                    st.focus_dir(d);
                    return true;
                }
            }
        }

        // Ctrl+Shift+... → pane / window management.
        if mods.control_key() && mods.shift_key() {
            if let Key::Named(named) = &event.logical_key {
                let delta = match named {
                    NamedKey::ArrowLeft | NamedKey::ArrowUp => Some(-RESIZE_STEP),
                    NamedKey::ArrowRight | NamedKey::ArrowDown => Some(RESIZE_STEP),
                    _ => None,
                };
                if let Some(d) = delta {
                    st.resize_ratio(d);
                    return true;
                }
            }
            if let Key::Character(s) = &event.logical_key {
                match s.chars().next().map(|c| c.to_ascii_lowercase()) {
                    Some('d') => return st.action_split(&shell, SplitDir::Horizontal),
                    Some('e') => return st.action_split(&shell, SplitDir::Vertical),
                    Some('w') => return st.action_close_pane(),
                    Some('z') => return st.action_zoom(),
                    Some('t') => return st.action_new_window(&shell),
                    Some('n') => return st.action_switch_window(true),
                    Some('p') => return st.action_switch_window(false),
                    _ => {}
                }
            }
        }
        false
    }
}

impl State {
    fn cell_dims(&self) -> (u32, u32) {
        (self.renderer.cell_w().max(1), self.renderer.cell_h().max(1))
    }

    /// `(sidebar_w, content_w, height)` — the sidebar takes a fixed column (capped at 1/3 width).
    fn areas(&self) -> (u32, u32, u32) {
        let sidebar_w = self.renderer.sidebar_width().min(self.config.width / 3);
        let content_w = self.config.width.saturating_sub(sidebar_w).max(1);
        (sidebar_w, content_w, self.config.height)
    }

    fn spawn_pane(&self, shell: &str) -> Option<Pane> {
        let (cw, ch) = self.cell_dims();
        let cols = (self.config.width / cw).max(1) as u16;
        let rows = (self.config.height / ch).max(1) as u16;
        Pane::spawn(shell, PtySize { cols, rows }).ok()
    }

    fn focus_active_pane(&self) {
        if let Some(w) = self.session.active_window() {
            w.active_pane().focus();
        }
        self.clear_active_toast();
        flash_window(self.hwnd, false);
    }

    fn clear_active_toast(&self) {
        if let (Some(nf), Some(w)) = (&self.notifier, self.session.active_window()) {
            nf.clear(&format!("pane-{}", w.active_id().0), TOAST_GROUP);
        }
    }

    fn action_split(&mut self, shell: &str, dir: SplitDir) -> bool {
        if let Some(pane) = self.spawn_pane(shell) {
            if let Some(w) = self.session.active_window_mut() {
                w.split(dir, pane);
            }
            self.resize_active_window();
            self.window.request_redraw();
        }
        true
    }

    fn action_new_window(&mut self, shell: &str) -> bool {
        if let Some(pane) = self.spawn_pane(shell) {
            self.session.add_window(pane);
            self.resize_active_window();
            self.window.request_redraw();
        }
        true
    }

    fn action_close_pane(&mut self) -> bool {
        let closed_pane = self.session.active_window_mut().and_then(|w| w.close_active());
        if closed_pane.is_none() {
            // Last pane in the window: close the window (if not the last one).
            self.session.close_active_window();
        }
        self.resize_active_window();
        self.window.request_redraw();
        true
    }

    fn action_zoom(&mut self) -> bool {
        if let Some(w) = self.session.active_window_mut() {
            w.toggle_zoom();
        }
        self.resize_active_window();
        self.window.request_redraw();
        true
    }

    fn action_switch_window(&mut self, next: bool) -> bool {
        if next {
            self.session.next_window();
        } else {
            self.session.prev_window();
        }
        self.resize_active_window();
        self.window.request_redraw();
        true
    }

    fn focus_dir(&mut self, dir: FocusDir) {
        let (_, content_w, h) = self.areas();
        if let Some(win) = self.session.active_window_mut() {
            win.focus_dir(dir, content_w, h);
        }
        self.window.request_redraw();
    }

    fn resize_ratio(&mut self, delta: f32) {
        if let Some(win) = self.session.active_window_mut() {
            win.resize_active(delta);
        }
        self.resize_active_window();
        self.window.request_redraw();
    }

    /// Resize every pane of the active window to match its computed rectangle (content area).
    fn resize_active_window(&self) {
        let (cw, ch) = self.cell_dims();
        let (_, content_w, h) = self.areas();
        if let Some(w) = self.session.active_window() {
            for (id, rect) in w.layout_rects(content_w, h) {
                if let Some(p) = w.pane(id) {
                    let cols = (rect.w / cw).max(1) as u16;
                    let rows = (rect.h / ch).max(1) as u16;
                    let _ = p.resize(PtySize { cols, rows });
                }
            }
        }
    }

    fn fire_toast(&mut self, pane_id: u64, n: &Notification) {
        let now = Instant::now();
        if let Some(prev) = self.last_toast.get(&pane_id) {
            if now.duration_since(*prev) < TOAST_MIN_INTERVAL {
                return;
            }
        }
        self.last_toast.insert(pane_id, now);
        let title = if n.title.is_empty() { "gmux".to_string() } else { n.title.clone() };
        let req = ToastRequest {
            tag: format!("pane-{pane_id}"),
            group: TOAST_GROUP.to_string(),
            title,
            body: n.body.clone(),
            urgency: map_urgency(n.urgency),
            launch_arg: format!("pane={pane_id}"),
        };
        if let Some(nf) = &self.notifier {
            let _ = nf.show(&req);
        }
        flash_window(self.hwnd, true);
    }

    /// Execute one automation-API call against the mux state (main thread).
    fn handle_api(&mut self, shell: &str, req: &gmux_proto::Request) -> gmux_proto::Response {
        use gmux_proto::{Call, PaneInfo, Response, ResultBody};
        let id = req.id;
        match &req.call {
            Call::Hello { .. } => Response::ok(
                id,
                ResultBody::Hello {
                    server_version: env!("CARGO_PKG_VERSION").to_string(),
                    protocol: gmux_proto::PROTOCOL_VERSION,
                },
            ),
            Call::ListPanes => {
                let active_win = self.session.active_index();
                let mut panes = Vec::new();
                for (wi, win) in self.session.windows().iter().enumerate() {
                    let active_pane = win.active_id();
                    for p in win.panes() {
                        let snap = p.snapshot();
                        panes.push(PaneInfo {
                            id: p.id.0,
                            window: wi,
                            active: wi == active_win && p.id == active_pane,
                            title: p.title(),
                            cwd: p.cwd(),
                            cols: snap.cols,
                            rows: snap.rows,
                            attention: p.attention().is_pending(),
                        });
                    }
                }
                panes.sort_by_key(|p| p.id);
                Response::ok(id, ResultBody::Panes(panes))
            }
            Call::SendKeys { pane, text, enter } => match self.find_pane(*pane) {
                Some(p) => {
                    let mut bytes = text.as_bytes().to_vec();
                    if *enter {
                        bytes.push(b'\r');
                    }
                    match p.write(&bytes) {
                        Ok(()) => Response::ok(id, ResultBody::Done),
                        Err(e) => Response::err(id, format!("write failed: {e}")),
                    }
                }
                None => Response::err(id, format!("no pane %{pane}")),
            },
            Call::CapturePane { pane } => match self.find_pane(*pane) {
                Some(p) => {
                    let snap = p.snapshot();
                    let mut lines: Vec<String> = snap
                        .cells
                        .iter()
                        .map(|row| {
                            let mut s: String = row.iter().map(|c| c.ch).collect();
                            s.truncate(s.trim_end_matches(' ').len());
                            s
                        })
                        .collect();
                    while lines.last().is_some_and(|l| l.is_empty()) {
                        lines.pop();
                    }
                    Response::ok(id, ResultBody::Text(lines.join("\n")))
                }
                None => Response::err(id, format!("no pane %{pane}")),
            },
            Call::SplitPane { dir, command } => {
                let sd = match dir.as_str() {
                    "h" => SplitDir::Horizontal,
                    "v" => SplitDir::Vertical,
                    other => return Response::err(id, format!("bad dir '{other}' (h|v)")),
                };
                let cmd = command.clone().unwrap_or_else(|| shell.to_string());
                match self.spawn_pane(&cmd) {
                    Some(pane) => {
                        let pid = pane.id.0;
                        if let Some(w) = self.session.active_window_mut() {
                            w.split(sd, pane);
                        }
                        self.resize_active_window();
                        self.window.request_redraw();
                        Response::ok(id, ResultBody::PaneId(pid))
                    }
                    None => Response::err(id, "failed to spawn pane"),
                }
            }
            Call::NewWindow { command } => {
                let cmd = command.clone().unwrap_or_else(|| shell.to_string());
                match self.spawn_pane(&cmd) {
                    Some(pane) => {
                        let pid = pane.id.0;
                        self.session.add_window(pane);
                        self.resize_active_window();
                        self.window.request_redraw();
                        Response::ok(id, ResultBody::PaneId(pid))
                    }
                    None => Response::err(id, "failed to spawn pane"),
                }
            }
            Call::Notify { pane, title, body } => {
                let target = pane
                    .or_else(|| self.session.active_window().map(|w| w.active_id().0));
                let Some(target) = target else { return Response::err(id, "no target pane") };
                if self.find_pane(target).is_none() {
                    return Response::err(id, format!("no pane %{target}"));
                }
                if let Some(p) = self.find_pane(target) {
                    p.request_attention();
                }
                let n = Notification {
                    kind: gmux_mux::NotifyKind::Osc777,
                    title: title.clone(),
                    body: body.clone(),
                    urgency: Urgency::Normal,
                    id: None,
                };
                if !self.focused {
                    self.fire_toast(target, &n);
                }
                self.window.request_redraw();
                Response::ok(id, ResultBody::Done)
            }
        }
    }

    fn find_pane(&self, id: u64) -> Option<&Pane> {
        self.session.pane(gmux_mux::PaneId(id))
    }

    fn render(&mut self) {
        use wgpu::CurrentSurfaceTexture::{Suboptimal, Success};
        let frame = match self.surface.get_current_texture() {
            Success(t) | Suboptimal(t) => t,
            _ => {
                self.surface.configure(&self.renderer.device, &self.config);
                return;
            }
        };
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let (w, h) = (self.config.width, self.config.height);
        let (sidebar_w, content_w, _) = self.areas();

        // Sidebar rows: one per window (tab), with git/cwd metadata + attention.
        let active_idx = self.session.active_index();
        let rows: Vec<SidebarRow> = self
            .session
            .windows()
            .iter()
            .enumerate()
            .map(|(i, win)| {
                let info = win.workspace_info();
                SidebarRow { name: info.name, branch: info.branch, attention: info.attention, active: i == active_idx }
            })
            .collect();

        // Collect snapshots for the active window's panes (offset into the content area).
        let mut snaps: Vec<(PaneSnapshot, gmux_mux::Attention, bool, gmux_mux::Rect)> = Vec::new();
        if let Some(win) = self.session.active_window() {
            let active = win.active_id();
            for (id, mut rect) in win.layout_rects(content_w, h) {
                rect.x += sidebar_w;
                if let Some(p) = win.pane(id) {
                    snaps.push((p.snapshot(), p.attention(), id == active, rect));
                }
            }
        }
        let views: Vec<PaneView> = snaps
            .iter()
            .map(|(s, a, active, rect)| PaneView { snap: s, attention: *a, active: *active, rect: *rect })
            .collect();
        self.renderer.render_frame(&view, &rows, sidebar_w, &views, w, h);
        // `frame` presents on drop.
    }
}

fn window_hwnd(window: &Window) -> Option<isize> {
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match window.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
        _ => None,
    }
}

fn map_urgency(u: Urgency) -> NUrgency {
    match u {
        Urgency::Low => NUrgency::Low,
        Urgency::Normal => NUrgency::Normal,
        Urgency::Critical => NUrgency::Critical,
    }
}

fn map_progress(s: ProgressState) -> NProgress {
    match s {
        ProgressState::Remove => NProgress::None,
        ProgressState::Set => NProgress::Normal,
        ProgressState::Error => NProgress::Error,
        ProgressState::Indeterminate => NProgress::Indeterminate,
        ProgressState::Paused => NProgress::Paused,
    }
}

/// Translate a key press into the bytes to send to the PTY. Full win32-input-mode fidelity is a
/// later milestone (ARCHITECTURE §5.3).
fn key_to_bytes(event: &KeyEvent, mods: ModifiersState) -> Option<Vec<u8>> {
    use NamedKey::*;
    match &event.logical_key {
        Key::Named(named) => Some(match named {
            Enter => vec![b'\r'],
            Backspace => vec![0x7f],
            Tab => vec![b'\t'],
            Escape => vec![0x1b],
            Space => vec![b' '],
            ArrowUp => b"\x1b[A".to_vec(),
            ArrowDown => b"\x1b[B".to_vec(),
            ArrowRight => b"\x1b[C".to_vec(),
            ArrowLeft => b"\x1b[D".to_vec(),
            Home => b"\x1b[H".to_vec(),
            End => b"\x1b[F".to_vec(),
            Delete => b"\x1b[3~".to_vec(),
            _ => return None,
        }),
        Key::Character(s) => {
            if mods.control_key() && !mods.shift_key() {
                let c = s.chars().next()?.to_ascii_lowercase();
                if c.is_ascii_lowercase() {
                    return Some(vec![(c as u8 - b'a') + 1]); // Ctrl-A = 1 ...
                }
            }
            Some(s.as_bytes().to_vec())
        }
        _ => None,
    }
}
