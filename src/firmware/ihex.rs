//! Intel-HEX to a flat image, the way the vendor updater flattens it: data records laid out by address, 0xFF in the gaps,
//! `04` (extended linear address) honoured, `05` (start linear address) ignored. Every record's checksum is verified.

/// Largest span between the first and last data byte that is accepted (the real images are under 100 KiB).
pub const MAX_SPAN: u64 = 16 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    /// Address of `data[0]`.
    pub start: u32,
    pub data: Vec<u8>,
}

impl Image {
    /// One past the last byte.
    pub fn end(&self) -> u64 {
        self.start as u64 + self.data.len() as u64
    }
}

fn hex_byte(b: &[u8]) -> Option<u8> {
    let d = |c: u8| (c as char).to_digit(16);
    Some((d(b[0])? * 16 + d(b[1])?) as u8)
}

/// Assemble `lines` (each `(label, text)`, the label only names the line in errors) into one image.
pub fn assemble<'a>(lines: impl IntoIterator<Item = (&'a str, &'a str)>) -> Result<Image, String> {
    let mut base: u64 = 0;
    let mut chunks: Vec<(u64, Vec<u8>, &str)> = Vec::new();
    let mut eof = false;
    for (label, text) in lines {
        let fail = |what: &str| format!("{label}: {what}");
        if eof {
            return Err(fail("record after the end-of-file record"));
        }
        let t = text.trim();
        let body = t
            .strip_prefix(':')
            .ok_or_else(|| fail("record does not start with ':'"))?;
        if body.len() % 2 != 0 || body.len() < 10 || !body.is_ascii() {
            return Err(fail("record is not whole hex bytes, or too short"));
        }
        let bytes: Vec<u8> = body
            .as_bytes()
            .chunks(2)
            .map(hex_byte)
            .collect::<Option<_>>()
            .ok_or_else(|| fail("record has a non-hex character"))?;
        let len = bytes[0] as usize;
        if bytes.len() != len + 5 {
            return Err(fail(&format!(
                "length byte says {len} data bytes, record has {}",
                bytes.len() - 5
            )));
        }
        let sum = bytes[..bytes.len() - 1]
            .iter()
            .fold(0u8, |a, &b| a.wrapping_add(b));
        let want = sum.wrapping_neg();
        let got = bytes[bytes.len() - 1];
        if want != got {
            return Err(fail(&format!(
                "bad checksum (record says {got:#04x}, computed {want:#04x})"
            )));
        }
        let addr = u16::from_be_bytes([bytes[1], bytes[2]]) as u64;
        let data = &bytes[4..4 + len];
        match bytes[3] {
            0x00 => chunks.push((base + addr, data.to_vec(), label)),
            0x01 => {
                if len != 0 {
                    return Err(fail("end-of-file record carries data"));
                }
                eof = true;
            }
            0x04 => {
                if len != 2 {
                    return Err(fail("extended linear address record is not 2 bytes"));
                }
                base = (u16::from_be_bytes([data[0], data[1]]) as u64) << 16;
            }
            0x05 => {}
            t => return Err(fail(&format!("record type {t:#04x} is not supported"))),
        }
    }
    if !eof {
        return Err("no end-of-file record (truncated image)".into());
    }
    let start = chunks
        .iter()
        .map(|c| c.0)
        .min()
        .ok_or("image has no data records")?;
    let end = chunks.iter().map(|c| c.0 + c.1.len() as u64).max().unwrap();
    if end > 1 << 32 {
        return Err(format!(
            "image runs past the 32-bit address space (ends at {end:#x})"
        ));
    }
    if end - start > MAX_SPAN {
        return Err(format!(
            "image spans {} bytes, over the {MAX_SPAN} byte limit",
            end - start
        ));
    }
    let mut data = vec![0xFFu8; (end - start) as usize];
    let mut taken = vec![false; data.len()];
    for (addr, bytes, label) in &chunks {
        let at = (addr - start) as usize;
        for (i, &b) in bytes.iter().enumerate() {
            if taken[at + i] {
                return Err(format!(
                    "{label}: overlaps an earlier record at {:#x}",
                    addr + i as u64
                ));
            }
            taken[at + i] = true;
            data[at + i] = b;
        }
    }
    Ok(Image {
        start: start as u32,
        data,
    })
}
