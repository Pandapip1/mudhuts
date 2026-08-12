mod gpu_term;
mod handlers;
mod hut;
mod input;
mod keybindings;
mod render;
mod stack;
mod state;
mod winit_backend;

use smithay::reexports::calloop::EventLoop;
use smithay::reexports::wayland_server::Display;

pub use state::State;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_logging();

    let mut event_loop: EventLoop<'static, State> = EventLoop::try_new()?;
    let display: Display<State> = Display::new()?;

    let socket = state::create_socket()?;

    // Point each Hut's shell (and anything launched from it) at mudhuts'
    // own socket, *without* touching the compositor's own process-wide
    // `WAYLAND_DISPLAY` — the winit backend still needs that pointed at
    // whatever mudhuts itself is nested inside, read when `init_winit`
    // runs below.
    let socket_name = socket.1.to_string_lossy().into_owned();
    let extra_env = vec![("WAYLAND_DISPLAY".to_string(), socket_name)];
    let (hut, term_events) = hut::Hut::spawn(extra_env.clone())?;

    let (redraw_ping, redraw_ping_source) = smithay::reexports::calloop::ping::make_ping()?;
    let loop_handle = event_loop.handle();
    let stack = stack::HutStack::new(hut, term_events, loop_handle, extra_env)?;
    let mut state = State::new(&mut event_loop, display, stack, socket, redraw_ping)?;

    winit_backend::init_winit(&mut event_loop, &mut state, redraw_ping_source)?;

    event_loop.run(None, &mut state, |_| {})?;

    Ok(())
}

fn init_logging() {
    if let Ok(env_filter) = tracing_subscriber::EnvFilter::try_from_default_env() {
        tracing_subscriber::fmt().with_env_filter(env_filter).init();
    } else {
        tracing_subscriber::fmt().init();
    }
}
