//! Phase 5b's standalone helper program: tags *other* clients' toplevels
//! (ones that don't speak `mudhuts_shell_v1` natively) as Sub-Windows or
//! Alerts of some other Main Window, by simple exact `app_id` rules given
//! on the command line. Trusted by mudhuts because mudhuts itself is what
//! spawns it (`mudhuts --authority-helper <path to this binary>`), handing
//! it the one-time secret token via the `MUDHUTS_AUTHORITY_TOKEN`
//! environment variable — see `mudhuts/src/handlers/shell.rs`'s module doc
//! for the full trust model.
//!
//! Usage: `mudhuts-authority-helper --sub TARGET_APP_ID:MAIN_APP_ID`
//! (repeatable, `--alert` for the other role). A toplevel matching
//! TARGET_APP_ID is tagged the moment a *currently known* toplevel matches
//! MAIN_APP_ID — if the main window hasn't appeared yet when the target
//! does, that target is never retried; a known, accepted limitation for
//! this first pass (see mudhuts' `project_known_issues` memory).

use std::collections::HashMap;
use std::env;
use std::process::ExitCode;
use std::sync::{Arc, Mutex, MutexGuard};

use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_registry;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};

use wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1::{
    self, ExtForeignToplevelHandleV1,
};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_list_v1::{
    self, ExtForeignToplevelListV1,
};

use mudhuts_protocols::client::mudhuts_shell_authority_v1::MudhutsShellAuthorityV1;
use mudhuts_protocols::client::mudhuts_shell_v1::MudhutsShellV1;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Sub,
    Alert,
}

struct Rule {
    role: Role,
    target_app_id: String,
    main_app_id: String,
}

/// Parses `TARGET:MAIN` — an exact-match pair of app_ids, not a glob/regex
/// (kept deliberately simple for this first pass).
fn parse_rule(role: Role, spec: &str) -> Option<Rule> {
    let (target, main) = spec.split_once(':')?;
    Some(Rule {
        role,
        target_app_id: target.to_string(),
        main_app_id: main.to_string(),
    })
}

#[derive(Default, Clone)]
struct ToplevelInfo {
    identifier: Option<String>,
    app_id: String,
    /// Already handed to `set_sub`/`set_alert` — a toplevel only ever
    /// gets tagged once, even if further `done` events arrive later
    /// (e.g. its title changing).
    tagged: bool,
}

struct AppState {
    authority: Option<MudhutsShellAuthorityV1>,
    rules: Vec<Rule>,
    /// Every toplevel seen so far, keyed by its Wayland object id (stable
    /// for the object's lifetime, unlike anything content-derived) —
    /// searched by `app_id` both to find a match for an incoming target
    /// and to serve as a "main window" candidate for a later target.
    toplevels: HashMap<u32, Arc<Mutex<ToplevelInfo>>>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for AppState {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Every global this one-shot helper needs is bound right after
        // the initial roundtrip in `main` — dynamic add/remove afterward
        // isn't handled.
    }
}

impl Dispatch<ExtForeignToplevelListV1, ()> for AppState {
    fn event(
        _: &mut Self,
        _list: &ExtForeignToplevelListV1,
        event: ext_foreign_toplevel_list_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // `Event::Toplevel` itself carries no data worth reacting to here
        // — the handle it creates is tracked via `event_created_child!`
        // below, and everything interesting arrives as *that* object's
        // own events (title/app_id/identifier/done).
        if let ext_foreign_toplevel_list_v1::Event::Finished = event {
            eprintln!("ext_foreign_toplevel_list_v1 finished — compositor is tearing it down");
        }
    }

    wayland_client::event_created_child!(AppState, ExtForeignToplevelListV1, [
        ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE => (ExtForeignToplevelHandleV1, Arc::new(Mutex::new(ToplevelInfo::default())))
    ]);
}

impl Dispatch<ExtForeignToplevelHandleV1, Arc<Mutex<ToplevelInfo>>> for AppState {
    fn event(
        state: &mut Self,
        handle: &ExtForeignToplevelHandleV1,
        event: ext_foreign_toplevel_handle_v1::Event,
        data: &Arc<Mutex<ToplevelInfo>>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_foreign_toplevel_handle_v1::Event;
        match event {
            Event::AppId { app_id } => data.lock().unwrap().app_id = app_id,
            Event::Identifier { identifier } => data.lock().unwrap().identifier = Some(identifier),
            Event::Done => {
                let id = handle.id().protocol_id();
                state.toplevels.insert(id, data.clone());
                state.try_tag(&mut data.lock().unwrap());
            }
            Event::Closed => {
                state.toplevels.remove(&handle.id().protocol_id());
            }
            // `Event::Title` deliberately unused — matching is by app_id
            // only in this first pass.
            _ => {}
        }
    }
}

