//! The thin-client windowed app: a winit window + wgpu surface that renders the **daemon's** panes
//! (fetched over the pipe each frame) and forwards input/control to the daemon. The daemon owns the
//! panes, so closing this window detaches — the agents keep running — and relaunching reattaches.

use std::sync::Arc;
use std::time::{Duration, Instant};

use gmux_mux::{Attention, Cell, PaneSnapshot, Rect, Rgb};
use gmux_notify::{flash_window, Notifier, ProgressState as NProgress, Taskbar, ToastRequest, Urgency as NUrgency};
use gmux_proto::{Call, GridWire, LayoutWire, NotifyWire, ResultBody, CELL_BOLD, CELL_INVERSE, CELL_ITALIC, CELL_UNDERLINE};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

use crate::daemon_client::DaemonClient;
use crate::renderer::{PaneView, SidebarRow};
use crate::Renderer;

const FONT_PX: f32 = 18.0;
const TOAST_GROUP: &str = "gmux-session";
const TOAST_MIN_INTERVAL: Duration = Duration::from_millis(1000);
const FRAME: Duration = Duration::from_millis(33); // ~30 fps poll of remote grids

pub struct App {
    mods: ModifiersState,
    state: Option<State>,
}

struct State {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    renderer: Renderer,
    client: DaemonClient,
    active_pane: u64,
    focused: bool,
    hwnd: isize,
    notifier: Option<Notifier>,
    taskbar: Option<Taskbar>,
    last_toast: std::collections::HashMap<u64, Instant>,
}

/// Run the gmux GUI. `_shell` is currently unused (the daemon picks its shell); kept for the CLI
/// signature and a future `--daemon <shell>` hand-off.
pub fn run(_shell: String) -> Result<(), Box<dyn std::error::Error>> {
    let event_loop = EventLoop::new()?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut app = App { mods: ModifiersState::empty(), state: None };
    event_loop.run_app(&mut app)?;
    Ok(())
}

impl ApplicationHandler for App {
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

        // Attach to (or start) the daemon.
        let client = match DaemonClient::connect_or_spawn("gmux") {
            Ok(c) => c,
            Err(e) => {
                eprintln!("gmux: cannot reach the daemon: {e}");
                el.exit();
                return;
            }
        };

        let hwnd = window_hwnd(&window).unwrap_or(0);
        let notifier = Notifier::new("com.gmux.app", "gmux").ok();
        let taskbar = if hwnd != 0 { Taskbar::new(hwnd) } else { None };

