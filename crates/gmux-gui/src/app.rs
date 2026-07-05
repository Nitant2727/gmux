//! The windowed app: a winit window + wgpu surface rendering a single [`Pane`], with keyboard
//! input routed to the pane and the grid re-rendered on output. One pane for M1; splits/tabs land
//! in M3.

use std::sync::Arc;
use std::time::{Duration, Instant};

use gmux_mux::{Pane, PaneEvent, PtySize};
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

struct State {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    renderer: Renderer,
    pane: Pane,
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

        self.state = Some(State { window, surface, config, renderer, pane });
        el.set_control_flow(ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(16)));
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(st) = self.state.as_mut() else { return };
        match event {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),
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
        for ev in st.pane.drain_events() {
            match ev {
                PaneEvent::Output | PaneEvent::Notification(_) | PaneEvent::Bell => redraw = true,
                PaneEvent::Title(t) => st.window.set_title(&format!("gmux — {t}")),
                PaneEvent::Exited => el.exit(),
                _ => {}
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
