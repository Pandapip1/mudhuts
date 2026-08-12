mod gpu_term;
mod handlers;
mod hut;
mod input;
mod keybindings;
mod render;
mod state;
mod winit_backend;

use smithay::reexports::calloop::EventLoop;
use smithay::reexports::wayland_server::Display;

pub use state::State;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_logging();

    let mut event_loop: EventLoop<State> = EventLoop::try_new()?;
    let display: Display<State> = Display::new()?;

    let socket = state::create_socket()?;

    // Point the Hut's shell (and anything launched from it) at mudhuts'
    // own socket, *without* touching the compositor's own process-wide
    // `WAYLAND_DISPLAY` — the winit backend still needs that pointed at
    // whatever mudhuts itself is nested inside, read when `init_winit`
    // runs below.
    let socket_name = socket.1.to_string_lossy().into_owned();
    let (hut, term_events) = hut::Hut::spawn([("WAYLAND_DISPLAY".to_string(), socket_name)])?;

    let mut state = State::new(&mut event_loop, display, hut, socket)?;

    event_loop
        .handle()
        .insert_source(term_events, |event, _, _state| {
            if let smithay::reexports::calloop::channel::Event::Msg(event) = event {
                match event {
                    mudhuts_term::TermEvent::Title(title) => tracing::debug!("hut title: {title}"),
                    mudhuts_term::TermEvent::Exited => tracing::info!("hut shell exited"),
                    mudhuts_term::TermEvent::Wakeup => {}
                }
            }
        })?;

    winit_backend::init_winit(&mut event_loop, &mut state)?;

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
