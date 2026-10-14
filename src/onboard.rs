//! Onboard-memory button assignments (read side).
//!
//! Ported from Razer's per-device Synapse 4 middleware for the Viper Mini SE
//! (apps.razer.com/synapse/products/158/mw, rzDevice25 + obmEngine).
//! See docs/onboard-button-protocol.md.

use crate::hidraw::Hidraw;
use crate::protocol::Report;
use std::fmt;
use std::io;

pub const GET_PROFILE_IDS: (u8, u8) = (0x05, 0x81);
pub const GET_BUTTON_IDS: (u8, u8) = (0x02, 0x84);
pub const GET_BUTTON: (u8, u8) = (0x02, 0x8c);
pub const SET_BUTTON: (u8, u8) = (0x02, 0x0c);
#[allow(dead_code)]
pub const GET_BUTTON_LIST: (u8, u8) = (0x02, 0x8e);

pub const MODE_NORMAL: u8 = 0;
pub const MODE_HYPERSHIFT: u8 = 1;

/// Physical button (firmware id) -> friendly name.
pub fn button_name(id: u8) -> String {
    match id {
        1 => "left".into(),
        2 => "right".into(),
        3 => "middle".into(),
        4 => "back".into(),
        5 => "forward".into(),
        9 => "wheel-up".into(),
        10 => "wheel-down".into(),
        0x34 => "wheel-left".into(),
        0x35 => "wheel-right".into(),
        0x60 => "dpi".into(),
        n => format!("button-{n:#04x}"),
    }
}

/// Mouse-button codes used as the payload of fnId 1 (ButtonCode).
fn mouse_code_name(c: u8) -> String {
    match c {
        1 => "left-click".into(),
        2 => "right-click".into(),
        3 => "middle-click".into(),
        4 => "back".into(),
        5 => "forward".into(),
        9 => "wheel-up".into(),
        10 => "wheel-down".into(),
        0x68 => "wheel-left".into(),
        0x69 => "wheel-right".into(),
        n => format!("mouse-button {n:#04x}"),
    }
}

fn modifiers(bits: u8) -> Vec<&'static str> {
    const M: [(u8, &str); 8] = [
        (0x01, "ctrl"),
        (0x02, "shift"),
        (0x04, "alt"),
        (0x08, "super"),
        (0x10, "rctrl"),
        (0x20, "rshift"),
        (0x40, "ralt"),
        (0x80, "rsuper"),
    ];
    M.iter()
        .filter(|(b, _)| bits & b != 0)
        .map(|(_, n)| *n)
        .collect()
}

/// USB HID keyboard usage -> name (enough to make assignments readable).
fn hid_key_name(u: u8) -> String {
    match u {
        0x04..=0x1d => ((b'a' + (u - 0x04)) as char).to_string(),
        0x1e..=0x26 => ((b'1' + (u - 0x1e)) as char).to_string(),
        0x27 => "0".into(),
        0x28 => "enter".into(),
        0x29 => "esc".into(),
        0x2a => "backspace".into(),
        0x2b => "tab".into(),
        0x2c => "space".into(),
        0x3a..=0x45 => format!("f{}", u - 0x3a + 1),
        0x68..=0x73 => format!("f{}", u - 0x68 + 13),
        0x4f => "right".into(),
        0x50 => "left".into(),
        0x51 => "down".into(),
        0x52 => "up".into(),
        0x49 => "insert".into(),
        0x4a => "home".into(),
        0x4b => "pageup".into(),
        0x4c => "delete".into(),
        0x4d => "end".into(),
        0x4e => "pagedown".into(),
        n => format!("key {n:#04x}"),
    }
}

/// One stored assignment: what the firmware does when the button is pressed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assignment {
    pub button: u8,
    pub fn_id: u8,
    pub size: u8,
    pub data: [u8; 5],
}

