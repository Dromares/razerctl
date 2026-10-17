# Changelog

## v0.2.0

First tagged release. razerctl is an independent Linux rewrite, in Rust, of the device-control core of Razer Synapse;
it is not affiliated with Razer Inc. (see the README).

**What it is.** A command-line tool, `razerctl`, and a GUI, `razerctl-gui` (egui/eframe, Wayland and X11), for Razer
mice. It talks to the mouse either through the OpenRazer kernel driver's sysfs attributes (picked automatically when
the driver is bound) or by raw Protocol25 feature reports over `/dev/hidraw*`.

**Devices.** Written and checked against a Razer Viper Mini Signature Edition on its wireless dongle (USB ID 1532:009F). The code matches any
Razer (USB vendor 1532) HID device and has no per-model allow-list, but no other model has been tested.

**What it does**

- **Buttons:** remaps saved into the mouse's onboard memory (profile 1), so they keep working with no software
  running; the left button is locked so clicking cannot be lost. CLI: `razerctl onboard` / `onboard set BUTTON ACTION`.
- **Device:** battery level, firmware, serial, DPI stages and DPI, polling rate (125, 250, 500, 1000, 2000, 4000 and
  8000 Hz). A DPI stage table that comes back half-read is re-queried instead of shown. CLI: `info`, `battery`,
  `dpi`, `poll`, `raw`.
- **Software remap:** an optional computer-side remapper (`razerctl remap`, `remap.conf`, the
  `razerctl-remap` systemd user unit) for things the mouse cannot store, built on an exclusive evdev grab and a
  uinput device. It exits 78 on a config error so systemd does not restart it in a loop.
- **Macros:** host-side macros (`macros.conf`, bound with `macro:NAME[,once|xN|held|toggle]`), a macro editor in the
  GUI, `razerctl macros list|show|check|play --dry-run`, and a *Stop all macros* control. Nothing is written to the
  mouse for macros.
- **Battery:** optional low-battery desktop notification (`razerctl battery-watch`, the `razerctl-battery`
  systemd user unit).
- **System tray:** `razerctl-gui` has a StatusNotifierItem tray icon (battery, remap state, Show/Quit, Stop all
  macros). Closing the window hides to the tray when a tray host exists; the choice is on the App tab, and unsaved
  edits are carried across hide and Show.
- **Button tester:** shows what the computer receives when a button is pressed (`razerctl buttons`, bottom bar of
  the GUI).

**Requirements.** Reading and writing the mouse needs access to its `hidraw` and `/dev/input` nodes. Install the
udev rule from `udev/70-razerctl.rules` (`./install.sh` does this with sudo, or copy it to `/etc/udev/rules.d/`
and reload udev). Nothing else installs it, including the AppImage.

**Packaging.** `razerctl-0.2.0-x86_64.AppImage` is the GUI as a single file (build recipe in the README). It needs
glibc 2.35 or newer (Ubuntu 22.04, Debian 12, Fedora 36 or newer; not RHEL/Rocky/Alma 9, Ubuntu 20.04 or Debian 11),
the host's graphics stack (Wayland or X11, xkbcommon, OpenGL) and the udev rule above. It was started on Debian 12,
Fedora 42 and Ubuntu 24.04 under Wayland and X11, with software GL and no real mouse; hardware GL, the tray with a real
StatusNotifier host, and a real device in the AppImage were not tested. An Arch/CachyOS `PKGBUILD` and `./install.sh`
are in the repository.
