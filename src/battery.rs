//! Low-battery desktop notifications.
//!
//! A small background watcher (`razerctl battery-watch`, run by the
//! `razerctl-battery.service` systemd user unit) polls the battery and sends a
//! freedesktop notification via `notify-send` when it drops to the threshold.
//! Settings live in ~/.config/razerctl/battery.conf and are re-read every poll,
//! so changes from the GUI apply without restarting the service.

use std::fs;
use std::io;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use crate::device::{self, Prefer};

pub const SERVICE: &str = "razerctl-battery.service";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub enabled: bool,
    /// Notify at or below this percentage.
    pub threshold: u8,
    /// Minutes between battery checks.
    pub interval_min: u32,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            enabled: false,
            threshold: 20,
            interval_min: 5,
        }
    }
}

pub fn config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        });
    base.join("razerctl/battery.conf")
}

/// `key = value` lines; unknown keys and bad values fall back to defaults.
pub fn parse_config(text: &str) -> Config {
    let mut c = Config::default();
    for line in text.lines() {
        let line = line.split('#').next().unwrap().trim();
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let v = v.trim();
        match k.trim() {
            "enabled" => c.enabled = matches!(v, "true" | "yes" | "1" | "on"),
            "threshold" => {
                if let Ok(n) = v.trim_end_matches('%').parse::<u8>() {
                    c.threshold = n.clamp(1, 99)
                }
            }
            "interval_minutes" => {
                if let Ok(n) = v.parse::<u32>() {
                    c.interval_min = n.clamp(1, 120)
                }
            }
            _ => {}
        }
    }
    c
}

pub fn format_config(c: &Config) -> String {
    format!(
        "# razerctl low-battery notification settings\nenabled = {}\nthreshold = {}\ninterval_minutes = {}\n",
        c.enabled, c.threshold, c.interval_min
    )
}

pub fn load() -> Config {
    fs::read_to_string(config_path())
        .map(|t| parse_config(&t))
        .unwrap_or_default()
}

pub fn save(c: &Config) -> io::Result<()> {
    let p = config_path();
    if let Some(d) = p.parent() {
        fs::create_dir_all(d)?;
    }
    fs::write(p, format_config(c))
}

/// Alert state, kept between polls so you get one notification per drop
/// rather than one every few minutes.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Alerter {
    /// Lowest percentage we've already notified about since the last re-arm.
    notified_at: Option<u8>,
}

/// After the first alert, remind again each time it falls this many more points.
pub const REMIND_STEP: u8 = 5;
/// Re-arm once the battery is this far above the threshold (or charging).
pub const HYSTERESIS: u8 = 5;

impl Alerter {
    /// Feed one reading; returns Some(pct) when a notification should be sent.
    pub fn step(&mut self, pct: u8, charging: bool, threshold: u8) -> Option<u8> {
        if charging || pct >= threshold.saturating_add(HYSTERESIS) {
            self.notified_at = None;
            return None;
        }
        if pct > threshold {
            return None;
        }
        match self.notified_at {
            Some(last) if pct + REMIND_STEP > last => None,
            _ => {
                self.notified_at = Some(pct);
                Some(pct)
            }
        }
    }
}

pub fn notify(summary: &str, body: &str, critical: bool) -> io::Result<()> {
    let st = Command::new("notify-send")
        .args(["--app-name=razerctl", "--icon=razerctl"])
        .arg(format!(
            "--urgency={}",
            if critical { "critical" } else { "normal" }
        ))
        .args([summary, body])
        .status()?;
    if st.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("notify-send exited with {st}")))
    }
}

pub fn notify_low(name: &str, pct: u8) -> io::Result<()> {
    let critical = pct <= 10;
    notify(
        &format!("{name}: battery low ({pct}%)"),
        "Plug in the mouse's charging cable soon.",
        critical,
    )
}

/// Read (percentage, charging) from the first Razer device.
pub fn read_battery() -> io::Result<(String, u8, bool)> {
    let devs = device::open_all(Prefer::Auto, 0x1f)?;
    let (_, b) = devs
        .into_iter()
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no Razer device"))?;
    let b = b?;
    let pct = device::raw_to_pct(b.battery_raw()?);
    let charging = b.charging().unwrap_or(false);
    Ok((b.name(), pct, charging))
}

/// Service main loop. Exits only on error-free shutdown signals (systemd stops it).
pub fn watch_forever() -> ! {
    let mut alert = Alerter::default();
    loop {
        let cfg = load();
        if cfg.enabled {
            match read_battery() {
                Ok((name, pct, charging)) => {
                    if let Some(p) = alert.step(pct, charging, cfg.threshold) {
                        eprintln!("battery {p}% <= {}%: notifying", cfg.threshold);
                        if let Err(e) = notify_low(&name, p) {
                            eprintln!("notify failed: {e}");
                        }
                    }
                }
                // asleep / unplugged / receiver gone: just try again next time
                Err(e) => eprintln!("battery read skipped: {e}"),
            }
        }
        std::thread::sleep(Duration::from_secs(cfg.interval_min as u64 * 60));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_round_trip_and_defaults() {
        let c = Config {
            enabled: true,
            threshold: 15,
            interval_min: 10,
        };
        assert_eq!(parse_config(&format_config(&c)), c);
        assert_eq!(parse_config("garbage\nthreshold = abc"), Config::default());
        assert_eq!(parse_config("threshold = 0").threshold, 1);
        assert_eq!(parse_config("threshold = 25%").threshold, 25);
    }

    #[test]
    fn notifies_once_then_reminds_every_5_points() {
        let mut a = Alerter::default();
        let seq: Vec<Option<u8>> = [30, 21, 20, 19, 18, 16, 15, 14, 11, 10, 9]
            .iter()
            .map(|&p| a.step(p, false, 20))
            .collect();
        assert_eq!(
            seq,
            [
                None,
                None,
                Some(20),
                None,
                None,
                None,
                Some(15),
                None,
                None,
                Some(10),
                None
            ]
        );
    }

    #[test]
    fn charging_or_recovery_rearms() {
        let mut a = Alerter::default();
        assert_eq!(a.step(18, false, 20), Some(18));
        assert_eq!(a.step(18, false, 20), None);
        assert_eq!(a.step(18, true, 20), None); // plugged in
        assert_eq!(a.step(17, false, 20), Some(17)); // unplugged while still low -> warn again
        assert_eq!(a.step(22, false, 20), None); // small bounce up: stays armed-off
        assert_eq!(a.step(19, false, 20), None);
        assert_eq!(a.step(60, false, 20), None); // charged
        assert_eq!(a.step(20, false, 20), Some(20));
    }
}
