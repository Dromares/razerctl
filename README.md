# razerctl

> razerctl is an independent, community project. It is **not affiliated with, authorized, sponsored or endorsed by Razer Inc.**
> "Razer", "Synapse", "Viper" and related names and marks are trademarks of Razer Inc. and are used here only to describe
> which hardware and protocol this tool is compatible with.

![razerctl GUI](docs/screenshot-buttons.png)

## Install

```
./install.sh                 # builds, installs CLI + GUI + app-menu entry to ~/.local, udev rule via sudo
./install.sh --uninstall
# or, as an Arch/CachyOS package:
cd packaging && makepkg -si
```

Then open **razerctl** from the app menu (or run `razerctl-gui`).

* **Buttons** tab: remaps saved *into the mouse* (onboard memory, profile 1). They keep working
  with no software running, on any computer. Left click is locked so you can't lose clicking.
* **Device** tab: battery, firmware, serial, DPI stages, set DPI / polling rate.
* **Software remap** tab: optional computer-side remapper (`remap.conf` + systemd user service)
  for things the mouse can't store.
* **Low-battery notification** (Device tab): optional desktop alert at or below a chosen
  percentage, run by the `razerctl-battery.service` user unit (settings in
  `~/.config/razerctl/battery.conf`).
* **Button tester** (bottom bar): shows what the computer actually receives when you press a button.

CLI equivalent: `razerctl --backend hidraw onboard` / `onboard set BUTTON ACTION`.
Protocol details: [docs/onboard-button-protocol.md](docs/onboard-button-protocol.md).

### AppImage

