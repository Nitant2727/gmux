//! The thin-client windowed app: a winit window + wgpu surface that renders the **daemon's** panes
//! (fetched over the pipe each frame) and forwards input/control to the daemon. The daemon owns the
//! panes, so closing this window detaches — the agents keep running — and relaunching reattaches.

use std::sync::Arc;
use std::time::{Duration, Instant};

use gmux_mux::{Attention, Cell, PaneSnapshot, Rect, Rgb};
use gmux_notify::{flash_window, Notifier, ProgressState as NProgress, Taskbar, ToastRequest, Urgency as NUrgency};
use gmux_proto::{Call, GridWire, LayoutWire, NotifyWire, ResultBody, CELL_BOLD, CELL_INVERSE, CELL_ITALIC, CELL_UNDERLINE};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, KeyEvent, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

use crate::config::{config_path, Action, Config, Keymap};
use crate::daemon_client::DaemonClient;
use crate::renderer::{PaneView, SidebarRow};
use crate::Renderer;

/// Fallback font size when `config.font_px` is unset.
const DEFAULT_FONT_PX: f32 = 18.0;
/// Theme defaults (match the renderer's baked-in look): sidebar text and window background.
const DEFAULT_FG: [u8; 3] = [0xcc, 0xcc, 0xcc];
const DEFAULT_BG: [u8; 3] = [0x08, 0x08, 0x08]; // 0.03 * 255 ≈ 8
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
    // Scrollback viewport for the active pane (0 = live screen), with the last-seen history
    // depth and grid rows from the daemon for local clamping / page sizing.
    scroll_offset: usize,
    scroll_history: usize,
    active_rows: usize,
    heartbeat_ticks: u32,
    // Config-driven keybindings + the last config mtime we loaded, for hot-reload.
    keymap: Keymap,
    font_px: f32,
    config_mtime: Option<std::time::SystemTime>,
    /// M12: the flag-gated WebView2 browser pane (its own top-level window), opened on the first
    /// `Browse` request drained from the daemon.
    #[cfg(feature = "browser")]
    browser: Option<gmux_browser::BrowserPane>,
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

        // Load user config up front: font size feeds the atlas build, theme feeds the renderer.
        let user_config = Config::load();
        let font_px = user_config.font_px.unwrap_or(DEFAULT_FONT_PX);
        let keymap = Keymap::build(&user_config);
        let config_mtime = config_mtime();

        let mut renderer = Renderer::from_device(device, queue, format, font_px);
        apply_theme(&mut renderer, &user_config);

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

        // First launch ever: one welcome toast pointing at the two setup commands.
        if first_run(&state_dir()) {
            if let Some(nf) = &notifier {
                let _ = nf.show(&ToastRequest {
                    tag: "welcome".to_string(),
                    group: TOAST_GROUP.to_string(),
                    title: "gmux".to_string(),
                    body: "Run 'gmux hooks setup all' to get agent notifications, and 'gmux shell-integration --install' for prompt/cwd tracking.".to_string(),
                    urgency: NUrgency::Normal,
                    launch_arg: "welcome".to_string(),
                });
            }
        }

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
            scroll_offset: 0,
            scroll_history: 0,
            active_rows: 0,
            heartbeat_ticks: 0,
            keymap,
            font_px,
            config_mtime,
            #[cfg(feature = "browser")]
            browser: None,
        };
        st.sync_size();
        st.send_palette(&user_config); // theme the daemon's panes to match config
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
            WindowEvent::MouseWheel { delta, .. } => {
                if let Some(st) = self.state.as_mut() {
                    // Wheel up (positive y) scrolls deeper into history.
                    let lines = match delta {
                        MouseScrollDelta::LineDelta(_, y) => (y * 3.0).round() as i64,
                        MouseScrollDelta::PixelDelta(p) => (p.y / st.cell_dims().1 as f64).round() as i64,
                    };
                    st.scroll_by(lines);
                }
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                if !self.try_shortcut(&event) {
                    if let Some(bytes) = key_to_bytes(&event, self.mods) {
                        if let Some(st) = self.state.as_mut() {
                            st.scroll_offset = 0; // typing snaps back to the live screen
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

        // M12 (feature "browser"): drain queued Browse requests into the WebView2 pane.
        #[cfg(feature = "browser")]
        if let Ok(ResultBody::Browses(urls)) = st.client.call(Call::PollBrowse) {
            for url in urls {
                match &st.browser {
                    Some(b) => b.navigate(&url),
                    None => match gmux_browser::BrowserPane::open(&url) {
                        Ok(b) => st.browser = Some(b),
                        Err(e) => eprintln!("gmux: browser pane failed: {e}"),
                    },
                }
            }
        }

        // Re-send our geometry roughly once per second (30 of the 33ms poll ticks) so a
        // restarted daemon relearns pane sizes.
        st.heartbeat_ticks += 1;
        if st.heartbeat_ticks >= 30 {
            st.heartbeat_ticks = 0;
            st.sync_size();
            st.maybe_reload_config();
        }

        // Poll the daemon for fresh output by re-rendering.
        st.window.request_redraw();
        el.set_control_flow(ControlFlow::WaitUntil(Instant::now() + FRAME));
    }
}

impl App {
    /// Handle a gmux keyboard chord by dispatching the configured [`Action`] to the daemon.
    fn try_shortcut(&mut self, event: &KeyEvent) -> bool {
        let mods = self.mods;
        let Some(st) = self.state.as_mut() else { return false };

        if let Some(action) = st.keymap.action(mods, &event.logical_key) {
            st.dispatch(action);
            return true;
        }

        // Escape while scrolled snaps back to live (not a rebindable action; consumed here so the
        // pane never sees it).
        if let Key::Named(NamedKey::Escape) = &event.logical_key {
            if st.scroll_offset > 0 {
                st.scroll_offset = 0;
                st.window.request_redraw();
                return true;
            }
        }
        false
    }
}

impl State {
    /// Run a keybinding [`Action`] with the same side effects the old hardcoded matches had.
    fn dispatch(&mut self, action: Action) {
        // Layout/focus actions snap back to the live screen; the scroll actions must NOT (they
        // move the viewport). scroll_page already requests its own redraw.
        if !matches!(action, Action::ScrollPageUp | Action::ScrollPageDown) {
            self.scroll_offset = 0;
        }
        match action {
            Action::FocusLeft => self.client.control(Call::FocusPane { dir: "left".into() }),
            Action::FocusRight => self.client.control(Call::FocusPane { dir: "right".into() }),
            Action::FocusUp => self.client.control(Call::FocusPane { dir: "up".into() }),
            Action::FocusDown => self.client.control(Call::FocusPane { dir: "down".into() }),
            Action::SplitH => {
                self.client.control(Call::SplitPane { dir: "h".into(), command: None });
                self.sync_size();
            }
            Action::SplitV => {
                self.client.control(Call::SplitPane { dir: "v".into(), command: None });
                self.sync_size();
            }
            Action::ClosePane => {
                self.client.control(Call::ClosePane);
                self.sync_size();
            }
            Action::ToggleZoom => {
                self.client.control(Call::ToggleZoom);
                self.sync_size();
            }
            Action::NewWindow => {
                self.client.control(Call::NewWindow { command: None });
                self.sync_size();
            }
            Action::NextWindow => {
                self.client.control(Call::SwitchWindow { next: true });
                self.sync_size();
            }
            Action::PrevWindow => {
                self.client.control(Call::SwitchWindow { next: false });
                self.sync_size();
            }
            Action::ScrollPageUp => self.scroll_page(1),
            Action::ScrollPageDown => self.scroll_page(-1),
        }
        self.window.request_redraw();
    }

    /// If the config file's mtime changed since we last loaded, reload it: keys and theme apply
    /// live; a font-size change needs a renderer rebuild we don't do here, so it's logged and
    /// deferred to the next launch.
    fn maybe_reload_config(&mut self) {
        let now = config_mtime();
        if now == self.config_mtime {
            return;
        }
        self.config_mtime = now;
        let config = Config::load();
        self.keymap = Keymap::build(&config);
        apply_theme(&mut self.renderer, &config);
        self.send_palette(&config); // re-theme the daemon's panes on hot-reload
        let new_font = config.font_px.unwrap_or(DEFAULT_FONT_PX);
        if (new_font - self.font_px).abs() > f32::EPSILON {
            eprintln!("gmux: font size change requires a restart to take effect");
            self.font_px = new_font; // remember it so we don't warn again every reload
        }
        self.window.request_redraw();
    }

    fn cell_dims(&self) -> (u32, u32) {
        (self.renderer.cell_w().max(1), self.renderer.cell_h().max(1))
    }

    fn areas(&self) -> (u32, u32, u32) {
        let sidebar_w = self.renderer.sidebar_width().min(self.config.width / 3);
        let content_w = self.config.width.saturating_sub(sidebar_w).max(1);
        (sidebar_w, content_w, self.config.height)
    }

    /// Scroll the active pane's viewport by `lines` (positive = deeper into history), clamped
    /// locally to the last-seen history depth; the daemon clamps again server-side.
    fn scroll_by(&mut self, lines: i64) {
        let next = (self.scroll_offset as i64 + lines).clamp(0, self.scroll_history as i64) as usize;
        if next != self.scroll_offset {
            self.scroll_offset = next;
            self.window.request_redraw();
        }
    }

    /// Scroll by one page (`dir` = +1 up into history, -1 back toward live).
    fn scroll_page(&mut self, dir: i64) {
        let page = if self.active_rows > 1 { self.active_rows - 1 } else { 24 };
        self.scroll_by(dir * page as i64);
    }

    /// Tell the daemon our content geometry so it resizes its panes.
    fn sync_size(&mut self) {
        let (_, content_w, h) = self.areas();
        let (cw, ch) = self.cell_dims();
        self.client.control(Call::ResizeView { w: content_w, h, cell_w: cw, cell_h: ch });
    }

    /// Push `config`'s full terminal palette to the daemon (fg/bg + 16 system colors), which
    /// applies it to every pane. Sent once after connecting and on each config hot-reload. A
    /// pre-palette daemon rejects the unknown method; `control` discards the error, so old daemons
    /// simply keep their built-in colors.
    fn send_palette(&mut self, config: &Config) {
        self.client.control(palette_call(config));
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
        if layout.active_pane != self.active_pane {
            // The active pane changed daemon-side (e.g. the old one exited): the scroll offset
            // belonged to the previous pane, so snap the new one to its live screen.
            self.scroll_offset = 0;
        }
        self.active_pane = layout.active_pane;
        let rows: Vec<SidebarRow> = layout
            .tabs
            .iter()
            .map(|t| SidebarRow { name: t.name.clone(), branch: t.branch.clone(), attention: t.attention, active: t.active, progress: t.progress, progress_error: t.progress_error })
            .collect();

        // Update the taskbar attention badge / progress based on overall attention.
        if let Some(tb) = &self.taskbar {
            let any = layout.panes.iter().any(|p| p.attention);
            tb.set_progress(if any { NProgress::Paused } else { NProgress::None }, None);
        }

        let mut snaps: Vec<(PaneSnapshot, Attention, bool, Rect)> = Vec::new();
        for pr in &layout.panes {
            // Only the active pane scrolls; the rest always show the live screen.
            let offset = if pr.active { self.scroll_offset } else { 0 };
            if let Ok(ResultBody::Grid(g)) = self.client.call(Call::GetGrid { pane: pr.id, offset }) {
                if pr.active {
                    // Accept the server's clamp and remember the history depth / rows for
                    // local wheel clamping and page sizing.
                    self.scroll_offset = g.offset as usize;
                    self.scroll_history = g.history as usize;
                    self.active_rows = g.rows as usize;
                }
                let mut snap = grid_to_snapshot(&g);
                if g.offset > 0 {
                    // Scrolled into history: park the cursor off-grid so no cell draws it.
                    snap.cursor = (g.cols, g.rows);
                }
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
        // Present explicitly: dropping a SurfaceTexture does NOT present it — unpresented frames
        // exhaust the swapchain and every later acquire times out (window stays white/stale).
        self.renderer.queue.present(frame);
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

/// Last-modified time of the config file, or `None` if it doesn't exist / can't be stat'd.
fn config_mtime() -> Option<std::time::SystemTime> {
    std::fs::metadata(config_path()).and_then(|m| m.modified()).ok()
}

/// Build the `SetPalette` call from the config's resolved palette (defaults when no theme).
fn palette_call(config: &Config) -> Call {
    let p = config.palette();
    Call::SetPalette { fg: p.fg, bg: p.bg, ansi: p.ansi.to_vec() }
}

/// Push the config's theme (fg/bg, with the built-in defaults as fallback) into the renderer.
fn apply_theme(renderer: &mut Renderer, config: &Config) {
    let [fr, fg, fb] = config.fg(DEFAULT_FG);
    let [br, bg, bb] = config.bg(DEFAULT_BG);
    renderer.set_theme(Rgb { r: fr, g: fg, b: fb }, Rgb { r: br, g: bg, b: bb });
}

/// Where the first-run marker lives: `%LOCALAPPDATA%\gmux\state`.
fn state_dir() -> std::path::PathBuf {
    let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".to_string());
    std::path::PathBuf::from(base).join("gmux").join("state")
}

/// True exactly once per install: reports whether `dir/first-run` is absent and drops the marker.
/// Io errors are ignored — a failed marker just means the welcome toast may repeat next launch.
fn first_run(dir: &std::path::Path) -> bool {
    let marker = dir.join("first-run");
    if marker.exists() {
        return false;
    }
    let _ = std::fs::create_dir_all(dir);
    let _ = std::fs::write(&marker, "");
    true
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_run_reports_once_then_sees_the_marker() {
        let dir = std::env::temp_dir().join(format!("gmux-first-run-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(first_run(&dir), "fresh dir should be a first run");
        assert!(dir.join("first-run").exists(), "marker file should be created");
        assert!(!first_run(&dir), "second call should see the marker");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
