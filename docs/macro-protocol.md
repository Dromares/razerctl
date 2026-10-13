# Viper Mini Signature Edition: button-macro protocol

Companion to `docs/onboard-button-protocol.md`. Same device (Viper Mini SE,
USB `1532:009F` receiver, fw 1.4), same sources, one question: how does Synapse
create, store and send button macros?

Investigation only. **Nothing here has been sent to the mouse**, and no live
capture was made. Anything that needs a capture is tagged **unverified** and the
procedure to settle it is at the end.

## Sources and how they were read
- **Web**: `https://apps.razer.com/synapse/products/158/mw/` (index +
  `main.d7eb85f9d4db42cb11f5.js` + all 89 webpack chunks listed in its chunk map,
  fetched 2026-10-13). Evidence is cited as `chunk:function-or-string`.
  - `6259.1944c2392b18ba3f8282.js`: rzDevice25 protocol layer **and** a copy of
    the obmEngine (`this.addMacro`, `this.macroItemToFWData`, ...) **and** the
    host-engine bridge (`callMappingEngineAction`, ~137k lines pretty-printed).
  - `7303.8b96a462054239b4b494.js`: obmEngine proper (the same engine; it holds
    only `init`/`loadProfilesFromDevice`, the macro code is in 6259).
  - `rzDevice30.e14f762cc06dc394861d.js`: Protocol 30 layer, used only as a
    cross-check (it defines a *different* class-0x04 macro command set).
  - `main.d7eb85f9d4db42cb11f5.js`: the product config for this device.
- **macOS installer**, `/home/beeizebubs/Downloads/RazerSynapseInstaller.pkg`,
  payloads extracted read-only into `/home/beeizebubs/work/worktrees/DAN-1416-pkg/`:
  `EngineInstall.pkg` (Razer.app 4.4.25, build 4.4.25.194; Razer Synapse.app 0.8.11),
  `SynapseInstall.pkg`, `ChromaInstall.pkg`. `app.asar` extracted with
  `@electron/asar`.

## 1. Are macros stored onboard, or played back on the host?

**Both mechanisms exist in Synapse. For this mouse macros are played back on the
host. Confirmed from code; not confirmed on the device.**

### Evidence that this device is configured for host macros only
1. Product config (`main.js`, the 158 entry):
   `isOBMDevice:!0, OBMSlots:1, OBMSpecs:{... supportedFeature:{macros:!1,
   brightness:!0, dpi:!0, pollingRate:!1}, supportedMappings:{disableGroup,
   hyperShiftGroup, keyboardGroup, mouseGroup, multimediaGroup,
   sensitivityGroup:[DPI_Clutch,DPI_Up,DPI_Down,DPI_CycleUp,DPI_CycleDown]}}`.
   There is **no `macroGroup`** key in `supportedMappings`, and `macros` is `false`.
2. The flag reaches the engine. `main.js` (offset ~173746) ends its boot with
   `useFeature("isObm","yes", Object.assign({}, getFeatureParam("isObm","yes"),
   DeviceInfo.OBMSpecs.supportedFeature))`. The framework default for `isObm.yes` is
   `{macros:true, brightness:true, dpi:true}` (`main.js` feature builder, `if (e.obm)
   t.isObm = {yes:{macros:!0,...}}`), so the per-device `macros:false` **overrides**
   the default. `6259:` (pretty ~78560 to 78600) then reads those "injected
   features" (`o = isObm.yes`) and calls `obmEngine.init(BUTTON_LIST, mapping, e)`;
   `7303:init` (line ~100) sets `this.options.macros = i.macros` (= `false` here).
