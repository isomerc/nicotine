# Development shell for Nicotine. Provides:
#   - Linux build deps (matches pkgs/nicotine.nix consumer)
#   - cargo-xwin + clang + llvm so `cargo xwin clippy --target
#     x86_64-pc-windows-msvc -- -D warnings` mirrors CI locally
#
# Usage: `nix-shell` (or via direnv with an `.envrc` containing
# `use nix`). The first `cargo xwin` run downloads several hundred MB
# of MSVC SDK + CRT into ~/.cache/cargo-xwin; subsequent runs are fast.
#
# This shell uses system rustup (installed via NixOS systemPackages)
# instead of vending a fixed Rust toolchain — the project tracks stable
# Rust and pinning it here would diverge from CI without value.

{ pkgs ? import <nixpkgs> {} }:

pkgs.mkShell {
  buildInputs = with pkgs; [
    # Linux build runtime libs — same set pkgs/nicotine.nix declares so
    # `cargo build` (Linux target) inside this shell links cleanly.
    pkg-config
    xorg.libX11
    xorg.libXcursor
    xorg.libXrandr
    xorg.libXi
    xorg.libxcb
    libxkbcommon
    libGL
    wayland

    # Windows cross-compile via xwin. clang provides clang-cl (the MSVC-
    # compatible front-end cargo-xwin invokes); llvm provides llvm-rc
    # (the resource compiler build.rs calls to embed the .exe icon).
    cargo-xwin
    clang
    llvm
  ];

  # Static linker hints for the Linux build path. egui-winit and the
  # X11/Wayland deps load these at runtime; setting LD_LIBRARY_PATH so
  # plain `cargo run` works without patchelf during dev.
  LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath (with pkgs; [
    libGL
    wayland
    libxkbcommon
    xorg.libX11
    xorg.libXcursor
    xorg.libXrandr
    xorg.libXi
    xorg.libxcb
  ]);

  shellHook = ''
    # Use the rustup-managed Rust toolchain, not whatever nix happens
    # to put on PATH. The system has rustup installed via NixOS
    # systemPackages; that toolchain is where the
    # `x86_64-pc-windows-msvc` cross target (rust-std) gets added. A
    # nix-store rustc/cargo on PATH would be a *different* toolchain
    # that doesn't know about user-added targets — `cargo xwin` would
    # then fail with `can't find crate for core`.
    if command -v rustup >/dev/null 2>&1; then
      rustup target add x86_64-pc-windows-msvc >/dev/null 2>&1 || true
      RUSTUP_TOOLCHAIN_BIN="$(dirname "$(rustup which rustc 2>/dev/null)")"
      if [ -n "$RUSTUP_TOOLCHAIN_BIN" ] && [ -d "$RUSTUP_TOOLCHAIN_BIN" ]; then
        export PATH="$RUSTUP_TOOLCHAIN_BIN:$PATH"
      fi
    fi

    cat <<'EOF'
nicotine dev shell ready.

Linux:    cargo build
          cargo test
          cargo clippy --all-targets -- -D warnings

Windows:  cargo xwin clippy --release --target x86_64-pc-windows-msvc -- -D warnings
          cargo xwin build   --release --target x86_64-pc-windows-msvc
EOF
  '';
}
