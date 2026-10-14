# razerctl

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

---

A Linux rewrite, in Rust, of the device-control core of **Razer Synapse 4 for macOS**
(`RazerSynapseInstaller.pkg`, RazerAppEngine 4.4.25).

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