A release carries `razerctl-X.Y.Z-x86_64.AppImage`: the GUI as one file, built in an Ubuntu 22.04 root filesystem. It
**needs glibc 2.35 or newer** (the GUI binary's highest symbol is `GLIBC_2.35`): Ubuntu 22.04, Debian 12 and Fedora 36
or newer are fine; RHEL, Rocky and Alma 9 (glibc 2.34), Ubuntu 20.04 and Debian 11 refuse to start with
`GLIBC_2.35 not found`. It bundles no graphics libraries: Wayland, X11, xkbcommon and OpenGL come from the host. It
does **not** install the udev rule, so the mouse stays unreachable until you do:

```
chmod +x razerctl-0.2.0-x86_64.AppImage && ./razerctl-0.2.0-x86_64.AppImage
sudo cp udev/70-razerctl.rules /etc/udev/rules.d/ && sudo udevadm control --reload && sudo udevadm trigger
```

The command-line tool is inside it too (`--appimage-extract`, then `squashfs-root/usr/bin/razerctl`). The
systemd units are not packaged; use `./install.sh` if you want the remap service or the low-battery notifier.

To build one (the image needs at least the build machine's glibc, so the release is built
in an Ubuntu 22.04 root filesystem):

```
cargo install cargo-packager --locked
cargo build --release --locked && cargo packager --release --formats appimage
```

The packaging settings are in `Cargo.toml` under `[package.metadata.packager]`; the output is
`target/packager/razerctl-gui_<version>_x86_64.AppImage`, renamed to `razerctl-<version>-x86_64.AppImage` for the release.
`cargo-packager` downloads `linuxdeploy` and the AppImage runtime when it runs, and needs `patchelf`, `file`,
`desktop-file-utils` and `squashfs-tools` installed.

---

A Linux rewrite, in Rust, of the device-control core of **Razer Synapse 4 for macOS**
(`RazerSynapseInstaller.pkg`, RazerAppEngine 4.4.25).

## Verifying a change (local CI)

Hosted Actions are not available, so a non-author verifies a PR by running one command at the PR head SHA in a clean worktree:

```sh
scripts/local-ci.sh /path/to/verify.log
```

It prints the toolchain (`rustc -V`, `cargo -V`, pinned by `rust-toolchain.toml`), checks the GUI system libraries with `pkg-config`, then runs `cargo build --release --locked`, `cargo test --release --locked`, `cargo clippy --all-targets`, `cargo fmt --check`, and an `xvfb-run` smoke test that launches `razerctl-gui` for 5 s with an isolated `HOME` (no device writes). Clippy is compared with a baseline recorded in the script (`CLIPPY_BASELINE`): a new warning fails; existing debt is reported. `cargo fmt --check` is strict: any unformatted hunk fails. Exit status is 0 only on `RESULT: PASS`; attach the log to the PR.

## What was in the .pkg

| Component | What it is | Rewritten? |
|---|---|---|
| `Razer.app/Contents/Resources/app.asar` | Electron shell (main process + IPC bridge). The UI itself is a remote web app loaded from `app-assets.razer.com` | **Device I/O path yes**; UI no (it's Razer's hosted, proprietary web app) |
| `electron/Protocol/protocol25*.js`, `UsbRzDeviceAction.js` | Protocol25 framing, status codes, `hid.sendFeatureReport`/`getFeatureReport` IPC | **Yes** → `src/protocol.rs`, `src/hidraw.rs` |
| `node-rz-hid`, `rz-usb-detect` (IOKit .node addons) | HID transport and hot-plug | **Yes** → `/sys/class/hidraw` + `HIDIOC[SG]FEATURE` |
| `com.razer.appengine.driver.dext` (DriverKit, arm64) | `RazerGenericHIDDriverKit`: intercepts input reports so buttons can be remapped/hooked (`SetHook`, `HandleMouseButton`, `handleReport`) | **Yes** → `src/remap.rs` (evdev grab + uinput) |
| Per-device Synapse middleware (`apps.razer.com/synapse/products/158/mw/`, not in the .pkg) | Onboard-memory button assignment (`setSingleButtonAssignment`, obmEngine) | **Yes** → `src/onboard.rs`, GUI Buttons tab |
| Synapse web UI | — | **Replaced** by `razerctl-gui` (egui/eframe) |
| `libmapping_engine.dylib`, `libsimple_service.dylib` | Macro/key-mapping engine, background services | No |
| `RazerGameManagerService` (.NET + SteamKit2) | Game library scanner for auto-profile switching | No |
| Sentry telemetry, Razer ID login, updater, installer scripts | — | Intentionally dropped |

## Protocol (Protocol25)

There are 90-byte feature reports, sent with report ID 0 (91 bytes on the wire):

```
[0] status  [1] transaction id  [2..3] remaining packets (BE)  [4] protocol type
[5] data size  [6] command class  [7] command id  [8..87] args  [88] XOR(2..87)  [89] 0
status: 0 new, 1 busy, 2 success, 3 failed, 4 timeout, 5 not supported
```

## Backends

* `openrazer-sysfs` (automatic when the `razermouse`/`razerkbd` kernel driver is bound). It
  uses the driver's sysfs attributes, so it never races the kernel for the control channel.
  The user needs to be in the `openrazer` group.
* `hidraw`: raw Protocol25. This needs rw access to `/dev/hidraw*`:
  `sudo cp udev/70-razerctl.rules /etc/udev/rules.d/ && sudo udevadm control --reload && sudo udevadm trigger`

## Usage

```
razerctl list | info | battery | dpi [X [Y]] | poll [HZ] | raw CLASS ID SIZE [ARGS..]
razerctl --backend hidraw raw 00 81 02      # firmware version, raw
```

## Button remapping (replaces the DriverKit extension)

Synapse's `com.razer.appengine.driver.dext` intercepts button reports (`handleReport` →
`HandleMouseButton` → hooks set by `SetHook`) and lets the mapping engine substitute an action.
`razerctl remap` does the same in user space: it takes an exclusive EVIOCGRAB on the mouse's evdev
nodes, mirrors them onto a uinput device named "razerctl remapped mouse", and rewrites mapped buttons.

```
razerctl buttons                         # press buttons to see their names
razerctl remap --map back=ctrl+c         # quick test, Ctrl+C to stop
$EDITOR ~/.config/razerctl/remap.conf    # persistent rules
install -Dm755 target/release/razerctl ~/.local/bin/razerctl
cp systemd/razerctl-remap.service ~/.config/systemd/user/ && systemctl --user enable --now razerctl-remap
```
Requirements: read access to the Razer `/dev/input/event*` nodes (openrazer group) and rw on
`/dev/uinput` (your user already has both).