impl Assignment {
    /// True if this is the factory behaviour (button sends itself).
    pub fn is_default(&self) -> bool {
        match self.button {
            0x60 => self.fn_id == 6 && self.data[0] == 6,
            b => self.fn_id == 1 && self.data[0] == b,
        }
    }
}

impl fmt::Display for Assignment {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let d = &self.data;
        match self.fn_id {
            0 => write!(f, "disabled"),
            1 => write!(f, "{}", mouse_code_name(d[0])),
            2 => {
                let mut parts = modifiers(d[0]);
                let key = if d[1] == 0 {
                    None
                } else {
                    Some(hid_key_name(d[1]))
                };
                let mut s: Vec<String> = parts.drain(..).map(String::from).collect();
                if let Some(k) = key {
                    s.push(k);
                }
                write!(f, "key {}", s.join("+"))
            }
            6 => match d[0] {
                1 => write!(f, "dpi-up"),
                2 => write!(f, "dpi-down"),
                5 => write!(
                    f,
                    "dpi-clutch {}x{}",
                    u16::from_be_bytes([d[1], d[2]]),
                    u16::from_be_bytes([d[3], d[4]])
                ),
                6 => write!(f, "dpi-cycle-up"),
                7 => write!(f, "dpi-cycle-down"),
                n => write!(f, "dpi function {n}"),
            },
            10 => write!(f, "media {:#06x}", u16::from_be_bytes([d[0], d[1]])),
            11 => write!(f, "double-click"),
            12 => write!(f, "hypershift"),
            13 => write!(
                f,
                "turbo key (every {} ms)",
                u16::from_be_bytes([d[2], d[3]])
            ),
            14 => write!(
                f,
                "turbo {} (every {} ms)",
                mouse_code_name(d[0]),
                u16::from_be_bytes([d[1], d[2]])
            ),
            n => write!(f, "fn {n} data {:02x?}", &d[..self.size.min(5) as usize]),
        }
    }
}

/// Decode a Get Button Assignment List (02:8E) response.
#[allow(dead_code)]
/// Layout: [profile, mode, offset, then 8-byte records: button, fnId, size, data[5]], 0-terminated.
pub fn decode_list(args: &[u8]) -> Vec<Assignment> {
    let mut out = Vec::new();
    let mut i = 3;
    while i + 8 <= args.len() && args[i] != 0 {
        let mut data = [0u8; 5];
        data.copy_from_slice(&args[i + 3..i + 8]);
        out.push(Assignment {
            button: args[i],
            fn_id: args[i + 1],
            size: args[i + 2],
            data,
        });
        i += 8;
    }
    out
}

/// Button name -> firmware id (only buttons this mouse reports).
pub fn parse_button(s: &str) -> Result<u8, String> {
    Ok(match s.to_ascii_lowercase().as_str() {
        "left" => 1,
        "right" => 2,
        "middle" => 3,
        "back" => 4,
        "forward" => 5,
        "dpi" => 0x60,
        "wheel-up" => 9,
        "wheel-down" => 10,
        other => {
            return Err(format!(
                "unknown button '{other}' (left right middle back forward dpi wheel-up wheel-down)"
            ))
        }
    })
}

fn hid_usage(k: &str) -> Option<u8> {
    let b = k.as_bytes();
    if b.len() == 1 && b[0].is_ascii_lowercase() {
        return Some(0x04 + (b[0] - b'a'));
    }
    if b.len() == 1 && (b'1'..=b'9').contains(&b[0]) {
        return Some(0x1e + (b[0] - b'1'));
    }
    if let Some(n) = k.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
        return match n {
            1..=12 => Some(0x3a + n - 1),
            13..=24 => Some(0x68 + n - 13),
            _ => None,
        };
    }
    Some(match k {
        "0" => 0x27,
        "enter" => 0x28,
        "esc" => 0x29,
        "backspace" => 0x2a,
        "tab" => 0x2b,
        "space" => 0x2c,
        "right" => 0x4f,
        "left" => 0x50,
        "down" => 0x51,
        "up" => 0x52,
        "insert" => 0x49,
        "home" => 0x4a,
        "pageup" => 0x4b,
        "delete" => 0x4c,
        "end" => 0x4d,
        "pagedown" => 0x4e,
        _ => return None,
    })
}

