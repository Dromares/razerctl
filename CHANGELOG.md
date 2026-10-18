# Changelog

## v0.3.0

razerctl now tells you when a newer release exists, can update an AppImage for you after you say yes, and every release
comes with checksums and a udev rule you can download. Nothing about how the mouse is driven changed.

**If you are on v0.2.0, install v0.3.0 by hand once.** v0.2.0 has no update check and cannot find or fetch this
release. Install v0.3.0 the way you installed v0.2.0 (see the README, *Install*); from then on updates work as described
below.

**Update check.**

- `razerctl-gui` asks GitHub whether a newer release exists when the window opens and then at most once a day. It never
  updates by itself: a newer release shows a banner, *See what's new* shows the release notes, and nothing is downloaded
  until you press *Update* and confirm.
- Offline, rate limited, or no release to show: the only trace is the *Last check* line on the **App** tab, which also has
  the on/off switch and a *Check now* button (a manual check skips the once-a-day limit). To turn the check off without
  the GUI, put `check_updates = false` in `~/.config/razerctl/gui.conf`. The time of the last check is kept in
  `~/.config/razerctl/update.state`.
- `razerctl update --check` makes one check and prints the answer. `razerctl update` shows what is new and asks
  `[y/N]` before it downloads anything. The command line never checks on its own; it only runs when you type it.
- What an update does depends on how razerctl was installed:
  - **AppImage:** check on by default. *Update* downloads the new `.AppImage` and `SHA256SUMS`, checks the download
    against the sum, replaces the file you started in place, keeps the old one as `<name>.old`, and asks you to restart.
    A checksum mismatch refuses and changes nothing. If razerctl cannot write to the folder, the new file is saved in
    your Downloads folder (or your home folder) and the message says where.
  - **Package under `/usr/` (for example the `razerctl-bin` recipe in `packaging/aur/razerctl-bin/`, which is not on the AUR yet):** check **off** by default, and notify-only if you turn it on.
    razerctl never replaces files under `/usr`; update with your package manager.
  - **Tarball, `./install.sh`, or a binary you copied yourself:** check on by default, notify-only, with a link to the
    release page. Repeat the install steps with the new release.
- Checking a download against `SHA256SUMS` catches a corrupted or truncated download. It does **not** protect against
  someone taking over the GitHub account or the release: the checksums come from the same place as the files, and the
  releases are not signed.

**AppImage updates with other tools.** The AppImage now embeds update information
(`gh-releases-zsync|Dromares|razerctl|latest|razerctl-*-x86_64.AppImage.zsync`) and every release carries the matching
`.zsync` file, so [AppImageUpdate](https://github.com/AppImageCommunity/AppImageUpdate), Gear Lever and AppImageLauncher
can update it with a delta download.

**`--version` and `--help`.** `razerctl --version` (or `-V`) now prints `razerctl X.Y.Z` and exits; before, it went on to
look for a mouse and exited with "no Razer HID devices found". `razerctl-gui` takes `--version`, `-V`, `--help` and `-h`,
and they work with no display. Because the AppImage starts the GUI, `./razerctl-X.Y.Z-x86_64.AppImage --version` and
`--help` work too.

**Release downloads.** A release has five files, with fixed names:

| File | What it is |
|---|---|
| `razerctl-X.Y.Z-x86_64.AppImage` | the GUI as one file (the command-line tool is inside; `--appimage-extract`) |
| `razerctl-X.Y.Z-x86_64.AppImage.zsync` | for AppImageUpdate, Gear Lever and AppImageLauncher |
| `razerctl-X.Y.Z-x86_64-linux.tar.gz` | `razerctl`, `razerctl-gui`, the udev rule, desktop entry, icons and the two systemd user units |
| `70-razerctl.rules` | the udev rule on its own |
| `SHA256SUMS` | check a download with `sha256sum -c SHA256SUMS --ignore-missing` |

Every format needs the udev rule once, because only root can install it, and the mouse stays unreachable without it:

- **AppImage:** it cannot install the rule itself. Run the `sudo curl -fLo /etc/udev/rules.d/70-razerctl.rules …` line
  from the README (it fetches `70-razerctl.rules` from the latest release); it also reloads udev.
- **Tarball:** `sudo install -Dm644 70-razerctl.rules /etc/udev/rules.d/70-razerctl.rules`, then
  `sudo udevadm control --reload && sudo udevadm trigger`.
- **`./install.sh`:** installs it with sudo.
- **Arch package** (`packaging/aur/razerctl-bin/`, not on the AUR yet): installs it to `/usr/lib/udev/rules.d/`.

If the GUI still reports no access after installing the rule, unplug and replug the mouse. The rule itself is unchanged
since v0.2.0.

**For people who build the AppImage.** The four tools the AppImage build uses (`AppRun`, `linuxdeploy`, its AppImage
plugin and the AppImage runtime) are checked against pinned sha256 values by `examples/fetch_appimage_tools`, a Rust
program, and the build stops if one differs. `RELEASING.md` describes how a release is cut.

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
