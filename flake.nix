{
  description = "mudhuts - a smithay-based compositor with a built-in terminal emulator";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };

        runtimeLibs = with pkgs; [
          wayland
          libGL
          libxkbcommon
          libinput
          mesa
          libglvnd
          libgbm
          libdrm
          udev
          seatd
          dbus
          fontconfig
          freetype
          pixman
          pipewire
        ];
      in
      {
        devShells.default = pkgs.mkShell {
          name = "mudhuts-dev";

          nativeBuildInputs = with pkgs; [
            pkg-config
            rustc
            cargo
            rust-analyzer
            clippy
            rustfmt
            # `libspa-sys`'s build script (part of `mudhuts-portal`'s
            # `pipewire` dependency) uses `bindgen` to generate FFI
            # bindings from PipeWire/SPA's C headers — this is nixpkgs'
            # standard hook for that, setting up `LIBCLANG_PATH` and
            # `BINDGEN_EXTRA_CLANG_ARGS` (glibc's own include path) so
            # clang can find both libclang itself and the standard C
            # headers it needs to parse the wrapper header.
            rustPlatform.bindgenHook
          ];

          buildInputs = with pkgs; [
            wayland
            wayland-protocols
            libxkbcommon
            libinput
            mesa
            libglvnd
            libgbm
            libdrm
            udev
            seatd
            dbus
            fontconfig
            freetype
            pixman
            pipewire
          ];

          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath runtimeLibs;

          # So `wayland-scanner`/`wayland-client-sys`-style build scripts and
          # `pkg-config` probes for wayland-protocols find the .xml schema files.
          WAYLAND_PROTOCOLS_DIR = "${pkgs.wayland-protocols}/share/wayland-protocols";
        };
      });
}
