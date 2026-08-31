mod autostart;
mod chrome;
mod chrome_config;
mod config;
mod cursor;
mod docks;
mod gpu_term;
mod grabs;
mod graph;
mod graph_nodes;
mod graph_stack;
mod handlers;
mod console_hut;
mod input;
mod keybindings;
mod main_window;
mod malloc;
mod ownership;
mod perf_config;
mod redraw;
mod render;
mod rt_sched;
mod space_element;
mod state;
mod switcher;
mod theme;
#[cfg(test)]
mod test_support;
mod udev_backend;
mod hut;
mod village_chrome;
mod winit_backend;

use smithay::reexports::calloop::EventLoop;
use smithay::reexports::wayland_server::Display;

pub use state::State;

fn main() -> std::process::ExitCode {
    // Must run before anything else has a chance to allocate — see
    // `malloc`'s module doc.
    malloc::limit_mmap_threshold(128 * 1024);
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

/// A helper program to spawn once mudhuts itself is up, and the (already
/// helper-specific) arguments to launch it with — see `parse_args`.
struct AuthorityHelper {
    path: String,
    args: Vec<String>,
}

struct Args {
    tty: bool,
    authority_helper: Option<AuthorityHelper>,
}

/// `--tty` (see the plan's Phase 7 notes) and `--authority-helper <path>
/// [args...]` (Phase 5b — everything after the path is that helper's own
/// command line, not mudhuts' own, since it needs to parse its own
/// `--sub`/`--alert` rules; see `handlers/shell.rs`'s module doc for the
/// trust model this establishes) can both be given, in any order.
fn parse_args() -> Args {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let tty = args.iter().any(|a| a == "--tty");
    let authority_helper = args
        .iter()
        .position(|a| a == "--authority-helper")
        .and_then(|i| {
            let path = args.get(i + 1)?.clone();
            let helper_args = args.get(i + 2..).map(<[String]>::to_vec).unwrap_or_default();
            Some(AuthorityHelper { path, args: helper_args })
        });
    Args { tty, authority_helper }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = parse_args();

    let mut event_loop: EventLoop<'static, State> = EventLoop::try_new()?;
    let display: Display<State> = Display::new()?;

    let socket = state::create_socket()?;

    // Point each ConsoleHut's shell (and anything launched from it) at mudhuts'
    // own socket, *without* touching the compositor's own process-wide
    // `WAYLAND_DISPLAY` — the winit backend still needs that pointed at
    // whatever mudhuts itself is nested inside, read when `init_winit`
    // runs below.
    let socket_name = socket.1.to_string_lossy().into_owned();
    let extra_env = vec![("WAYLAND_DISPLAY".to_string(), socket_name.clone())];
    // Spawned before any backend/output exists, so the real scale isn't
    // known yet — starts at 1.0 and gets caught up by
    // `Stack::rescale_all` once `init_udev`/`init_winit` below learn
    // the real value (see `ConsoleHut::rescale`'s doc comment).
    let (hut, term_events) = console_hut::ConsoleHut::spawn(extra_env.clone(), 1.0)?;

    let (redraw_ping, redraw_ping_source) = smithay::reexports::calloop::ping::make_ping()?;
    let loop_handle = event_loop.handle();
    let stack = graph_stack::GraphStack::new(
        hut,
        term_events,
        loop_handle,
        extra_env.clone(),
        redraw::RedrawHandle::new(redraw_ping.clone()),
    )?;
    let mut state = State::new(&mut event_loop, display, stack, socket, redraw_ping)?;

    if let Some(helper) = &args.authority_helper {
        // Trust model hinges on this being the *only* process that ever
        // sees `state.authority_token`: set directly on this one child's
        // environment, never written to a file, logged, or otherwise
        // exposed — see `handlers/shell.rs`'s module doc.
        match std::process::Command::new(&helper.path)
            .args(&helper.args)
            .env("WAYLAND_DISPLAY", &socket_name)
            .env("MUDHUTS_AUTHORITY_TOKEN", &state.authority_token)
            .spawn()
        {
            Ok(_child) => tracing::info!("spawned authority helper {:?}", helper.path),
            Err(err) => tracing::error!("failed to spawn authority helper {:?}: {err}", helper.path),
        }
    }

    // Explicit opt-in only — no auto-detection from absent
    // `WAYLAND_DISPLAY`/`DISPLAY`. `--tty` is a real seat/DRM-owning
    // backend with real safety stakes (it can seize DRM master / switch
    // VTs), so it should never activate by surprise.
    if args.tty {
        // Gated to a real session only — under winit (nested/dev testing)
        // this would spawn a user's entire real desktop autostart set
        // into a throwaway test window every time, which nobody wants.
        autostart::run(&mut state.stack);
        // Same real-session gating, for a different reason: SCHED_FIFO
        // is only useful for the thread actually driving DRM
        // commits/input under real hardware, and grabbing real-time
        // scheduling on a host desktop just to run a nested dev/test
        // instance has no upside and a real (if unlikely) downside —
        // see `perf_config.rs`'s `PerfConfig::sched_fifo` doc comment.
        // Applied before `init_udev` so the whole session-setup/render
        // path runs under it from the start; still a plain no-op unless
        // `[performance]` opts in (default off) and the launching
        // session/service granted the needed capability/limit.
        if state.perf_config.sched_fifo {
            rt_sched::apply(state.perf_config.sched_fifo_priority);
        }
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
