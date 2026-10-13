# Viper Mini Signature Edition: onboard button-mapping protocol

Source: Razer's own Synapse 4 middleware for this mouse, downloaded from
`https://apps.razer.com/synapse/products/158/mw/` (webpack app
`project-Viper-Mini-Signature-Edition-mw`). Main code is in
`6259.*.js` (rzDevice25 protocol layer) and `7303.*.js` (obmEngine).
Nothing here has been sent to the mouse yet.

## Device facts (from the product config)
- productId 158 (0x9E, wired), dongleId 159 (0x9F, the one you have), vendor 0x1532
- `isOBMDevice: true`, `OBMSlots: 1`, protocol "25" (rzDevice25), claimInterface 0
- Supported onboard mappings: disable, mouse buttons, keyboard keys (+modifiers),
  multimedia, DPI functions (up/down/cycle/clutch), hypershift. **No macros onboard.**

## Report framing (rzDevice25, same as OpenRazer)
90-byte feature report:
`[0]=status 0, [1]=transaction id, [2..4]=0, [5]=data size, [6]=command class,
[7]=command id, [8..87]=args, [88]=XOR of bytes 2..87, [89]=0`.
Synapse cycles transaction id 0..31; device echoes it back.

## Commands
| Command | class,id | size | args |
|---|---|---|---|
| Get Button ID list | 0x02,0x84 | 0x50 | → `[count, ids...]` |
| **Get Single Button Assignment** | **0x02,0x8C** | 0x50 | `profile, buttonId, mode` |
| **Set Single Button Assignment** | **0x02,0x0C** | 0x50 | `profile, buttonId, mode, fnId, fnSize, data[5]` |
| Get Button Assignment List | 0x02,0x8E | 0x50 | `profile, mode, offset` |
| Get Number of Profiles | 0x05,0x80 | 1 | → `[count]` |
| Get Profile ID List | 0x05,0x81 | 0x50 | → `[count, ids...]` |
| Create New Profile | 0x05,0x02 | 1 | `profile` |
| Delete Profile | 0x05,0x03 | 1 | `profile` |
| Get / Set Active Profile ID | 0x05,0x84 / 0x05,0x04 | 1 | `profile` |
| Set Profile Reset | 0x05,0x0B | 1 | `profile` |

Response to Get Single Button Assignment: `data = [profile, buttonId, mode, fnId, fnSize, d0..d4]`.

`mode`: 0 = normal, 1 = hypershift.

## Button IDs (fwID) relevant to this mouse
left 1, right 2, middle 3, back (Button4) 4, forward (Button5) 5,
scroll up 9, scroll down 10, bottom DPI button (DKM_SB_03) 96.
The actual list the mouse exposes should be read with Get Button ID list.

## Function IDs (fnId) and data bytes
| fnId | meaning | size | data |
|---|---|---|---|
| 0 | Off (disabled) | 0 | – |
| 1 | ButtonCode (mouse button) | 1 | `[buttonCode]`: 1 left, 2 right, 3 middle, 4 back, 5 forward, 9 wheel up, 10 wheel down, 0x68 wheel left, 0x69 wheel right |
| 2 | KeyCode | 2 | `[modifierBits, HID usage]` |
| 6 | DPI | 1 (5 for clutch) | 1 up, 2 down, 5 clutch (+x hi,lo,y hi,lo), 6 cycle up, 7 cycle down |
| 10 | MediaKeys | 2 | HID consumer usage, big-endian (e.g. 0x00E9 vol up, 0x00EA vol down, 0x00E2 mute) |
| 11 | DoubleClick | 1 | `[1]` |
| 12 | Hypershift | 1 | `[1]` |
| 13 / 14 | Turbo key / turbo button | 4 / 3 | adds 16-bit interval in ms (1000 / presses-per-second) |

Modifier bits: LCtrl 0x01, LShift 0x02, LAlt 0x04, LGUI 0x08,
RCtrl 0x10, RShift 0x20, RAlt 0x40, RGUI 0x80.
A bare modifier key is sent as `[modBit, 0]`.

## Profiles
Synapse treats profile id 1 as the built-in default (reset keeps only id 1).

## Verified on the real mouse (Viper Mini SE 1532:009F, fw 1.4) — read-only
- Get Profile ID List 05:81 → `[01, 01]`: one profile, **id 1**.
- Get Button ID list 02:84 → `[08, 01,02,03,04,05,60,09,0a]`: left, right,
  middle, back, forward, dpi (0x60), wheel up, wheel down.
- Get Single Button Assignment 02:8C → response `[profile, button, mode, fnId, size, d0..d4]`;
  every button currently = factory default (fnId 1 / own code; dpi = fnId 6, data 6 = cycle up).
  Normal and hypershift layers identical.
- Unknown button (e.g. 07) → status Failed, as expected.
- Get Active Profile 05:84 is **not supported** (always Failed) on this firmware.
- Bulk list 02:8E works (8-byte records) but sometimes returns Failed with a
  truncated table, so razerctl reads buttons one at a time with 02:8C.
- **Write verified:** Set Single Button Assignment 02:0C `[01, 05, 00, 01, 01, 03, 0,0,0,0]`
  (profile 1, forward, normal, ButtonCode, size 1, middle) succeeded; evdev then
  reported BTN_MIDDLE for the forward button; the setting survived unplugging and
  replugging the receiver; restored with `[01, 05, 00, 01, 01, 05, ...]`.
- Status 0x04 (Timeout) on every command = mouse asleep (wireless); move it to wake.
- Responses occasionally collide with openrazer-daemon's own polling (wrong
  command echoed back, or Failed); reads are retried.