3. Every class-0x06 macro call in the engine is behind `this.options.macros`:
   `6259: loadMacrosFromDevice` (`if (this.options.macros) { getMacroStorageInfo();
   getNumberOfMacros(); getMacroIdsList(); ... } else {skip}`), `7303`
   lines ~1066 and ~1197 (`if (this.options.macros) { getMacroStorageInfo ...}`), and `6259: formatDevice` (`FormatStorage()` only
   `&& this.options.macros`). So the read/format paths never send a class-0x06
   command to this mouse. Caveat: `addMacro` and `deleteMacro` (callers of
   `createMacroId`, `allocateMacroDataSize`, `setMacroMemoryData`, `deleteMacro`) are
   **not** themselves guarded by `options.macros`; they run from the OBM sync
   (`macrosToBeCreated` / `macrosToBeRemoved`, `6259` ~84380 and ~85177), which diffs
   the macros referenced by OBM profile button assignments. I did not trace that set
   to empty for productId 158. It should be empty because item 4 strips `macroGroup`
   before the OBM write, but that part is **unverified** (it is the one place a
   class-0x06 upload could still originate, so capture B below checks for it).
4. A macro assigned to a button is filtered out of the OBM write: the button
   mapping goes through `6259: ve(e)` (pretty ~128410), which returns `false`
   when `OBMSpecs.supportedMappings[outputType]` is `undefined`
   (`if (void 0 === c) return !1`), and the caller then rewrites that entry to
   `disableGroup` (`ve(n) || (delete n[n.outputType], n.outputType =
   "disableGroup")`, ~128310). `macroGroup` is therefore never written to the
   device for this product.
5. The macro itself is converted for the host engine instead (see section 3):
   `6259: createMacro` (pretty ~16578) produces `{name, guid, appEngine:{events,
   hash, mouseMoveType}, macroEvents}`, and the macro list is pushed to the
   native engine with `localStorageSetItem("synapseMacros", JSON.stringify(...))`
   (`6259` ~89504 and ~90388).

### Does the *firmware* support onboard macros at all? **Unverified.**
The rzDevice25 layer in 6259 defines a complete class-0x06 command set (section 2),
so *protocol 25 as a family* supports device-side macros (other protocol-25
products set `macros: true`; not enumerated here). Whether the Viper Mini SE
firmware 1.4 answers class 0x06 is unknown, because Synapse never asks. The product
config says "not supported", which is the manufacturer's statement, not a probe
result. A read-only probe is given in the capture section: `06:8E` (Get Macro
Storage Info) and `06:80` (Get Number of Macros). Both are Get commands. They were
**not** sent for this investigation.

So the existing "No macros onboard" line in the onboard doc is **confirmed as
Synapse's configuration** (items 1 to 4 above), and **not independently confirmed as
a firmware fact**.

## 2. Device-side macro protocol (rzDevice25, class 0x06). Exists, but gated off for this mouse

Same 90-byte framing as the onboard doc. Command table from `6259:` (pretty
~42500 to 42560), `[dataSize, class, id]`, all 16-bit values **big-endian**
(`E4(x)` = `x>>8&255`, `l7(x)` = `x&255`, `Jz(hi,lo)` = BE join; helpers are in
the shared utility module):

