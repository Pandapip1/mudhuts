//! Generated bindings for mudhuts' custom Wayland protocol extensions
//! (`mudhuts_shell_v1` and `mudhuts_window_role_v1` — see
//! `protocol/mudhuts-shell.xml`). Mirrors the exact pattern
//! `wayland-protocols` itself uses internally (its `wayland_protocol!`
//! macro in `protocol_macro.rs`): `wayland-scanner`'s `generate_*_code!`
//! macros are invoked directly here, no `build.rs` needed — the XML path
//! resolves relative to this crate's own `CARGO_MANIFEST_DIR`.
//!
//! `mudhuts` itself only ever needs the `server` side; `client` exists
//! purely for the Phase 5 test client, since no existing application
//! speaks this protocol to test against otherwise.

#[cfg(feature = "server")]
pub mod server {
    //! Server-side API — what `mudhuts` itself dispatches against.
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