/// Parse an action string into an assignment for `button`.
/// Supported (all confirmed in Synapse's encoder for this mouse):
///   default | disable | left-click right-click middle-click back forward wheel-up wheel-down
///   dpi-up dpi-down dpi-cycle-up dpi-cycle-down | double-click | key:[mods+]KEY (e.g. key:ctrl+c, key:f13)
pub fn parse_action(button: u8, s: &str) -> Result<Assignment, String> {
    let s = s.to_ascii_lowercase();
    let mut data = [0u8; 5];
    let (fn_id, size) = match s.as_str() {
        "default" => return Ok(default_for(button)),
        "disable" | "disabled" | "off" => (0, 0),
        "left-click" => {
            data[0] = 1;
            (1, 1)
        }
        "right-click" => {
            data[0] = 2;
            (1, 1)
        }
        "middle-click" => {
            data[0] = 3;
            (1, 1)
        }
        "back" => {
            data[0] = 4;
            (1, 1)
        }
        "forward" => {
            data[0] = 5;
            (1, 1)
        }
        "wheel-up" => {
            data[0] = 9;
            (1, 1)
        }
        "wheel-down" => {
            data[0] = 10;
            (1, 1)
        }
        "dpi-up" => {
            data[0] = 1;
            (6, 1)
        }
        "dpi-down" => {
            data[0] = 2;
            (6, 1)
        }
        "dpi-cycle-up" => {
            data[0] = 6;
            (6, 1)
        }
        "dpi-cycle-down" => {
            data[0] = 7;
            (6, 1)
        }
        "double-click" => {
            data[0] = 1;
            (11, 1)
        }
        k if k.starts_with("key:") => {
            let mut mods = 0u8;
            let mut key = 0u8;
            for part in k[4..].split('+') {
                let bit = match part {
                    "ctrl" => 0x01,
                    "shift" => 0x02,
                    "alt" => 0x04,
                    "super" | "meta" | "win" => 0x08,
                    "rctrl" => 0x10,
                    "rshift" => 0x20,
                    "ralt" => 0x40,
                    "rsuper" => 0x80,
                    _ => 0,
                };
                if bit != 0 {
                    mods |= bit;
                    continue;
                }
                if key != 0 {
                    return Err(format!("only one non-modifier key allowed in '{k}'"));
                }
                key = hid_usage(part).ok_or_else(|| format!("unknown key '{part}'"))?;
            }
            if mods == 0 && key == 0 {
                return Err("empty key".into());
            }
            data[0] = mods;
            data[1] = key;
            (2, 2)
        }
        other => return Err(format!("unknown action '{other}'")),
    };
    Ok(Assignment {
        button,
        fn_id,
        size,
        data,
    })
}

impl Assignment {
    /// The `parse_action` string that recreates this assignment, if it is one
    /// razerctl knows how to write (used by the GUI to pre-fill editors).
    pub fn action_string(&self) -> Option<String> {
        let d = &self.data;
        Some(match self.fn_id {
            0 => "disable".into(),
            1 => match d[0] {
                1 => "left-click",
                2 => "right-click",
                3 => "middle-click",
                4 => "back",
                5 => "forward",
                9 => "wheel-up",
                10 => "wheel-down",
                _ => return None,
            }
            .into(),
            2 => {
                let mut parts: Vec<String> =
                    modifiers(d[0]).into_iter().map(String::from).collect();
                if d[1] != 0 {
                    let k = hid_key_name(d[1]);
                    if k.starts_with("key ") {
                        return None;
                    }
                    parts.push(k);
                }
                format!("key:{}", parts.join("+"))
            }
            6 => match d[0] {
                1 => "dpi-up",
                2 => "dpi-down",
                6 => "dpi-cycle-up",
                7 => "dpi-cycle-down",
                _ => return None,
            }
            .into(),
            11 => "double-click".into(),
            _ => return None,
        })
    }
}

