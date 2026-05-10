{
  description = "Nicotine — high-performance EVE Online multiboxing tool";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };

        runtimeLibs = with pkgs; [
          libGL
          wayland
          libxkbcommon
          xorg.libX11
          xorg.libxcb
          xorg.libXcursor
          xorg.libXrandr
          xorg.libXi
        ];

        runtimeTools = [ pkgs.wmctrl ];
      in
      {
        devShells.default = pkgs.mkShell {
          nativeBuildInputs = with pkgs; [
            rustc
            cargo
            rustfmt
            clippy
            rust-analyzer
            pkg-config
          ];

          buildInputs = runtimeLibs ++ runtimeTools;

          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath runtimeLibs;

          shellHook = ''
            echo "nicotine dev shell — rustc $(rustc --version | cut -d' ' -f2), wmctrl on PATH"
          '';
        };
      });
}
