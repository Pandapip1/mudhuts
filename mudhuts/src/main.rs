mod chrome;
mod cursor;
mod docks;
mod gpu_term;
mod grabs;
mod handlers;
mod hut;
mod input;
mod keybindings;
mod main_window;
mod ownership;
mod render;
mod stack;
mod state;
mod switcher;
mod udev_backend;
mod winit_backend;

use smithay::reexports::calloop::EventLoop;
use smithay::reexports::wayland_server::Display;

pub use state::State;

fn main() -> std::process::ExitCode {
    init_logging();

    // Routed through `tracing::error!` (not the default `Termination`
    // impl's `Debug`-printed-to-stderr behavior) so a startup failure
    // reaches journald the same way panics and everything else do — see
    // `init_logging`'s doc comment for why that matters here specifically.
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!("{err}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
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

    // Explicit opt-in only — no auto-detection from absent
    // `WAYLAND_DISPLAY`/`DISPLAY`. `--tty` is a real seat/DRM-owning
    // backend with real safety stakes (it can seize DRM master / switch
    // VTs), so it should never activate by surprise.
    if std::env::args().nth(1).as_deref() == Some("--tty") {
        udev_backend::init_udev(&mut event_loop, &mut state, redraw_ping_source)?;
    } else {
        winit_backend::init_winit(&mut event_loop, &mut state, redraw_ping_source)?;
    }

    event_loop.run(None, &mut state, |_| {})?;

    Ok(())
}

/// Sets up logging to both stderr (for nested/dev-shell testing, where
/// it's visible directly) and the systemd journal via `tracing-journald`
/// (which talks to journald's own socket directly, so it's captured no
/// matter what does or doesn't capture this process's stdout/stderr —
/// under the udev/TTY backend that's normally a greeter/session manager,
/// which turned out not to forward or log a launched session's output
/// anywhere accessible). Also installs a panic hook that logs the panic
/// through `tracing` (in addition to the default hook's own stderr
/// output), for the same reason: a panic that only reaches stderr is
/// invisible when nothing captures stderr.
fn init_logging() {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    let journald_layer = match tracing_journald::layer() {
        Ok(layer) => Some(layer),
        Err(err) => {
            eprintln!("failed to connect to the systemd journal, logging to stderr only: {err}");
            None
        }
    };

    tracing_subscriber::registry()
        .with(env_filter)
        .with(tracing_subscriber::fmt::layer())
        .with(journald_layer)
        .init();

    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!("{info}");
        default_hook(info);
    }));
}
