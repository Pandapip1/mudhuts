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
          ];

          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath runtimeLibs;

          # So `wayland-scanner`/`wayland-client-sys`-style build scripts and
          # `pkg-config` probes for wayland-protocols find the .xml schema files.
          WAYLAND_PROTOCOLS_DIR = "${pkgs.wayland-protocols}/share/wayland-protocols";
        };
      });
}