impl AppState {
    /// If `info` (a just-`done` toplevel) matches some rule's target
    /// app_id, and a currently-known toplevel matches that rule's main
    /// app_id, tag it now. A no-op if it's already tagged, or nothing
    /// matches yet — see this module's doc on why an unmatched target
    /// isn't retried later.
    fn try_tag(&self, info: &mut MutexGuard<'_, ToplevelInfo>) {
        if info.tagged {
            return;
        }
        let Some(identifier) = &info.identifier else {
            return;
        };
        let Some(authority) = &self.authority else {
            return;
        };
        for rule in &self.rules {
            if rule.target_app_id != info.app_id {
                continue;
            }
            let Some(main_identifier) = self.toplevels.values().find_map(|other| {
                let other = other.lock().unwrap();
                (other.app_id == rule.main_app_id)
                    .then(|| other.identifier.clone())
                    .flatten()
            }) else {
                continue;
            };
            match rule.role {
                Role::Sub => authority.set_sub(identifier.clone(), main_identifier.clone()),
                Role::Alert => authority.set_alert(identifier.clone(), main_identifier.clone()),
            }
            println!(
                "tagged {identifier} ({}) as a {} of {main_identifier} ({})",
                info.app_id,
                match rule.role {
                    Role::Sub => "Sub-Window",
                    Role::Alert => "Alert",
                },
                rule.main_app_id
            );
            info.tagged = true;
            return;
        }
    }
}

wayland_client::delegate_noop!(AppState: ignore MudhutsShellV1);
wayland_client::delegate_noop!(AppState: ignore MudhutsShellAuthorityV1);

fn parse_args() -> Result<Vec<Rule>, String> {
    let mut rules = Vec::new();
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        let role = match arg.as_str() {
            "--sub" => Role::Sub,
            "--alert" => Role::Alert,
            other => return Err(format!("unrecognized argument {other:?}")),
        };
        let spec = args
            .next()
            .ok_or_else(|| format!("{arg} needs a TARGET_APP_ID:MAIN_APP_ID argument"))?;
        let rule = parse_rule(role, &spec).ok_or_else(|| format!("bad rule {spec:?}, expected TARGET:MAIN"))?;
        rules.push(rule);
    }
    Ok(rules)
}

fn main() -> ExitCode {
    let rules = match parse_args() {
        Ok(rules) => rules,
        Err(err) => {
            eprintln!("{err}");
            eprintln!("usage: mudhuts-authority-helper [--sub|--alert TARGET_APP_ID:MAIN_APP_ID]...");
            return ExitCode::FAILURE;
        }
    };
    if rules.is_empty() {
        eprintln!("no --sub/--alert rules given — nothing to do");
        return ExitCode::FAILURE;
    }

    let Ok(token) = env::var("MUDHUTS_AUTHORITY_TOKEN") else {
        eprintln!("MUDHUTS_AUTHORITY_TOKEN not set — this helper must be spawned by mudhuts itself");
        return ExitCode::FAILURE;
    };

    let conn = match Connection::connect_to_env() {
        Ok(conn) => conn,
        Err(err) => {
            eprintln!("failed to connect to the Wayland display: {err}");
            return ExitCode::FAILURE;
        }
    };

    let (globals, mut event_queue) = match registry_queue_init::<AppState>(&conn) {
        Ok(pair) => pair,
        Err(err) => {
            eprintln!("failed to initialize globals: {err}");
            return ExitCode::FAILURE;
        }
    };
    let qh = event_queue.handle();
    let mut state = AppState {
        authority: None,
        rules,
        toplevels: HashMap::new(),
    };

    let shell: MudhutsShellV1 = match globals.bind(&qh, 2..=2, ()) {
        Ok(s) => s,
        Err(err) => {
            eprintln!(
                "mudhuts_shell_v1 (v2, for get_authority) not available ({err}) — is this running under mudhuts?"
            );
            return ExitCode::FAILURE;
        }
    };
    // Binding is enough — the `toplevel` events it starts sending
    // immediately are what this helper actually cares about (see the
    // `Dispatch<ExtForeignToplevelListV1, _>` impl above), not any
    // request on the object itself.
    let _foreign_list: ExtForeignToplevelListV1 = match globals.bind(&qh, 1..=1, ()) {
        Ok(l) => l,
        Err(err) => {
            eprintln!("ext_foreign_toplevel_list_v1 not available: {err}");
            return ExitCode::FAILURE;
        }
    };

    let authority = shell.get_authority(&qh, ());
    authority.authenticate(token);
    state.authority = Some(authority);

    if let Err(err) = event_queue.roundtrip(&mut state) {
        eprintln!("initial roundtrip failed (bad token?): {err}");
        return ExitCode::FAILURE;
    }

    println!("authenticated — watching for toplevels to tag, Ctrl+C to stop");
    loop {
        if let Err(err) = event_queue.blocking_dispatch(&mut state) {
            eprintln!("event queue error: {err}");
            return ExitCode::FAILURE;
        }
    }
}