pub fn default_for(button: u8) -> Assignment {
    if button == 0x60 {
        Assignment {
            button,
            fn_id: 6,
            size: 1,
            data: [6, 0, 0, 0, 0],
        }
    } else {
        Assignment {
            button,
            fn_id: 1,
            size: 1,
            data: [button, 0, 0, 0, 0],
        }
    }
}

/// Decode a Get Single Button Assignment (02:8C) response.
pub fn decode_single(args: &[u8]) -> Assignment {
    let mut data = [0u8; 5];
    data.copy_from_slice(&args[5..10]);
    Assignment {
        button: args[1],
        fn_id: args[3],
        size: args[4],
        data,
    }
}

pub struct Onboard<'a> {
    pub dev: &'a Hidraw,
    pub tid: u8,
}

impl Onboard<'_> {
    /// Read-only query. The firmware intermittently answers Failed (0x03) when
    /// another client (openrazer-daemon) hits the device at the same moment, so
    /// reads are retried a few times. Never use this for writes.
    fn q(&self, c: (u8, u8), size: u8, args: &[u8]) -> io::Result<Report> {
        let req = Report::new(c, size, self.tid).with_args(args);
        let mut last = None;
        for _ in 0..6 {
            match self.dev.transact(&req) {
                Ok(r) => return Ok(r),
                Err(e) => {
                    last = Some(e);
                    std::thread::sleep(std::time::Duration::from_millis(30));
                }
            }
        }
        Err(last.unwrap())
    }

    pub fn profile_ids(&self) -> io::Result<Vec<u8>> {
        let r = self.q(GET_PROFILE_IDS, 0x50, &[])?;
        let n = r.args[0] as usize;
        Ok(r.args[1..1 + n.min(r.args.len() - 1)].to_vec())
    }

    pub fn button_ids(&self) -> io::Result<Vec<u8>> {
        let r = self.q(GET_BUTTON_IDS, 0x50, &[])?;
        let n = r.args[0] as usize;
        Ok(r.args[1..1 + n.min(r.args.len() - 1)].to_vec())
    }

    /// Read one button (02:8C). Response: [profile, button, mode, fnId, size, data[5]].
    /// This is what Synapse's obmEngine uses; the bulk list (02:8E) intermittently
    /// returns Failed with a truncated table on fw 1.4.
    pub fn assignment(&self, profile: u8, button: u8, mode: u8) -> io::Result<Assignment> {
        let r = self.q(GET_BUTTON, 0x0a, &[profile, button, mode])?;
        Ok(decode_single(&r.args))
    }

    /// Write one button to onboard memory (02:0C), exactly as Synapse's
    /// rzDevice25.setSingleButtonAssignment: [profile, button, mode, fnId, size, data[5]],
    /// padded to 0x50. Sent ONCE (no blind retry); the caller verifies by reading back.
    pub fn set_assignment(&self, profile: u8, mode: u8, a: &Assignment) -> io::Result<Assignment> {
        let mut args = vec![profile, a.button, mode, a.fn_id, a.size];
        args.extend_from_slice(&a.data);
        let req = Report::new(SET_BUTTON, 0x50, self.tid).with_args(&args);
        self.dev.transact(&req)?;
        std::thread::sleep(std::time::Duration::from_millis(50));
        let got = self.assignment(profile, a.button, mode)?;
        let same = got.fn_id == a.fn_id && got.data[..a.size as usize] == a.data[..a.size as usize];
        if !same {
            return Err(io::Error::other(format!(
                "read-back mismatch: wrote {a:?}, mouse now has {got:?}"
            )));
        }
        Ok(got)
    }

    pub fn assignments(
        &self,
        profile: u8,
        buttons: &[u8],
        mode: u8,
    ) -> io::Result<Vec<Assignment>> {
        buttons
            .iter()
            .map(|&b| self.assignment(profile, b, mode))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Captured from the real Viper Mini SE (fw 1.4), profile 1, normal mode.
    const LIVE: [u8; 71] = [
        0x01, 0x00, 0x00, 0x01, 0x01, 0x01, 0x01, 0, 0, 0, 0, 0x02, 0x01, 0x01, 0x02, 0, 0, 0, 0,
        0x03, 0x01, 0x01, 0x03, 0, 0, 0, 0, 0x04, 0x01, 0x01, 0x04, 0, 0, 0, 0, 0x05, 0x01, 0x01,
        0x05, 0, 0, 0, 0, 0x60, 0x06, 0x01, 0x06, 0, 0, 0, 0, 0x09, 0x01, 0x01, 0x09, 0, 0, 0, 0,
        0x0a, 0x01, 0x01, 0x0a, 0, 0, 0, 0, 0, 0, 0, 0,
    ];

    #[test]
    fn decodes_live_default_profile() {
        let a = decode_list(&LIVE);
        assert_eq!(
            a.iter().map(|x| x.button).collect::<Vec<_>>(),
            [1, 2, 3, 4, 5, 0x60, 9, 10]
        );
        assert!(a.iter().all(Assignment::is_default));
        assert_eq!(a[3].to_string(), "back");
        assert_eq!(a[5].to_string(), "dpi-cycle-up");
    }

    #[test]
    fn parses_actions_like_synapse() {
        let k = parse_action(5, "key:ctrl+c").unwrap();
        assert_eq!((k.fn_id, k.size, k.data[0], k.data[1]), (2, 2, 0x01, 0x06));
        let m = parse_action(5, "middle-click").unwrap();
        assert_eq!((m.fn_id, m.size, m.data[0]), (1, 1, 3));
        assert_eq!(parse_action(5, "default").unwrap(), default_for(5));
        assert_eq!(parse_action(0x60, "default").unwrap().fn_id, 6);
        assert_eq!(parse_action(5, "key:f13").unwrap().data[1], 0x68);
        assert_eq!(parse_action(5, "disable").unwrap().fn_id, 0);
        assert!(parse_action(5, "key:ctrl+a+b").is_err());
        assert_eq!(parse_button("Forward").unwrap(), 5);
    }

    #[test]
    fn action_string_round_trips() {
        for s in [
            "disable",
            "middle-click",
            "back",
            "key:ctrl+c",
            "key:ctrl+shift+t",
            "key:f13",
            "key:shift",
            "dpi-cycle-down",
            "double-click",
            "wheel-up",
        ] {
            let a = parse_action(5, s).unwrap();
            assert_eq!(a.action_string().as_deref(), Some(s), "{s}");
        }
        assert_eq!(
            default_for(0x60).action_string().as_deref(),
            Some("dpi-cycle-up")
        );
    }

    #[test]
    fn decodes_live_single() {
        // real response to 02:8C [01, 04, 00]
        let a = decode_single(&[0x01, 0x04, 0x00, 0x01, 0x01, 0x04, 0, 0, 0, 0]);
        assert_eq!((a.button, a.fn_id, a.size, a.data[0]), (4, 1, 1, 4));
        assert!(a.is_default());
    }

    #[test]
    fn formats_keys_and_disable() {
        let k = Assignment {
            button: 4,
            fn_id: 2,
            size: 2,
            data: [0x01, 0x06, 0, 0, 0],
        };
        assert_eq!(k.to_string(), "key ctrl+c");
        assert!(!k.is_default());
        let off = Assignment {
            button: 5,
            fn_id: 0,
            size: 0,
            data: [0; 5],
        };
        assert_eq!(off.to_string(), "disabled");
    }
}