| Command | class,id | size | args | response |
|---|---|---|---|---|
| Get Number of Macros | 0x06,0x80 | 2 | – | `[num_hi,num_lo]` |
| Create New Macro | 0x06,0x02 | 2 | `[id_hi,id_lo]` | status only |
| Delete Macro | 0x06,0x03 | 2 | `[id_hi,id_lo]` | status only |
| Get Macro Data | 0x06,0x85 | 0x50 | `[id_hi,id_lo]` | `[id(2), size(4: hi16,lo16), events...]` |
| Get Macro Data Size | 0x06,0x88 | 6 | `[id_hi,id_lo]` | `[id(2), size_hi16(2), size_lo16(2)]` |
| Allocate Macro Data Size | 0x06,0x08 | 6 | `[id(2), size_hi16(2), size_lo16(2)]` | status |
| Get Macro Memory Data | 0x06,0x89 | 0x50 | `[id(2), offset_hi16(2), offset_lo16(2), len]` | `[id(2), offset(4), len, data[len]]` |
| **Set Macro Memory Data** | **0x06,0x09** | 0x50 | `[id(2), offset(4), len, data[len]]` | status |
| Get Macro Memory Management | 0x06,0x8A | 6 | `[id_hi,id_lo, 0, mode]` | `[id(2), status, actionId]`, status 0 initial / 1 ongoing / 2 done / 3 error |
| Set Macro Memory Management | 0x06,0x0A | 6 | `[id_hi,id_lo, 0, mode]` (mode 2 = `MemoryFormat`, used with id 0; 0 none, 1 raw-data check, 3 check-by-id) | status |
| Get Macro ID List (with offset) | 0x06,0x8B | 0x50 | `[off_hi,off_lo]` | `[off(2), count(2), ids(2 each)...]` |
| Get Macro Name (with offset) | 0x06,0x8C | 0x50 | `[id(2), off(2)]` | `[id(2), off(2), size(2), bytes...]` |
| Set Macro Name (with offset) | 0x06,0x0C | 0x50 | `[id(2), off(2), size(2), bytes...]`; `size` is sent as the **total** name length (250) on every chunk, and the report's data-size byte is `chunk+6` | status |
| **Get Macro Storage Info** | **0x06,0x8E** | 14 | – | `[maxId(2), maxMem(4), availMem(4), recyclableMem(4)]` |

(In the bundle each constant is a JS comma expression, e.g.
`oo = (new Uint8Array([12,6,134]), ..., new Uint8Array([6,6,136]))`, which
evaluates to the **last** element only. The table above was resolved that way, so
`Get Macro Data Size` is `[6,6,0x88]`, not the first literal. Other literals in
those expressions (`06:81`, `06:84`, `06:04`, `06:05`, `06:06`, `06:07`, `06:86`,
`06:87`, `06:8D`, `06:0D`, `06:0E`) are dead definitions the engine never calls.)

Caution when reusing the arg layouts: the `Set Macro Memory Management` and
`Set Macro Name` encoders in the engine are thin; the column above is what the
call sites in `6259: addMacro/setMacroNameStructure/setMacroDataToFW/FormatStorage`
actually send, not a full spec of unused fields.

### Macro record layout (device-side, as built by `addMacro`)
`6259: addMacro` (pretty ~33560) does, in order:
1. `createMacroId()`: random id from `r.tx()` not already in `macroIdList`
   (generator range **unverified**; ids are 16-bit on the wire).
2. `convertMacroItemsToByteArray(numItems, items)`: encode events (below).
3. Check space: `required = ceil((dataSize+256)/256)*256`; fail with
   `ERROR_NOT_ENOUGH_STORAGE_SPACE` if `required > available+recyclable`; if
   `required > available` run `defragStorage()` first. `calculatePerMacroDataSizeWithName
   = ceil((size+256)/256)*256` (256 = the name structure, rounded up to blocks).
4. `06:08` Allocate Macro Data Size (`id`, total size).
5. `setMacroNameStructure`: the 250-byte name block, written in 64-byte chunks via
   `06:0C` (`off = 0,64,128,192`, final 58; data-size byte `chunk+6`):
   `guid(16) | numItems(4, little-endian u32) | name(100, UTF-8, NUL padded) |
   reserved(16 zeros) | timestamp(16) | hash(32) | pad(66)`. Readback
   (`getMacroNameStructure`) parses `0..16` guid, `16..20` numItems LE, `20..120`
   name, `120..136` extra, `136..152` timestamp, `152..184` hash.
6. `setMacroDataToFW`: the event bytes in **72-byte chunks** via `06:09`
   (`id, offset, len, data`; report data size `len+7`). (Note: the code slices
   `t.slice(i, i+r)` with `r` = total length rather than the chunk length; the
   encoder truncates to `len`, so it works. Unusual but harmless.)

