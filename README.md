<div align="center">
  <img src="assets/ghlogo.png" alt="Nicotine Logo" width="600">
</div>

# Nicotine 🚬

High-performance EVE Online multiboxing tool for Linux (X11 & Wayland), inspired by EVE-O Preview.

[Illuminated is recruiting!](https://illuminatedcorp.com)

## Features

- **Instant client cycling** with mouse buttons (forward/backward) or targeted switching (jump to client N)
- **Always-on-top overlay** showing all clients and their status
- **Daemon architecture** for near-zero-latency window switching
- **Auto-stack windows** to perfectly center multiple EVE clients
- **Draggable overlay** with middle-mouse button (X11 only)
- **Auto-detects display resolution** - works on any monitor setup
- **Multi-compositor support** - Works on X11, KDE Plasma (Wayland), Sway, and Hyprland
- **Minimize inactive clients** - Optional feature to reduce resource usage by minimizing unfocused clients

## Roadmap
- Comprehensive documentation
- More configuration options
- Optional preview windows
- Overlay redesign

## Quick Install

### One-Line Installer (Recommended)

```bash
curl -sSL https://raw.githubusercontent.com/isomerc/nicotine/master/install-github.sh | bash
```

Then restart your terminal and run:
```bash
nicotine start    # Automatically runs in background
```

### From Source

```bash
git clone https://github.com/isomerc/nicotine
cd nicotine
./install-local.sh
```

Then restart your terminal and run:
```bash
nicotine start    # Automatically runs in background
```

## Usage

### Basic Commands

```bash
nicotine start          # Start everything (daemon + overlay)
nicotine stop           # Stop all Nicotine processes
nicotine stack          # Stack all EVE windows
nicotine forward        # Cycle to next client
nicotine backward       # Cycle to previous client
nicotine 1              # Jump to client 1
nicotine 2              # Jump to client 2
```

### Targeted Cycling

By default, `nicotine 1`, `nicotine 2`, etc. use window detection order. To define your own order, create `~/.config/nicotine/characters.txt`:

```
Main Character
Alt One
Alt Two
```

Each line is a character name (without "EVE - " prefix). Line 1 = target 1, line 2 = target 2, etc. Bind these commands to hotkeys in your desktop environment for quick access.

### Mouse Bindings

**Native Support (Works on X11 & Wayland):**

Nicotine has built-in mouse button detection that works universally across all display servers and compositors

**Quick Setup:**
1. Add your user to the `input` group:
   ```bash
   sudo usermod -a -G input $USER
   ```
2. **Log out and log back in** (required for group membership to take effect)
3. Start Nicotine - mouse buttons work automatically!

**Configuration:**
Edit `~/.config/nicotine/config.toml` to customize:
```toml
enable_mouse_buttons = true
forward_button = 276   # Button 9 (forward/side button)
backward_button = 275  # Button 8 (backward button)
mouse_device_name = "" # Optional takes priority over mouse_device_path, use evtest to find name of the device
mouse_device_path = "/dev/input/event3" # Optional and not created on first run, find the correct device with evtest
```
If using mouse_device_name the string must be na exact match, you can list your devices with evtest.

```
$ sudo evtest
...
/dev/input/event16:     Logitech PRO X
...
```
In `~/.config/nicotine/config.toml` set

```
mouse_device_name = "Logitech PRO X"
```

Device is configuration priority order is:
1. Name
2. Path
3. Autodetect

**Common button codes:**
- `275` = BTN_EXTRA (button 8, backward)
- `276` = BTN_SIDE (button 9, forward)
- `277` = BTN_FORWARD
- `278` = BTN_BACK

**Find your button codes:**
```bash
sudo evtest  # Select your mouse, then click buttons to see their codes
```

**Troubleshooting:**
- Verify group membership: `groups | grep input`
- Check permissions: `ls -l /dev/input/event*`
- Disable if needed: `enable_mouse_buttons = false` in config

### Overlay Controls

- **Restack Windows** - Re-center all EVE clients
- **Daemon status** - Green = running, Red = stopped
- **Client list** - Shows all EVE clients with active indicator (>)
- **Middle-click drag** - Move the overlay (X11 only)

## Configuration

Config file: `~/.config/nicotine/config.toml`

Auto-generated on first run. Key settings:

```toml
display_width = 1920
display_height = 1080
panel_height = 0           # Set this if you have a taskbar/panel
eve_width = 1037           # ~54% of display width
eve_height = 1080
overlay_x = 10.0
overlay_y = 10.0
show_overlay = true        # Set to false to run daemon-only mode (no GUI)
enable_mouse_buttons = true
forward_button = 276       # Button 9
backward_button = 275      # Button 8
minimize_inactive = false  # Minimize clients when cycling away (saves resources)
```

## Architecture

- **Daemon mode**: Maintains window manager connection and state in memory for instant cycling
- **Unix socket IPC**: ~2ms command latency (vs ~50-100ms process spawning)
- **Non-blocking activation**: Fire-and-forget window switching
- **Native mouse support**: Direct evdev access for universal mouse button detection

## Requirements

### Display Server Support

Nicotine supports both **X11** and **Wayland** (compositor-dependent):

- **X11** - Full support (all features)
- **Wayland - KDE Plasma** - Full support via wmctrl (XWayland)
- **Wayland - Sway** - Full support via swaymsg
- **Wayland - Hyprland** - Full support via hyprctl
- **Wayland - GNOME** - Not supported (restrictive APIs)

### Dependencies

**Required:**
- **wmctrl** - Window management on X11 and KDE Plasma Wayland

**Wayland-specific (compositor tools):**
- **KDE Plasma:** wmctrl (uses XWayland compatibility)
- **Sway:** swaymsg (included with sway)
- **Hyprland:** hyprctl (included with hyprland)

**Install:**
```bash
# Arch
sudo pacman -S wmctrl

# Ubuntu/Debian
sudo apt install wmctrl

# Fedora
sudo dnf install wmctrl
```

For **mouse button support**, add yourself to the `input` group (see Mouse Bindings section).

## Wayland Support & Known Limitations

**What works:**
- Mouse buttons (native evdev support, no external tools needed)
- Window detection and cycling (all supported compositors)
- Window stacking (KDE/Sway/Hyprland)
- Auto-detection of display server and compositor

**Limitations:**
- Overlay dragging disabled on Wayland (security model prevents arbitrary window positioning)
  - Workaround: Use compositor window management (e.g., Super+drag)
- GNOME not supported (restrictive window management APIs)

## Building from Source

```bash
# Install Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Build
cargo build --release

# Binary at: target/release/nicotine
```

## License

See [LICENSE](LICENSE.md)