`razerctl remap` exits 78 when the config is the problem (no rules, a rule that does not parse, an undefined macro)
and the unit does not restart on that: it stays `failed` with the reason in `journalctl --user -u razerctl-remap`.
Any other failure (no mouse yet, a crash) is retried every 2 s. After you change the unit file, `systemctl --user
daemon-reload` (`./install.sh` does it). The GUI's "Save and restart service" / "Start service" check the unit's
`ExecStart` binary first when the rules play macros, then read the unit back and show why it is not running.

### Macros

Synapse plays this mouse's macros on the computer, not from the mouse (`docs/macro-protocol.md`),
and razerctl does the same: nothing is written to the device. Define macros in
`~/.config/razerctl/macros.conf` and bind them in `remap.conf` with `macro:NAME[,once|xN|held|toggle]`:

```
# macros.conf                            # remap.conf
[copy]                                   back = macro:copy
down ctrl                                forward = macro:copy,x3
tap c
up ctrl
```
Events: `down`/`up`/`tap KEY`, `click BUTTON`, `delay MS`, `wheel N`, `hwheel N`, `move DX DY`.
Like Synapse, playback adds a 3 ms pause after every 5th event and 20 ms at the end.

`razerctl-gui` edits the same two files on its Software remap tab: **Macros** is a list plus an event table
(saved to `macros.conf`; saving rewrites the file, so comments in it are not kept, and renaming a macro rewrites
its `macro:` rules in `remap.conf` in the same save), and **Rules** has an *Add a rule* strip that appends one
line to the rules text. A rule that names a macro that does not exist blocks *Save config*.

```
razerctl macros                          # list: events and run time
razerctl macros show copy                # one macro, canonical form
razerctl macros check                    # validate macros.conf and the macro: refs in remap.conf
razerctl macros play copy --dry-run      # the exact evdev frames and sleeps, nothing is sent
razerctl remap --macros FILE             # use another macros file (default above)
```

**Stop all macros** (the GUI's Software remap banner, and the tray menu) sends `SIGUSR1` to the remap service
(`systemctl --user kill --kill-whom=main --signal=SIGUSR1 razerctl-remap.service`): every running macro ends within
a poll tick (~20 ms), anything it held down is released, and remapping carries on. A `razerctl` older than this
change has no handler for the signal, so the service exits and systemd restarts it 2 s later (same effect, one
interruption): reinstall the binary (`./install.sh`) to get the in-place stop.

## System tray and closing the window

`razerctl-gui` puts a StatusNotifierItem in the system tray (the `ksni` crate, over D-Bus; no GTK). Closing the
window **hides to the tray** by default: the window really closes, and the process stays as the tray icon (it keeps
reading the battery and the remap unit every 30 s) until you choose *Show razerctl* (or click the icon), which
reopens the window, or *Quit razerctl*. Unsaved edits in the window are lost on close, as they always were. The
choice is in the **App** tab (*Minimise to tray* / *Quit*, stored in `~/.config/razerctl/gui.conf`); Ctrl+Q and the
App tab's *Quit razerctl* button quit whatever it says. The first time the window hides, one desktop notification
says so.

* Tray menu: Show, a battery line, the remap line with its opposite action (Start / Stop remap), *Stop all macros*,
  Quit. **Quit only quits the GUI**: the remap service and the low-battery notifier are separate systemd user units
  (`razerctl-remap.service`, `razerctl-battery.service`) that never depend on the GUI process and keep running.
* Icon: no mouse (dashed) > low battery (`!`) > charging (bolt) > remap running (dot) > normal. The symbolic SVGs in
  `packaging/icons/` are installed by `./install.sh` and the package so the shell can recolour them; without them
  a pixmap drawn by the app is used. The SVGs are generated: edit `examples/gen_icons/icons.rs`, run
  `cargo run --example gen_icons` and commit the result (`cargo test` fails if they drift).
* **Caveat: a StatusNotifier host is required to hide.** KDE Plasma, XFCE and most panels have one; GNOME needs the
  *AppIndicator and KStatusNotifierItem Support* extension. With no host, closing the window quits (the App tab says
  so and keeps your choice; the host is looked for again each time you close the window). A second `razerctl-gui`
  started while one is hidden in the tray is a second instance with its own icon.