### Event encoding (`6259: macroItemToFWData` ~32910, decoder ~32630 to 32800, enum in `89627`)
Each event is `[type, value...]`:

| Type | byte | Layout | Meaning |
|---|---|---|---|
| KEY_MAKE / KEY_BREAK | 0x01 / 0x02 | `[type, hidUsage]` | key down / up. Source scancodes (PS/2 set 1, e.g. 30 `A`, also the +128 break codes) are mapped to **HID keyboard usages** by a large switch (e.g. 30->0x04, 48->0x05); unknown -> `STATUS_FAIL` |
| POWER_MAKE / BREAK | 0x03 / 0x04 | – | defined in enum; encoder not traced (**unverified**) |
| MEDIA_MAKE / BREAK | 0x05 / 0x06 | – | defined in enum; encoder not traced (**unverified**) |
| BUTTON | 0x08 | `[8, heldMask]` | mouse buttons. `heldMask` is the **state of all buttons after the event**: bit0 left, bit1 right, bit2 middle, bit3 back(X1), bit4 forward(X2). A down sets the bit, an up clears it, so a press is two events with different masks |
| SCROLL_WHEEL | 0x0A | `[0x0A, 0x01 \| 0xFF]` | one notch up / down (+120 / -120 in the host model) |
| DELAY_1..4 | 0x11..0x14 | `[t, ms]` (1 byte), `[t, ms_hi, ms_lo]`, `[t, b2,b1,b0]` (3 bytes BE), `[t, b3..b0]` (4 bytes BE) | smallest size that fits `ms` (<256, <65536, <2^24, else) |
| CURSOR | 0x15 | `[0x15, x_hi,x_lo, y_hi,y_lo]` | relative move (`mouseMoveExt`) |

Decoder `macroItemToFWData`'s inverse also splits a combined `BUTTON` mask into
separate host down/up events.

### Binding to a button, and how it relates to `02:0C`
Where a firmware does support macros, the binding **is** a `02:0C` Set Single
Button Assignment (layout in the onboard doc) with a macro function id. Enum
(`6259:` ~35190, identical to the one in `docs/onboard-button-protocol.md`, which
simply omitted these rows):

| fnId | name | fnSize | data | playback |
|---|---|---|---|---|
| 3 | MacroTypeI | 3 | `[id_hi,id_lo,N]` | `N=1` run once; `N>1` repeat N times |
| 4 | MacroTypeII | 2 | `[id_hi,id_lo]` | continuous **while held** |
| 5 | MacroTypeIII | 2 | `[id_hi,id_lo]` | continuous **toggle** |
| 15 | MacroTypeIV | 2 | `[id_hi,id_lo]` | **sequence** |

Source: `6259: K` (macroGroup -> fnId/data, ~26780), `6259: E/I/S` (reverse,
~26356 to 26400), `6259: updateFnDataByteJson` (~33860: `dwMacroId = Jz(n0,n1)`,
`byNTimes = n2`, size 3 for Type I, 2 for II/III/IV). The macro id in the button
data is the **device macro id** (the 16-bit id of the `06:xx` record), found by
`guid`.

For the Viper Mini SE: **do not send these.** Synapse never does (section 1), the
effect on this firmware is unknown, and a malformed button assignment can leave a
button dead (the onboard doc restored by writing the old value back, which depends
on the mouse still answering).

### Read back, delete, overwrite, limits (device-side)
- **Read back**: `06:80` count; `06:8B` ids, 32 per request (data size `2n+4`);
  `06:8C` name block, 64 bytes per request; `06:88` data size; `06:89` data, 72 bytes
  per request. (`loadMacrosFromDevice`, `getMacroNameStructure`,
  `getMacroDataStructure`.)
- **Delete**: `06:03` by id. Freed space is "recyclable" and only usable after a
  **defrag**, which Synapse implements as *format the whole storage and rewrite
  everything* (`defragStorage` -> `formatDevice` -> `06:0A` mode 2, poll `06:8A`
  until `status == 2` (`06:0A` args `[0,0,0,2]`; up to 3 polls, after a 100 ms wait), then re-add every profile and
  macro). So deleting one macro and re-adding costs a full re-upload.
