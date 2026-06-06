<div align="center">
  <img src="assets/ghlogo.png" alt="Nicotine Logo" width="600">
</div>

# Nicotine 🚬

High-performance EVE Online multiboxing tool for Linux (X11 & Wayland) and Windows.

<div align="center">

### 💬 [**Join the Nicotine Discord**](https://discord.gg/N82KJcS47f) 💬

</div>

[Illuminated is recruiting!](https://illuminatedcorp.com)

## Features

- **Instant client cycling** with mouse buttons (forward/backward) or targeted switching (jump to client N)
- **Live preview windows** per EVE client — XComposite + XRender on Linux, DWM thumbnails on Windows
- **List display mode** — compact roster window as an alternative to per-client previews
- **Config panel** for character order, per-character jump hotkeys, preview sizing, and display mode
- **Daemon architecture** for near-zero-latency window switching
- **Auto-stack windows** to perfectly center multiple EVE clients
- **Drag-to-position with snap-to-dock** on previews and the list window; lockable layout for play
- **Multi-compositor support** — X11, KDE Plasma (Wayland), Sway, Hyprland, GNOME (Wayland; auto-stack excepted)
- **Minimize inactive clients** — optional, reduces resource usage when cycling away

## Roadmap
- Comprehensive documentation
- More configuration options

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

### Keyboard Bindings

**Quick Setup:**
1. Add your user to the `input` group:
   ```bash
   sudo usermod -a -G input $USER
   ```
2. **Log out and log back in** (required for group membership to take effect)

**Configuration:**
Edit `~/.config/nicotine/config.toml` to customize:
```toml
enable_keyboard_buttons = true
forward_key = 15  # TAB Key
backward_key = 15  # TAB Key - modifier_key applied if set in config
keyboard_device_path = None # Device path /dev/input/eventX (OPTIONAL but you may need to set this if keybinds don't work)
modifier_key = None # You will have to add this if you want a modifier key for backward cycling
```

**Common button codes:**
- `15` = KEY_TAB (TAB Key)
- `42` = LEFT_SHIFT

**Find your button codes:**
```bash
sudo evtest  # Select your keyboard, then click buttons to see their codes
```

**Troubleshooting:**
- Make sure you've enabled keyboard buttons: `enable_keyboard_buttons = true` in config
- Check for other device events, sometimes keyboards will have multiple events but only one is handling inputs
```bash
cat /proc/bus/input/devices | grep -B 5 "kbd" | grep -E "Name|Handlers"
sudo evtest /dev/input/eventX # Replace X with the correct event number i.e event11
```

### Config Panel & Previews

`nicotine start` opens the config panel and spawns one preview window per running EVE client. The panel has four sections: **Display Mode** (Previews vs List), **Cycle Order** (character list + per-character jump hotkeys), **Keyboard Hotkeys**, and **Preview Windows** (size sliders, show/hide toggle). Changes apply live; saves debounce to disk.

Previews and the list window:
- **Left-click-drag** to reposition; edges snap to adjacent windows
- **Click without drag** on a preview to foreground that EVE client
- **Lock positions** in the panel to freeze the layout during play

## Configuration

Config file: `~/.config/nicotine/config.toml`

Auto-generated on first run. Key settings:

```toml
display_width = 1920
display_height = 1080
panel_height = 0           # Set this if you have a taskbar/panel
eve_width = 1037           # ~54% of display width
eve_height = 1080
show_previews = true       # Spawn preview windows (false = daemon-only)
preview_width = 320        # Preview window width in px
preview_height = 180       # Preview window height in px
display_mode = "Previews"  # "Previews" or "List"
positions_locked = false   # Disable drag on previews + list
enable_mouse_buttons = true
forward_button = 276       # Button 9
backward_button = 275      # Button 8
minimize_inactive = false  # Minimize clients when cycling away (saves resources)
```

Per-character jump hotkeys live under `[character_hotkeys."Name"]` with `vk` and optional `modifier`; the config panel writes them for you.

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
- **Wayland - GNOME** - Cycling + preview windows work (via XWayland/EWMH); auto-stack is disabled because Mutter blocks applications from positioning windows. GNOME-on-Xorg supports everything.

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
- Window stacking (KDE/Sway/Hyprland; not GNOME Wayland)
- Preview windows (XComposite + XRender via XWayland)
- Auto-detection of display server and compositor

**Limitations:**
- GNOME (Wayland): auto-stacking is unavailable. Mutter does not let applications position windows, and there is no extension-free way around it. Client cycling and preview windows work normally via XWayland, and the Restack button is hidden on this session. GNOME-on-Xorg supports everything (including stacking), but note the GNOME Xorg session is being removed upstream (GNOME 49+/Fedora 43).

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
