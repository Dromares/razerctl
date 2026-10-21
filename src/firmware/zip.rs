//! Carve the ZIP that the vendor's self-extracting updater carries after its own .NET image (offline, `std` plus
//! `miniz_oxide` for inflate). The archive is found by signature, never by a fixed offset: the End Of Central Directory
//! record is located from the back, the central directory sits right before it, and the difference to the offset the
//! record claims is where the archive starts inside the file.

use std::collections::HashSet;

const EOCD_SIG: [u8; 4] = *b"PK\x05\x06";
const CEN_SIG: [u8; 4] = *b"PK\x01\x02";
const LOC_SIG: [u8; 4] = *b"PK\x03\x04";
const EOCD_LEN: usize = 22;
const CEN_LEN: usize = 46;
const LOC_LEN: usize = 30;
/// Longest ZIP comment, so the furthest back the EOCD record can start.
const MAX_COMMENT: usize = 0xFFFF;
/// Refuse to inflate a member beyond this (the real resource set is 1 MiB; this stops a decompression bomb).
pub const MAX_MEMBER: usize = 64 << 20;

#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    method: u16,
    crc32: u32,
    comp_size: usize,
    pub size: usize,
    /// Offset of the local header from the start of the archive.
    local: usize,
}

#[derive(Debug)]
pub struct Zip<'a> {
    data: &'a [u8],
    /// Offset of the archive's first byte inside the file.
    pub offset: usize,
    pub entries: Vec<Entry>,
}

fn u16_at(d: &[u8], i: usize) -> usize {
    u16::from_le_bytes([d[i], d[i + 1]]) as usize
}
fn u32_at(d: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([d[i], d[i + 1], d[i + 2], d[i + 3]])
}

/// CRC-32/ISO-HDLC (the ZIP checksum), bitwise: members are a few MiB at most.
pub fn crc32(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = (c >> 1) ^ (0xEDB8_8320 & (c & 1).wrapping_neg());
        }
    }
    !c
}

impl<'a> Zip<'a> {
    /// Find and index the archive appended to `data`. Errors name what is wrong with it.
    pub fn find(data: &'a [u8]) -> Result<Zip<'a>, String> {
        if data.len() < EOCD_LEN {
            return Err("file is too short to hold a ZIP archive".into());
        }
        let lowest = data.len().saturating_sub(EOCD_LEN + MAX_COMMENT);
        let mut last_err = None;
        for pos in (lowest..=data.len() - EOCD_LEN).rev() {
            if data[pos..pos + 4] != EOCD_SIG {
                continue;
            }
            match Self::index(data, pos) {
                Ok(z) => return Ok(z),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| {
            "no ZIP end-of-central-directory record found (the file is truncated, or not a self-extracting updater)"
                .into()
        }))
    }

    fn index(data: &'a [u8], eocd: usize) -> Result<Zip<'a>, String> {
        let count = u16_at(data, eocd + 10);
        let cd_size = u32_at(data, eocd + 12) as usize;
        let cd_off = u32_at(data, eocd + 16) as usize;
        if u16_at(data, eocd + 4) != 0 || u16_at(data, eocd + 6) != 0 {
            return Err("ZIP spans several disks, which is not supported".into());
        }
        if count == 0xFFFF || cd_size == 0xFFFF_FFFF || cd_off == 0xFFFF_FFFF {
            return Err("ZIP64 archives are not supported".into());
        }
        let cd_start = eocd
            .checked_sub(cd_size)
            .ok_or("ZIP central directory is larger than the data before it (truncated file)")?;
        let offset = cd_start.checked_sub(cd_off).ok_or(
            "ZIP central directory offset points before the start of the file (truncated or damaged file)",
        )?;
        let mut entries = Vec::with_capacity(count);
        let mut p = cd_start;
        for n in 0..count {
            if p + CEN_LEN > eocd || data[p..p + 4] != CEN_SIG {
                return Err(format!(
                    "ZIP central directory entry {n} is missing or damaged"
                ));
            }
            let (nlen, xlen, clen) = (
                u16_at(data, p + 28),
                u16_at(data, p + 30),
                u16_at(data, p + 32),
            );
            let end = p + CEN_LEN + nlen + xlen + clen;
            if end > eocd {
                return Err(format!(
                    "ZIP central directory entry {n} runs past the directory"
                ));
            }
            if u16_at(data, p + 8) & 1 != 0 {
                return Err("ZIP member is encrypted, which is not supported".into());
            }
            entries.push(Entry {
                name: String::from_utf8_lossy(&data[p + CEN_LEN..p + CEN_LEN + nlen]).into_owned(),
                method: u16_at(data, p + 10) as u16,
                crc32: u32_at(data, p + 16),
                comp_size: u32_at(data, p + 20) as usize,
                size: u32_at(data, p + 24) as usize,
                local: u32_at(data, p + 42) as usize,
            });
            p = end;
        }
        if p != eocd {
            return Err(
                "ZIP central directory does not end at the end-of-central-directory record".into(),
            );
        }
        let mut seen = HashSet::new();
        for e in &entries {
            if !seen.insert(e.name.as_str()) {
                return Err(format!("ZIP has two members named {}", e.name));
            }
        }
        Ok(Zip {
            data,
            offset,
            entries,
        })
    }

    /// The member called `name` (a path ending in it also matches, `\` and `/` alike).
    pub fn entry(&self, name: &str) -> Option<&Entry> {
        self.entries
            .iter()
            .find(|e| e.name.replace('\\', "/").rsplit('/').next() == Some(name))
    }

    /// Inflate (or copy) one member and check its size and CRC-32.
    pub fn read(&self, e: &Entry) -> Result<Vec<u8>, String> {
        let at = self.offset + e.local;
        let fail = |what: &str| format!("ZIP member {}: {what}", e.name);
        if at + LOC_LEN > self.data.len() || self.data[at..at + 4] != LOC_SIG {
            return Err(fail("local header is missing (truncated or damaged file)"));
        }
        let start = at + LOC_LEN + u16_at(self.data, at + 26) + u16_at(self.data, at + 28);
        let raw = start
            .checked_add(e.comp_size)
            .and_then(|end| self.data.get(start..end))
            .ok_or_else(|| fail("data runs past the end of the file (truncated)"))?;
        if e.size > MAX_MEMBER {
            return Err(fail(&format!(
                "claims {} bytes, over the {MAX_MEMBER} byte limit",
                e.size
            )));
        }
        let out = match e.method {
            0 => raw.to_vec(),
            8 => miniz_oxide::inflate::decompress_to_vec_with_limit(raw, MAX_MEMBER)
                .map_err(|err| fail(&format!("inflate failed: {:?}", err.status)))?,
            m => return Err(fail(&format!("compression method {m} is not supported"))),
        };
        if out.len() != e.size {
            return Err(fail(&format!(
                "inflated to {} bytes, the directory says {}",
                out.len(),
                e.size
            )));
        }
        if crc32(&out) != e.crc32 {
            return Err(fail("CRC-32 mismatch (damaged data)"));
        }
        Ok(out)
    }
}
