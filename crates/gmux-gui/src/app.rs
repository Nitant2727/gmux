//! The windowed app: a winit window + wgpu surface rendering a single [`Pane`], with keyboard
//! input routed to the pane and the grid re-rendered on output. One pane for M1; splits/tabs land
//! in M3.

use std::sync::Arc;
use std::time::{Duration, Instant};

use gmux_mux::{Notification, Pane, PaneEvent, ProgressState, PtySize, Urgency};
use gmux_notify::{
    flash_window, Notifier, ProgressState as NProgress, Taskbar, ToastRequest, Urgency as NUrgency,
};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

use crate::Renderer;

const FONT_PX: f32 = 18.0;

pub struct App {
    shell: String,
    mods: ModifiersState,
    state: Option<State>,
}

const TOAST_GROUP: &str = "gmux-session";
const TOAST_MIN_INTERVAL: Duration = Duration::from_millis(1000);

struct State {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    renderer: Renderer,
    pane: Pane,
    /// Whether the gmux window currently has focus (drives toast suppression).
    focused: bool,
    hwnd: isize,
    notifier: Option<Notifier>,
    taskbar: Option<Taskbar>,
    last_toast: Option<Instant>,
}

impl State {
    fn toast_tag(&self) -> String {
        format!("pane-{}", self.pane.id.0)
    }

    /// Fire a Windows toast + taskbar flash for a notification (rate-limited per pane).
    fn fire_toast(&mut self, n: &Notification) {
        let now = Instant::now();
        if let Some(prev) = self.last_toast {
            if now.duration_since(prev) < TOAST_MIN_INTERVAL {
                return; // collapse bursts
            }
        }
        self.last_toast = Some(now);
        let title = if n.title.is_empty() { "gmux".to_string() } else { n.title.clone() };
        let req = ToastRequest {
            tag: self.toast_tag(),
            group: TOAST_GROUP.to_string(),
            title,
            body: n.body.clone(),
            urgency: map_urgency(n.urgency),
            launch_arg: format!("pane={}", self.pane.id.0),
        };
        if let Some(nf) = &self.notifier {
            let _ = nf.show(&req);
        }
        flash_window(self.hwnd, true);
    }

    /// Clear attention on focus: mark pane read, remove toast, stop the taskbar flash.
    fn clear_attention(&self) {
        if let Some(nf) = &self.notifier {
            nf.clear(&self.toast_tag(), TOAST_GROUP);
        }
        flash_window(self.hwnd, false);
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

impl App {
    pub fn new(shell: String) -> Self {
        App { shell, mods: ModifiersState::empty(), state: None }
    }
}

/// Run the gmux GUI with the given shell command line. Blocks until the window closes.
pub fn run(shell: String) -> Result<(), Box<dyn std::error::Error>> {
    let event_loop = EventLoop::new()?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut app = App::new(shell);
    event_loop.run_app(&mut app)?;
    Ok(())
}

impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }
        let window = Arc::new(
            el.create_window(Window::default_attributes().with_title("gmux"))
                .expect("create window"),
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
        let cols = (config.width / renderer.cell_w()).max(1) as u16;
        let rows = (config.height / renderer.cell_h()).max(1) as u16;
        let pane = Pane::spawn(&self.shell, PtySize { cols, rows }).expect("spawn shell");

        let hwnd = window_hwnd(&window).unwrap_or(0);
        let notifier = Notifier::new("com.gmux.app", "gmux").ok();
        let taskbar = if hwnd != 0 { Taskbar::new(hwnd) } else { None };

        self.state = Some(State {
            window,
            surface,
            config,
            renderer,
            pane,
            focused: true,
            hwnd,
            notifier,
            taskbar,
            last_toast: None,
        });
        el.set_control_flow(ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(16)));
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(st) = self.state.as_mut() else { return };
        match event {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),
            WindowEvent::Focused(f) => {
                st.focused = f;
                if f {
                    // Focusing the window focuses its pane: clear attention, remove toast, stop flash.
                    st.pane.focus();
                    st.clear_attention();
                    st.window.request_redraw();
                }
            }
            WindowEvent::Resized(sz) => {
                st.config.width = sz.width.max(1);
                st.config.height = sz.height.max(1);
                st.surface.configure(&st.renderer.device, &st.config);
                let cols = (st.config.width / st.renderer.cell_w()).max(1) as u16;
                let rows = (st.config.height / st.renderer.cell_h()).max(1) as u16;
                let _ = st.pane.resize(PtySize { cols, rows });
                st.window.request_redraw();
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                if let Some(bytes) = key_to_bytes(&event, self.mods) {
                    let _ = st.pane.write(&bytes);
                }
                st.pane.focus(); // typing clears attention
            }
            WindowEvent::RedrawRequested => st.render(),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        let Some(st) = self.state.as_mut() else { return };
        let mut redraw = false;
        let events = st.pane.drain_events();
        for ev in events {
            match ev {
                PaneEvent::Output => redraw = true,
                PaneEvent::Notification(n) => {
                    redraw = true; // attention ring
                    if !st.focused {
                        st.fire_toast(&n); // toast + flash, only when we're not looking
                    }
                }
                PaneEvent::Bell => {
                    redraw = true;
                    if !st.focused {
                        flash_window(st.hwnd, true);
                    }
                }
                PaneEvent::Progress { state, pct } => {
                    if let Some(tb) = &st.taskbar {
                        tb.set_progress(map_progress(state), pct);
                    }
                }
                PaneEvent::Title(t) => st.window.set_title(&format!("gmux — {t}")),
                PaneEvent::Cwd(_) => {}
                PaneEvent::Exited => el.exit(),
            }
        }

        // A clicked toast focuses the target pane's window.
        if let Some(nf) = &st.notifier {
            if !nf.poll_activations().is_empty() {
                st.window.focus_window();
                st.pane.focus();
                st.clear_attention();
                redraw = true;
            }
        }

        if redraw {
            st.window.request_redraw();
        }
        el.set_control_flow(ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(16)));
    }
}

impl State {
    fn render(&mut self) {
        use wgpu::CurrentSurfaceTexture::{Success, Suboptimal};
        let frame = match self.surface.get_current_texture() {
            Success(t) | Suboptimal(t) => t,
            _ => {
                // Outdated / lost / timeout / occluded — reconfigure and skip this frame.
                self.surface.configure(&self.renderer.device, &self.config);
                return;
            }
        };
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let snap = self.pane.snapshot();
        self.renderer.render(&view, &snap, self.pane.attention(), self.config.width, self.config.height);
        // `frame` presents to the surface when it drops at end of scope (wgpu 30).
    }
}

/// Translate a key press into the bytes to send to the PTY. Covers the common cases; full
/// win32-input-mode fidelity comes later (ARCHITECTURE §5.3).
fn key_to_bytes(event: &winit::event::KeyEvent, mods: ModifiersState) -> Option<Vec<u8>> {
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
            // Ctrl+<letter> -> control code.
            if mods.control_key() {
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
