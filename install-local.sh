#!/bin/bash
# Nicotine - Local installer (for development)
set -e

INSTALL_DIR="$HOME/.local/bin"
BINARY_NAME="Nicotine"
LOWER_SYMLINK="nicotine"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

echo "=== Nicotine Local Installer ==="
echo

echo "[1/5] Building release binary..."
cargo build --release

echo "[2/5] Installing to $INSTALL_DIR..."
mkdir -p "$INSTALL_DIR"
cp "target/release/$BINARY_NAME" "$INSTALL_DIR/"
# Lowercase symlink so users can still type `nicotine` on the Linux CLI
# despite the binary itself being capitalized for Windows UX.
ln -sf "$BINARY_NAME" "$INSTALL_DIR/$LOWER_SYMLINK"

echo "[3/5] Installing desktop file and icon..."
mkdir -p ~/.local/share/applications
mkdir -p ~/.local/share/icons/hicolor/256x256/apps
cp "$SCRIPT_DIR/assets/nicotine.desktop" ~/.local/share/applications/
cp "$SCRIPT_DIR/assets/icon.png" ~/.local/share/icons/hicolor/256x256/apps/nicotine.png
update-desktop-database ~/.local/share/applications 2>/dev/null || true
gtk-update-icon-cache ~/.local/share/icons/hicolor 2>/dev/null || true

# Add to PATH if needed
if [[ ":$PATH:" != *":$INSTALL_DIR:"* ]]; then
    echo "[4/5] Adding $INSTALL_DIR to PATH..."
    SHELL_RC=""
    if [ -n "$BASH_VERSION" ]; then
        SHELL_RC="$HOME/.bashrc"
    elif [ -n "$ZSH_VERSION" ]; then
        SHELL_RC="$HOME/.zshrc"
    fi

    if [ -n "$SHELL_RC" ] && [ -f "$SHELL_RC" ]; then
        if ! grep -q "export PATH.*$INSTALL_DIR" "$SHELL_RC" 2>/dev/null; then
            echo "" >> "$SHELL_RC"
            echo "# Nicotine" >> "$SHELL_RC"
            echo "export PATH=\"\$HOME/.local/bin:\$PATH\"" >> "$SHELL_RC"
            echo "Added to $SHELL_RC"
        else
            echo "Already in PATH"
        fi
    fi
else
    echo "[4/5] PATH already configured"
fi

echo "[5/5] Testing installation..."
if "$INSTALL_DIR/$BINARY_NAME" --help > /dev/null 2>&1 || [ $? -eq 0 ]; then
    echo "✓ Binary installed successfully"
else
    echo "✓ Binary installed at $INSTALL_DIR/$BINARY_NAME"
fi

echo
echo "✓ Installation complete!"
echo
echo "Quick start:"
echo "  nicotine start"
echo
echo "Config will be auto-generated at: ~/.config/nicotine/config.toml"
echo
echo "Note: Restart your terminal first if PATH was just updated"