        let mut st = State {
            window,
            surface,
            config,
            renderer,
            client,
            active_pane: 0,
            focused: true,
            hwnd,
            notifier,
            taskbar,
            last_toast: std::collections::HashMap::new(),
        };
        st.sync_size();
        self.state = Some(st);
        el.set_control_flow(ControlFlow::WaitUntil(Instant::now() + FRAME));
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),
            WindowEvent::Focused(f) => {
                if let Some(st) = self.state.as_mut() {
                    st.focused = f;
                    if f {
                        st.clear_active_toast();
                        flash_window(st.hwnd, false);
                        st.window.request_redraw();
                    }
                }
            }
            WindowEvent::Resized(sz) => {
                if let Some(st) = self.state.as_mut() {
                    st.config.width = sz.width.max(1);
                    st.config.height = sz.height.max(1);
                    st.surface.configure(&st.renderer.device, &st.config);
                    st.sync_size();
                    st.window.request_redraw();
                }
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                if !self.try_shortcut(&event) {
                    if let Some(bytes) = key_to_bytes(&event, self.mods) {
                        if let Some(st) = self.state.as_mut() {
                            let text = String::from_utf8_lossy(&bytes).into_owned();
                            st.client.control(Call::SendKeys { pane: st.active_pane, text, enter: false });
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

        // Drain daemon notifications and toast the ones that arrived while unfocused.
        if let Ok(ResultBody::Notifications(notes)) = st.client.call(Call::PollNotifications) {
            for n in notes {
                if !st.focused {
                    st.fire_toast(&n);
                }
            }
        } else {
            // Daemon gone: nothing left to render.
            el.exit();
            return;
        }

        if let Some(nf) = &st.notifier {
            if !nf.poll_activations().is_empty() {
                st.window.focus_window();
                st.clear_active_toast();
                flash_window(st.hwnd, false);
            }
        }

        // Poll the daemon for fresh output by re-rendering.
        st.window.request_redraw();
        el.set_control_flow(ControlFlow::WaitUntil(Instant::now() + FRAME));
    }
}

impl App {
    /// Handle a gmux keyboard chord by forwarding a control call to the daemon.
    fn try_shortcut(&mut self, event: &KeyEvent) -> bool {
        let mods = self.mods;
        let Some(st) = self.state.as_mut() else { return false };

        if mods.alt_key() && !mods.control_key() {
            if let Key::Named(named) = &event.logical_key {
                let dir = match named {
                    NamedKey::ArrowLeft => Some("left"),
                    NamedKey::ArrowRight => Some("right"),
                    NamedKey::ArrowUp => Some("up"),
                    NamedKey::ArrowDown => Some("down"),
                    _ => None,
                };
                if let Some(d) = dir {
                    st.client.control(Call::FocusPane { dir: d.to_string() });
                    st.window.request_redraw();
                    return true;
                }
            }
        }

        if mods.control_key() && mods.shift_key() {
            if let Key::Character(s) = &event.logical_key {
                let control = match s.chars().next().map(|c| c.to_ascii_lowercase()) {
                    Some('d') => Some(Call::SplitPane { dir: "h".into(), command: None }),
                    Some('e') => Some(Call::SplitPane { dir: "v".into(), command: None }),
                    Some('w') => Some(Call::ClosePane),
                    Some('z') => Some(Call::ToggleZoom),
                    Some('t') => Some(Call::NewWindow { command: None }),
                    Some('n') => Some(Call::SwitchWindow { next: true }),
                    Some('p') => Some(Call::SwitchWindow { next: false }),
                    _ => None,
                };
                if let Some(call) = control {
                    st.client.control(call);
                    st.sync_size();
                    st.window.request_redraw();
                    return true;
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

    fn areas(&self) -> (u32, u32, u32) {
        let sidebar_w = self.renderer.sidebar_width().min(self.config.width / 3);
        let content_w = self.config.width.saturating_sub(sidebar_w).max(1);
        (sidebar_w, content_w, self.config.height)
    }

    /// Tell the daemon our content geometry so it resizes its panes.
    fn sync_size(&mut self) {
        let (_, content_w, h) = self.areas();
        let (cw, ch) = self.cell_dims();
        self.client.control(Call::ResizeView { w: content_w, h, cell_w: cw, cell_h: ch });
    }

    fn clear_active_toast(&self) {
        if let Some(nf) = &self.notifier {
            nf.clear(&format!("pane-{}", self.active_pane), TOAST_GROUP);
        }
    }

    fn fire_toast(&mut self, n: &NotifyWire) {
        let now = Instant::now();
        if let Some(prev) = self.last_toast.get(&n.pane) {
            if now.duration_since(*prev) < TOAST_MIN_INTERVAL {
                return;
            }
        }
        self.last_toast.insert(n.pane, now);
        let title = if n.title.is_empty() { "gmux".to_string() } else { n.title.clone() };
        let req = ToastRequest {
            tag: format!("pane-{}", n.pane),
            group: TOAST_GROUP.to_string(),
            title,
            body: n.body.clone(),
            urgency: match n.urgency {
                0 => NUrgency::Low,
                2 => NUrgency::Critical,
                _ => NUrgency::Normal,
            },
            launch_arg: format!("pane={}", n.pane),
        };
        if let Some(nf) = &self.notifier {
            let _ = nf.show(&req);
        }
        flash_window(self.hwnd, true);
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

        let layout: LayoutWire = match self.client.call(Call::GetLayout { w: content_w, h }) {
            Ok(ResultBody::Layout(l)) => l,
            _ => return,
        };
        self.active_pane = layout.active_pane;
        let rows: Vec<SidebarRow> = layout
            .tabs
            .iter()
            .map(|t| SidebarRow { name: t.name.clone(), branch: t.branch.clone(), attention: t.attention, active: t.active })
            .collect();

        // Update the taskbar attention badge / progress based on overall attention.
        if let Some(tb) = &self.taskbar {
            let any = layout.panes.iter().any(|p| p.attention);
            tb.set_progress(if any { NProgress::Paused } else { NProgress::None }, None);
        }

        let mut snaps: Vec<(PaneSnapshot, Attention, bool, Rect)> = Vec::new();
        for pr in &layout.panes {
            if let Ok(ResultBody::Grid(g)) = self.client.call(Call::GetGrid { pane: pr.id }) {
                let snap = grid_to_snapshot(&g);
                let att = if pr.attention { Attention::Pending } else { Attention::Quiet };
                let rect = Rect { x: pr.x + sidebar_w, y: pr.y, w: pr.w, h: pr.h };
                snaps.push((snap, att, pr.active, rect));
            }
        }
        let views: Vec<PaneView> = snaps
            .iter()
            .map(|(s, a, active, rect)| PaneView { snap: s, attention: *a, active: *active, rect: *rect })
            .collect();
        self.renderer.render_frame(&view, &rows, sidebar_w, &views, w, h);
    }
}

/// Reconstruct a [`PaneSnapshot`] from a wire grid.
fn grid_to_snapshot(g: &GridWire) -> PaneSnapshot {
    let cols = g.cols as usize;
    let rows = g.rows as usize;
    let blank = Cell {
        ch: ' ',
        fg: Rgb { r: 0xcc, g: 0xcc, b: 0xcc },
        bg: Rgb { r: 0x11, g: 0x11, b: 0x11 },
        bold: false,
        italic: false,
        underline: false,
        inverse: false,
    };
    let mut cells = Vec::with_capacity(rows);
    for r in 0..rows {
        let mut row = Vec::with_capacity(cols);
        for c in 0..cols {
            let idx = r * cols + c;
            row.push(match g.cells.get(idx) {
                Some(cw) => Cell {
                    ch: cw.ch,
                    fg: Rgb { r: cw.fg[0], g: cw.fg[1], b: cw.fg[2] },
                    bg: Rgb { r: cw.bg[0], g: cw.bg[1], b: cw.bg[2] },
                    bold: cw.flags & CELL_BOLD != 0,
                    italic: cw.flags & CELL_ITALIC != 0,
                    underline: cw.flags & CELL_UNDERLINE != 0,
                    inverse: cw.flags & CELL_INVERSE != 0,
                },
                None => blank,
            });
        }
        cells.push(row);
    }
    PaneSnapshot { cells, cursor: (g.cursor_col, g.cursor_row), cols: g.cols, rows: g.rows }
}

fn window_hwnd(window: &Window) -> Option<isize> {
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match window.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
        _ => None,
    }
}

/// Translate a key press into bytes for the PTY (full win32-input-mode fidelity comes later).
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
                    return Some(vec![(c as u8 - b'a') + 1]);
                }
            }
            Some(s.as_bytes().to_vec())
        }
        _ => None,
    }
}