- **Overwrite**: no in-place edit; Synapse deletes and re-adds (new random id).
- **Limits**: reported by the device in `06:8E` (`maxId`, `maxMemory`,
  `availableMemory`, `recyclableMemory`); Synapse reserves 256 bytes per extra
  profile (`availableMemory -= 256*(numOfProfiles-1)` when not firmware-reported).
  No fixed count is hard-coded in the web code. Per-device numbers are
  **unverified** for this mouse.
- **Wireless/BLE**: `macros` is forced off when `useBle` (`6259` ~78590, `y.A.runtimeData.useBle && o && (e.macros=false ...)`).

Cross-check, Protocol 30 (`rzDevice30`): defines `Get Macro Id list` /
`Create New Macro` / `Get Macro Data` / `Set Macro Data` / `Delete Macro` /
`Get/Set Macro Name` on **class 0x04**, a different layout. Not relevant to
this protocol-25 mouse; noted so nobody mixes the two.

## 3. Host-side macros (what actually applies to this mouse)

### Data model
The Synapse UI macro (`createMacro`, `6259` ~16578) is:
```
{ name, guid, deviceList: [],
  macroEvents: [ {Type:"actionBar"...}, ...UI events ],     // editor model, UI only
  appEngine:   { events: [...], hash, mouseMoveType } }     // what the engine runs
```
`appEngine.events[]` is an ordered list; event types seen (`6259` ~116150 to 117130,
`~121264 convertMacroDataToAppEngine`, native-engine strings below):

| type | fields |
|---|---|
| `keyboard` | `scancode` (PS/2 set 1 make code), `flag`: outputs in the sample templates use 2 then 3 for a key press+release (e.g. `{scancode:46,flag:2},{scancode:46,flag:3}`); mapping *inputs* use 0/1. Exact flag semantics are **unverified** (engine-internal) |
| `mouse` | `id` bitmask (Windows RawInput-style: 1 L-down, 2 L-up, 4 R-down, 8 R-up, 16 M-down, 32 M-up, 64/128 X1, 256/512 X2, 1024 wheel, 2048 h-wheel), `data` (wheel delta 120 / 65416 = -120) |
| `delay` | `ms` (the macro editor stores seconds as strings and converts `Math.round(1e3*s)`); `kind:"virtual"` for padding |
| `randomDelay` | `minMs`, `maxMs` |
| `mouseMove` / `mouseMoveExt` | relative/absolute move, `mouseMoveType` per macro |
| `multi` | `outputs:[...]` (atomic group) |
| `macro` | `guid`, `flag`, `repeat`, `toggle` (nested macro; engine limits recursion: strings "max macro recursion") |
| `loop` | `flag` 1 = start / 0 = end, `repeat` |
| `launch`, `clipboard` | `path` / `text` (not relevant to a mouse) |

Before storing, `~89515` pads the event list: after every 5 events it inserts a
`{type:"delay", ms:3, kind:"virtual"}` and after the last one a 20 ms delay
(`{each:5, delay:3, endDelay:20}`) so playback never floods the OS.
`hash` = `getHash(appEngine)`, used to detect changes.

### Where it is stored
- **Web Synapse (browser)**: macro list in IndexedDB db `macros-idb`, store
  `macros` (`6259` ~10933 to 11030, `MacroStorage`), plus `localStorage`
  `getMacros/setMacros`, broadcast on `BroadcastChannel("macros-change-channel")`.
  The profile/macro index is under `RzLockableStorage` key `MACRO_CONFIG`
  (entries `{guid, name, type:"file", date}`). **Cloud sync of macros: unverified**
  (not looked at).
