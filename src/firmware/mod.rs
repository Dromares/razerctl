//! `razerctl firmware`: look inside the vendor's firmware updater and rehearse a flash on paper. Nothing in here opens a
//! device (DAN-1885, phase 1 of DAN-1878): `inspect` reads a `.exe` the user supplies, `flash --dry-run` drives the DFU
//! state machine against a simulated device and prints the frames. Razer's firmware is never stored in this repository;
//! it is extracted from the user's own copy of the updater at run time.

pub mod dfu;
pub mod flash;
pub mod ihex;
pub mod resources;
pub mod sim;
pub mod zip;

use std::collections::HashMap;

/// Name of the resource set inside the updater's ZIP that holds the config and the Intel-HEX text.
const RESOURCE_SET: &str = "DeviceUpdater.resources";
const LINE_COUNT_SUFFIX: &str = "FWLineNum";

/// One firmware image of the updater, rebuilt and checked.
#[derive(Debug)]
pub struct FirmwareImage {
    /// The updater's name for it: `Dev1`, `Flash1`, `Dev2`.
    pub name: String,
    pub vid: u16,
    /// Normal-mode product id the image is for.
    pub pid: u16,
    pub version: String,
    pub image: ihex::Image,
    pub sha256: String,
    /// Intel-HEX records read (all checksums verified).
    pub records: usize,
    /// The Intel-HEX record text, one entry per line, as the updater stores it.
    pub lines: Vec<String>,
    /// For a `Flash` image: the closing CRC-16 its stream carries (see `flash`).
    pub flash_crc: Option<u16>,
    /// Bootloader the image is written through, when the config names one (`BLVID`/`BLPID`/`BLVER`/`BCDPID_BL`).
    pub bootloader: Option<Bootloader>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bootloader {
    pub vid: u16,
    pub pid: u16,
    pub version: String,
    /// `bcdDevice` the bootloader must report: the normal-mode PID of the device the image belongs to.
    pub bcd: u16,
}

#[derive(Debug)]
pub struct Inspection {
    pub file_len: usize,
    pub zip_offset: usize,
    pub zip_members: usize,
    pub resource_bytes: usize,
    pub images: Vec<FirmwareImage>,
}

fn hex16(cfg: &HashMap<String, String>, key: &str) -> Result<u16, String> {
    let v = cfg
        .get(key)
        .ok_or_else(|| format!("unknown resource layout: config entry '{key}' is missing"))?;
    u16::from_str_radix(v.trim(), 16)
        .map_err(|_| format!("config entry '{key}' = '{v}' is not a 4-digit hex id"))
}

/// Everything `inspect` reports, from the bytes of an updater `.exe`.
pub fn inspect(exe: &[u8]) -> Result<Inspection, String> {
    let z = zip::Zip::find(exe)?;
    let entry = z
        .entry(RESOURCE_SET)
        .ok_or_else(|| format!("the updater's ZIP has no {RESOURCE_SET} member"))?;
    let raw = z.read(entry)?;
    let cfg = resources::parse(&raw).map_err(|e| format!("{RESOURCE_SET}: {e}"))?;

    let mut names: Vec<&str> = cfg
        .keys()
        .filter_map(|k| k.strip_suffix(LINE_COUNT_SUFFIX))
        .collect();
    names.sort_unstable();
    if names.is_empty() {
        return Err(format!(
            "unknown resource layout: {RESOURCE_SET} has no <image>{LINE_COUNT_SUFFIX} entry, so no firmware image"
        ));
    }
    let mut images = Vec::new();
    for name in names {
        let key = format!("{name}{LINE_COUNT_SUFFIX}");
        let want: usize = cfg[&key]
            .trim()
            .parse()
            .map_err(|_| format!("{key} = '{}' is not a line count", cfg[&key]))?;
        let kind = name.trim_end_matches(|c: char| c.is_ascii_digit());
        let n = &name[kind.len()..];
        if kind.is_empty() || n.is_empty() {
            return Err(format!(
                "unknown resource layout: image name '{name}' is not <Kind><number>"
            ));
        }
        let labels: Vec<String> = (0..want).map(|i| format!("{name}FWLine{i}")).collect();
        let mut lines = Vec::with_capacity(want);
        for l in &labels {
            let t = cfg
                .get(l)
                .ok_or_else(|| format!("{name}: {l} is missing ({key} says {want} lines)"))?;
            lines.push((l.as_str(), t.as_str()));
        }
        let image = ihex::assemble(lines).map_err(|e| format!("{name}: {e}"))?;
        let version_key = format!("{kind}FWVer{n}");
        let version = cfg
            .get(&version_key)
            .ok_or_else(|| {
                format!("unknown resource layout: config entry '{version_key}' is missing")
            })?
            .trim()
            .to_string();
        // Only the `Dev` images go through the bootloader; the `Flash` image is sent through the running device (class 0x0A).
        let bootloader = match cfg.get(&format!("BLPID{n}")) {
            Some(_) if kind == "Dev" => Some(Bootloader {
                vid: hex16(&cfg, &format!("BLVID{n}"))?,
                pid: hex16(&cfg, &format!("BLPID{n}"))?,
                version: cfg.get(&format!("BLVER{n}")).cloned().unwrap_or_default(),
                bcd: hex16(&cfg, &format!("BCDPID_BL{n}"))?,
            }),
            _ => None,
        };
        let lines: Vec<String> = labels.iter().map(|l| cfg[l].trim().to_string()).collect();
        let flash_crc = if kind == "Flash" {
            Some(
                flash::stream_crc(lines.iter().map(String::as_str))
                    .map_err(|e| format!("{name}: {e}"))?,
            )
        } else {
            None
        };
        images.push(FirmwareImage {
            name: name.to_string(),
            vid: hex16(&cfg, &format!("VID{n}"))?,
            pid: hex16(&cfg, &format!("PID{n}"))?,
            version,
            sha256: crate::sha256::hex_digest(&image.data[..]).map_err(|e| e.to_string())?,
            image,
            records: want,
            lines,
            flash_crc,
            bootloader,
        });
    }
    Ok(Inspection {
        file_len: exe.len(),
        zip_offset: z.offset,
        zip_members: z.entries.len(),
        resource_bytes: raw.len(),
        images,
    })
}

/// The lines `firmware inspect` prints.
pub fn describe(path: &str, file_sha256: &str, ins: &Inspection) -> Vec<String> {
    let mut out = vec![
        format!("{path}: {} bytes, sha256 {file_sha256}", ins.file_len),
        format!(
            "ZIP at offset {} ({} members); {RESOURCE_SET} is {} bytes",
            ins.zip_offset, ins.zip_members, ins.resource_bytes
        ),
    ];
    for f in &ins.images {
        out.push(format!(
            "image {:<7} target {:04x}:{:04x}  version {}  {:#010x}-{:#010x}  {} bytes  sha256 {}  ({} records, all checksums ok)",
            f.name,
            f.vid,
            f.pid,
            f.version,
            f.image.start,
            f.image.end(),
            f.image.data.len(),
            f.sha256,
            f.records
        ));
        match &f.bootloader {
            Some(b) => out.push(format!(
                "  written via bootloader {:04x}:{:04x} (bcdDevice {:04x}, protocol {})",
                b.vid, b.pid, b.bcd, b.version
            )),
            None => out.push(match f.flash_crc {
                Some(c) => format!(
                    "  sent through the running device as class 0x0A records; stream CRC-16 {c:#06x}"
                ),
                None => "  no bootloader entry in the config".into(),
            }),
        }
    }
    out
}

const USAGE: &str = "usage: firmware inspect UPDATER.EXE
       firmware flash UPDATER.EXE --image NAME --dry-run [--device-version X.YY.ZZ] [--allow-reflash]";

fn read_updater(path: &str) -> Result<(Vec<u8>, Inspection), String> {
    let exe = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    let ins = inspect(&exe).map_err(|e| format!("{path}: {e}"))?;
    Ok((exe, ins))
}

/// `firmware flash UPDATER.EXE --image NAME --dry-run`: run the DFU state machine against the simulated device and print
/// every frame. Without `--dry-run` it refuses before it reads anything.
fn flash_cmd(args: &[String]) -> Result<(), String> {
    let (mut path, mut image, mut dry, mut version, mut reflash) = (None, None, false, None, false);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--dry-run" => dry = true,
            "--allow-reflash" => reflash = true,
            "--image" => image = Some(it.next().ok_or("--image needs a name")?.as_str()),
            "--device-version" => {
                version = Some(it.next().ok_or("--device-version needs X.YY.ZZ")?.as_str())
            }
            o if o.starts_with('-') => return Err(format!("unexpected argument '{o}'\n{USAGE}")),
            o if path.is_none() => path = Some(o),
            _ => return Err(USAGE.into()),
        }
    }
    if !dry {
        return Err("flashing is not supported yet: `firmware flash` only rehearses, with --dry-run, against a simulated \
                    device, and nothing is sent to any real device"
            .into());
    }
    let (path, image) = (
        path.ok_or(USAGE)?,
        image.ok_or("--image NAME is needed (see `firmware inspect`)")?,
    );
    let device_version = match version {
        None => dfu::Version(1, 4, 0),
        Some(v) => dfu::Version::parse(v)
            .ok_or_else(|| format!("--device-version '{v}' is not X.YY.ZZ"))?,
    };
    let (_, ins) = read_updater(path)?;
    let img = ins.images.iter().find(|i| i.name == image).ok_or_else(|| {
        let names: Vec<&str> = ins.images.iter().map(|i| i.name.as_str()).collect();
        format!(
            "{path}: no image named '{image}' (it has {})",
            names.join(", ")
        )
    })?;
    eprintln!(
        "{}: dry run of {} ({:04x}:{:04x}, {} bytes, version {}) against a simulated device reporting {device_version}; \
         every line below is a frame that WOULD be sent (trailing 00 bytes of the args are left out), and nothing is sent to anything",
        path,
        img.name,
        img.vid,
        img.pid,
        img.image.data.len(),
        img.version
    );
    let mut sim = sim::DryRun::new(std::io::stdout(), (img.vid, img.pid), device_version);
    let outcome = dfu::flash(
        &mut sim,
        img,
        &dfu::Options {
            allow_reflash: reflash,
        },
    )
    .map_err(|e| format!("dry run stopped: {e}"))?;
    eprintln!(
        "dry run complete: {} frames, {} packets programmed and {} read back (simulated device; nothing was sent)",
        sim.frames_sent(),
        outcome.programmed,
        outcome.verified
    );
    Ok(())
}

/// `razerctl firmware inspect|flash ...`. Offline: no device is opened.
pub fn cli(args: &[String]) -> Result<(), String> {
    match args {
        [sub, path] if sub == "inspect" => {
            let (exe, ins) = read_updater(path)?;
            let sha = crate::sha256::hex_digest(&exe[..]).map_err(|e| e.to_string())?;
            for l in describe(path, &sha, &ins) {
                println!("{l}");
            }
            Ok(())
        }
        [sub, rest @ ..] if sub == "flash" => flash_cmd(rest),
        _ => Err(USAGE.into()),
    }
}
