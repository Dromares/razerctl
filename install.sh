#!/usr/bin/env bash
# Build and install razerctl (CLI + GUI) for the current user.
#   ./install.sh            install to ~/.local
#   ./install.sh --uninstall
# The udev rule (needed for onboard button remapping) is installed system-wide
# and needs sudo once; everything else stays in your home directory.
set -euo pipefail
cd "$(dirname "$0")"
BIN="$HOME/.local/bin"
APPS="$HOME/.local/share/applications"
ICONS="$HOME/.local/share/icons/hicolor/scalable/apps"
UNITS="$HOME/.config/systemd/user"
RULE=/etc/udev/rules.d/99-razerctl.rules

if [[ "${1:-}" == "--uninstall" ]]; then
    systemctl --user disable --now razerctl-remap.service razerctl-battery.service 2>/dev/null || true
    rm -f "$BIN/razerctl" "$BIN/razerctl-gui" "$APPS/razerctl.desktop" "$ICONS/razerctl.svg" \
          "$UNITS/razerctl-remap.service" "$UNITS/razerctl-battery.service"
    systemctl --user daemon-reload || true
    echo "Removed. (Left $RULE and ~/.config/razerctl in place; delete them by hand if you want.)"
    exit 0
fi

cargo build --release --locked
install -Dm755 target/release/razerctl     "$BIN/razerctl"
install -Dm755 target/release/razerctl-gui "$BIN/razerctl-gui"
install -Dm644 packaging/razerctl.desktop  "$APPS/razerctl.desktop"
install -Dm644 packaging/razerctl.svg      "$ICONS/razerctl.svg"
install -Dm644 systemd/razerctl-remap.service "$UNITS/razerctl-remap.service"
install -Dm644 systemd/razerctl-battery.service "$UNITS/razerctl-battery.service"
systemctl --user daemon-reload || true
# pick up a new binary if the battery notifier is already running
systemctl --user try-restart razerctl-battery.service 2>/dev/null || true
update-desktop-database "$APPS" 2>/dev/null || true

if ! cmp -s udev/99-razerctl.rules "$RULE" 2>/dev/null; then
    echo "Installing udev rule (needs sudo once)…"
    sudo install -Dm644 udev/99-razerctl.rules "$RULE"
    sudo udevadm control --reload && sudo udevadm trigger
fi
echo "Installed. Launch 'razerctl' from your app menu, or run razerctl-gui."