- **Native host engine**: the middleware hands the macros to the native mapping
  engine as opaque JSON strings by key, through `localStorageSetItem(key, json)`:
  `"synapseMacros"` (all macros, `appEngine` events only, UI-only fields stripped:
  `delete t.actionLoad, t.deviceList, t.saved, t.macroEvents` in `~89504`),
  `"synapseTurbos"` (turbo configs) and, in the macOS dylib strings,
  `"synapsePhasedMacros"`. A per-device mapping blob is stored as
  `synapse_<productId>_<CONTAINER_ID>` (`6259` ~129016).

### How a button is bound
The button's mapping is converted to a **host mapping entry**, `{input, output}`,
and `macroGroup` becomes (`6259`, `ye` at ~117950):

```
macroGroup {guid, macroPlaybackOption, repeatCount}
  downOutput: { type: "macro" ("phasedMacro" for the phased variant),
                guid, flag: P, repeat: N, toggle: P }
  upOutput:   ContinuousHeld -> { type:"macro", guid, flag: 1 }   // stop on release
              phased          -> { type:"phasedMacro", guid, flag: 1 }
              otherwise       -> { type:"disabled" }
  P = { ContinuousToggle: 2, Sequence: 3, Queue: 4, PhasedMacro: 5 }[option] || 0
  N = Once -> 1, NTimes -> repeatCount
```
Playback options in the UI model (`6259` ~25985): `Once`, `NTimes`,
`ContinuousToggle`, `ContinuousHeld`, `Sequence` (and `Queue`/`PhasedMacro`
appear in the host mapping; **`Queue` has no UI constant in the file read**). Note
a likely latent bug in that code: the phased check is `"Phased" === option` while
the lookup table key is `PhasedMacro`, so `PhasedMacro` never takes the
`phasedMacro` branch. Not relevant to razerctl, but worth knowing if you read the
bundle.

**Nothing is written to the mouse for a host macro.** Item 4 of section 1 shows the
OBM path turns the entry into `disableGroup` rather than sending it (`02:0C` is not
used for it). The button keeps its factory report and the **host engine diverts the
press before the OS sees it**. That is the same mechanism `src/remap.rs` already
documents and implements (EVIOCGRAB + uinput):
- macOS: two DriverKit extensions ship in the pkg
  (`com.razer.appengine.driver.dext`, `com.razer.appengine.virtual.driver.dext`
  under `Razer Synapse.app/Contents/Library/SystemExtensions/`). The first
  contains `SetHook` / `SELECTOR_RAZER_DEV_SETALL_HOOK` / `..._REMOVE_ONE_HOOK` /
  `..._REMOVE_ALL_HOOK` (`strings`), i.e. user space installs a list of buttons
  to divert. The second wraps `IOHIDDevice::setReport` (injection).
- `libmapping_engine.dylib` additionally calls `CGEventTapCreate`, `CGEventTapEnable`
  and `CGEventPost` (`strings`).
- Windows: the same actions are exposed under `addUsbDevice` /
  `addUsbDeviceWithoutFilterDriver` (`6259` ~22960); that the "filter driver" is
  the diversion hook is inferred from the name (**unverified**).

The web layer drives the engine with these actions (`6259` ~22850 to 24000,
`callMappingEngineAction`): `mappingEngineInitialize`, `addUsbDevice`,
`addDevice`, `enableMapping`, `registerHardwareEvent`, `enableInputRedirect`,
`enableMouseMoveRedirect`, `startMacroRecording` / `stopMacroRecording` /
`getMacroRecordingMode` / `registerMacroRecorderEvent`,
`queryRunningMacrosAndKeysPressed`, `stopRunningTurbosAndMacros`,
`localStorageSetItem/DeleteItem/DeleteAllItems`.

### Modes (host side)
| Mode | Host mapping |
|---|---|
| Run once | `flag 0, repeat 1`, `upOutput disabled` |
| Run N times | `flag 0, repeat N` |
| While held (repeat until release) | `flag 0` down, `upOutput {macro, flag 1}` stops it |
| Toggle | `flag 2, toggle 2`; second press stops (engine state, not traced) |
| Sequence | `flag 3` |

