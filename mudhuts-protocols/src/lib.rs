//! Generated bindings for mudhuts' custom Wayland protocol extensions
//! (`mudhuts_shell_v1`/`mudhuts_window_role_v1` from
//! `protocol/mudhuts-shell.xml`, and `mudhuts_power_button_manager_v1`/
//! `mudhuts_power_button_v1` from `protocol/mudhuts-power-button.xml`).
//! Mirrors the exact pattern `wayland-protocols` itself uses internally
//! (its `wayland_protocol!` macro in `protocol_macro.rs`):
//! `wayland-scanner`'s `generate_*_code!` macros are invoked directly
//! here, no `build.rs` needed — the XML path resolves relative to this
//! crate's own `CARGO_MANIFEST_DIR`. Each protocol gets its own inner
//! module (with its own `__interfaces` submodule) rather than sharing
//! one — `generate_interfaces!` defines module-scoped helper items
//! (`SyncWrapper`, `types_null`, ...) that collide if two invocations
//! ever share a scope; every interface itself is still re-exported at
//! the top of `server`/`client` via `pub use`, so call sites reach
//! `mudhuts_protocols::server::mudhuts_power_button_v1::...` exactly as
//! before this split.
//!
//! `mudhuts` itself only ever needs the `server` side; `client` exists
//! purely for the Phase 5 test client, since no existing application
//! speaks `mudhuts_shell_v1` natively to test against otherwise (the
//! power-button protocol has no client-side bindings yet — nothing
//! outside mudhuts itself has needed to speak it as a client).

#[cfg(feature = "server")]
pub mod server {
    //! Server-side API — what `mudhuts` itself dispatches against.

    mod shell {
        #![allow(dead_code, non_camel_case_types, unused_unsafe, unused_variables)]
        #![allow(non_upper_case_globals, non_snake_case, unused_imports)]
        #![allow(missing_docs, clippy::all)]

        use wayland_server;
        use wayland_server::protocol::*;

        pub mod __interfaces {
            use wayland_protocols::xdg::shell::server::__interfaces::*;
            use wayland_server::protocol::__interfaces::*;
            wayland_scanner::generate_interfaces!("./protocol/mudhuts-shell.xml");
        }
        use self::__interfaces::*;
        use wayland_protocols::xdg::shell::server::xdg_toplevel;

        wayland_scanner::generate_server_code!("./protocol/mudhuts-shell.xml");
    }
    pub use shell::{
        mudhuts_shell_authority_v1, mudhuts_shell_v1, mudhuts_window_role_v1,
    };

    mod power_button {
        #![allow(dead_code, non_camel_case_types, unused_unsafe, unused_variables)]
        #![allow(non_upper_case_globals, non_snake_case, unused_imports)]
        #![allow(missing_docs, clippy::all)]

        use wayland_server;
        use wayland_server::protocol::*;

        pub mod __interfaces {
            use wayland_server::protocol::__interfaces::*;
            wayland_scanner::generate_interfaces!("./protocol/mudhuts-power-button.xml");
        }
        use self::__interfaces::*;

        wayland_scanner::generate_server_code!("./protocol/mudhuts-power-button.xml");
    }
    pub use power_button::{mudhuts_power_button_manager_v1, mudhuts_power_button_v1};
}

#[cfg(feature = "client")]
pub mod client {
    //! Client-side API — only used by the Phase 5 test client.
    #![allow(dead_code, non_camel_case_types, unused_unsafe, unused_variables)]
    #![allow(non_upper_case_globals, non_snake_case, unused_imports)]
    #![allow(missing_docs, clippy::all)]

    use wayland_client;
    use wayland_client::protocol::*;

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        use wayland_protocols::xdg::shell::client::__interfaces::*;
        wayland_scanner::generate_interfaces!("./protocol/mudhuts-shell.xml");
    }
    use self::__interfaces::*;
    use wayland_protocols::xdg::shell::client::xdg_toplevel;

    wayland_scanner::generate_client_code!("./protocol/mudhuts-shell.xml");
}
