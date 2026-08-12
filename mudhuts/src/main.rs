mod handlers;
mod hut;
mod input;
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

    let (hut, term_events) = hut::Hut::spawn()?;

    let mut state = State::new(&mut event_loop, display, hut)?;

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

    // So client processes launched from the built-in terminal connect to
    // mudhuts rather than whatever compositor mudhuts itself is nested in.
    unsafe { std::env::set_var("WAYLAND_DISPLAY", &state.socket_name) };

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