The exact engine semantics of `flag`/`toggle` values 2 to 5 are in the native
binary and were not decompiled (**unverified**); the mapping above is only the
encoding the middleware sends.

### Limits, read back, delete
There is no device storage. Count and size are bounded by the host storage (not
quantified; JSON in IndexedDB / the engine's key-value store). Delete = remove the
guid from the stores above and raise `ON_MACRO_DELETED` (`6259` ~73669, handler
only skimmed). What the engine does with mappings that still point at a deleted
macro is **unverified**.

## 4. Web vs macOS pkg
| Item | Web (apps.razer.com mw, 2026-10-13) | macOS pkg (Razer.app 4.4.25) | Verdict |
|---|---|---|---|
| Device protocol layer (rzDevice25, OBM engine, macro class-0x06 code) | present (`6259`, `7303`) | **not shipped**. `app.asar` contains no `MacroType`, `Get Macro` or `createNewMacro` strings; `electron/main.js` references `apps.razer.com` and loads the middleware at runtime | **Match by construction**: the pkg is a shell that runs the same web middleware. Any difference would be in the served version, which the pkg does not pin |
| Product config (`macros:false` for productId 158) | present | not in pkg | web only |
| Host macro engine | `localStorageSetItem("synapseMacros"/"synapseTurbos")`, `callMappingEngineAction` | `app.asar:electron/modules/mapping_engine/mac/index.js` declares the same FFI surface (macro recorder, `queryRunningMacrosAndKeysPressed`, `stopRunningTurbosAndMacros`, `localStorage*`); `libmapping_engine.dylib` (arm64 Mach-O, 12 MB) contains `synapseMacros`, `synapsePhasedMacros`, `synapseTurbos`, `macro_data.cc`, `phased_macro_data.cc`, `HandleMacroOnDeviceThread`, `ExecuteMacroLoopOnThread`, `HandleSequenceMacroOnDeviceThread`, `MacroEvents`, `MappingCache::GetMacroEvents` | **Match** on keys and model; native playback logic not decompiled |
| Button diversion | Windows filter driver (`addUsbDeviceWithoutFilterDriver`) | DriverKit dext + `CGEventTap` | platform specific |
| Phased macros | `phasedMacro` output type | `phasedMacro`, `PhasedMacroData`, `MappingEnginePhasedMacroDataThread` | match |

Not diffed: the dylib was inspected with `strings`, not disassembled. If the board
wants playback semantics (toggle/queue/sequence), that is a separate REA pass on
`libmapping_engine.dylib` (`HandleMacroOnDeviceThread` and friends).

## 5. Verification still needed (board-run, nothing was captured)
**A. Firmware probe (read-only Gets).** Using razerctl's existing hidraw
transport against `/dev/hidrawN` for `1532:009F`, send **only** these Gets, with the
wireless mouse awake, in this order, and record `status` and data:
`06:8E` (data size 14), `06:80` (data size 2), `06:8B` with args `[00 00]`
(data size 0x50). Expected if unsupported: status `Not supported` / `Failed`. Expected if
supported: `06:8E` returns `maxId/maxMemory/availableMemory/recyclableMemory`.
Stop on any status that is not an obvious not-supported. Do not send any `06:0x`
command (those with the high bit clear are Sets).

**B. Confirm the host-only claim (no device traffic).** On a machine with
Windows or macOS Synapse (or Chrome with WebHID on Linux), start a capture, then in
the dashboard assign a macro to the forward button and save:
- Linux capture: `sudo modprobe usbmon`; find the receiver with `lsusb -d 1532:009f`
  (bus `B`, device `D`); `wireshark -k -i usbmonB` (or
  `tshark -i usbmonB -w macro.pcapng`).
- Display filter: `usb.device_address == D && usb.setup.bRequest == 0x09`
  (SET_REPORT control transfers; 90-byte reports, report ID 0). Add
  `&& usb.data_len == 90` to drop noise. With `tshark -r macro.pcapng -Y
  'usb.setup.bRequest==9 && usb.data_len==90' -T fields -e frame.time_relative -e
  usb.data_fragment` read byte 6 (command class) and byte 7 (command id) of each
  report: `0x06` would be a device macro upload; `0x02 0x0C` would be a button
  assignment.
- **Prediction (this document):** no class `0x06` report and **no** `02:0C` for the
  assigned button, only the same polling the dashboard always does. If a `02:0C` with
  fnId 3/4/5/15 appears, section 1 is wrong for this firmware/version.
- Repeat with a `Save` of a macro assigned to *hypershift*, a *toggle* and a
  *while held* macro to see whether any traffic distinguishes modes (prediction: none).
- Safety: this writes to the mouse in the sense that the dashboard may also send its
  own profile/lighting commands; do it on a device whose state you can restore
  (re-run the razerctl restore from the onboard doc if `02:0C` traffic is seen).

**C. Dashboard vs host.** Check whether the dashboard offers the *Macro* action for
this mouse at all (UI gating, **unverified**; the OBM `supportedMappings` has no
`macroGroup` but host-engine macros are offered for devices without OBM macros).

## 6. What razerctl needs

razerctl already has the right host-side primitive (`src/remap.rs`: EVIOCGRAB the
evdev nodes, forward through a uinput clone, rewrite mapped buttons). A macro is
just a richer `Action` on that path. **No new HID commands are required**, and
nothing in this document asks to write macros into the mouse.

1. `remap::Action::Macro { events: Vec<MacroEvent>, mode: Playback }` with
   `MacroEvent = Key(KeyCode, down) | Button(KeyCode, down) | Wheel(i32) | Delay(ms)
   | Move(dx,dy)`; play it on a worker thread through the existing uinput device.
   Do not play on the grab thread (a long macro would block forwarding).
2. `Playback = Once | Times(n) | Held (loop until release) | Toggle | Sequence`.
   Press event starts, release event stops for `Held`; second press stops `Toggle`.
   Stop all running macros on shutdown, device unplug, and when the mapping is
   reloaded (compare Synapse's `stopRunningTurbosAndMacros`).
3. Insert a 3 ms pad every 5 events and a 20 ms tail like Synapse (cheap, and
   avoids event floods). Cap recursion if nested macros are ever added.
4. Config: extend `remap.rs` text config (`parse_action`) with `macro:<name>[,mode]`
   and a separate macros file keyed by name. Keep Synapse's model so import is
   possible later: `guid`, `name`, `events` list as in section 3 (keyboard events
   as evdev `KEY_*` instead of PS/2 scancodes; the flag 2/3 vs 0/1 convention is
   Synapse-internal and should not leak).
5. Do **not** change the device (`02:0C`) for macros. The button stays factory
   default; razerctl diverts it. Only if the diversion path is ever unavailable would
   a fallback be to `02:0C`-remap the button to an unused key (e.g. F13 to F24 via
   fnId 2 `[0x00, usage]`, **unverified**, the ButtonCode write is the only verified
   write) and trigger the macro on that key.
6. Optional later, only after probe A passes and the board agrees: device-side macros
   (`06:xx`, section 2). Costs: a flash-wear budget, 72-byte chunked uploads, full
   defrag-on-delete, a PS/2-to-HID scancode table, and the real risk of a bricked
   button. Benefit is playback with no host running. Not recommended for a mouse
   Synapse itself treats as host-macros-only.
7. Hypershift: macros bound to the hypershift layer are separate host entries
   (`isHyperShift`). razerctl would need to track the hypershift modifier state; the
   mouse reports it as an HID report (report IDs 4/5, `razerKeyReportID`,
   `hardwareEventReportID`, from the product config), which razerctl does not read yet.
