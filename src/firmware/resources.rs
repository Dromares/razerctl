//! Reader for the .NET `ResourceReader` set format (`*.resources`, magic 0xBEEFCACE), string and small-integer entries.
//! That is all the updater's `DeviceUpdater.resources` holds: device config plus the firmware as Intel-HEX text lines. Layout (MS-NRBF
//! "Resource Files", set version 2): magic, header version, header size, reader type, set type, set version, entry
//! count, type table, 8-byte pad, name hashes, name offsets, data-section offset, then the names and the data.

use std::collections::HashMap;

const MAGIC: u32 = 0xBEEF_CACE;
/// `ResourceTypeCode` values this reader understands: strings, and the small integer types the config uses (read as
/// decimal text). Everything else (byte arrays, streams, user types) is refused by name.
const CODE_STRING: u32 = 1;
const CODE_BOOL: u32 = 2;
const CODE_BYTE: u32 = 4;
const CODE_INT16: u32 = 6;
const CODE_UINT16: u32 = 7;
const CODE_INT32: u32 = 8;
const CODE_UINT32: u32 = 9;

struct Cur<'a> {
    d: &'a [u8],
    p: usize,
}

impl<'a> Cur<'a> {
    fn take(&mut self, n: usize, what: &str) -> Result<&'a [u8], String> {
        let s = self
            .p
            .checked_add(n)
            .and_then(|e| self.d.get(self.p..e))
            .ok_or_else(|| format!("resource set is truncated while reading {what}"))?;
        self.p += n;
        Ok(s)
    }
    fn i32(&mut self, what: &str) -> Result<i32, String> {
        Ok(i32::from_le_bytes(self.take(4, what)?.try_into().unwrap()))
    }
    fn count(&mut self, what: &str) -> Result<usize, String> {
        let v = self.i32(what)?;
        usize::try_from(v).map_err(|_| format!("resource set has a negative {what} ({v})"))
    }
    /// .NET `Write7BitEncodedInt`: 7 bits per byte, low group first, at most 5 bytes.
    fn v7(&mut self, what: &str) -> Result<u32, String> {
        let mut v = 0u32;
        for i in 0..5 {
            let b = self.take(1, what)?[0];
            v |= ((b & 0x7f) as u32) << (7 * i);
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(format!(
            "resource set has a malformed length while reading {what}"
        ))
    }
    fn utf8(&mut self, what: &str) -> Result<String, String> {
        let n = self.v7(what)? as usize;
        Ok(String::from_utf8_lossy(self.take(n, what)?).into_owned())
    }
}

/// Every entry of the set as `name -> string`. Errors name the part of the layout that is wrong.
pub fn parse(data: &[u8]) -> Result<HashMap<String, String>, String> {
    let mut c = Cur { d: data, p: 0 };
    if c.take(4, "the magic number")? != MAGIC.to_le_bytes() {
        return Err("not a .NET resource set (bad magic number)".into());
    }
    let header_version = c.i32("the header version")?;
    let header_size = c.count("header size")?;
    if header_version != 1 {
        return Err(format!(
            "unknown resource layout: header version {header_version} (expected 1)"
        ));
    }
    let header_end = c.p + header_size;
    let reader = c.utf8("the reader type")?;
    let set = c.utf8("the resource set type")?;
    if !reader.starts_with("System.Resources.ResourceReader,")
        || !set.starts_with("System.Resources.RuntimeResourceSet")
    {
        return Err(format!(
            "unknown resource layout: reader '{reader}', set '{set}'"
        ));
    }
    if c.p != header_end {
        return Err(
            "unknown resource layout: the header size does not match the reader and set type names"
                .into(),
        );
    }
    let version = c.i32("the set version")?;
    if version != 2 {
        return Err(format!(
            "unknown resource layout: set version {version} (expected 2)"
        ));
    }
    let n = c.count("entry count")?;
    let types = c.count("type count")?;
    if n > data.len() / 8 || types > data.len() {
        return Err("resource set claims more entries than it has bytes".into());
    }
    for _ in 0..types {
        c.utf8("a type name")?;
    }
    c.p = c.p.next_multiple_of(8);
    c.take(4 * n, "the name hashes")?;
    let mut name_pos = Vec::with_capacity(n);
    for _ in 0..n {
        name_pos.push(c.count("a name offset")?);
    }
    let data_start = c.count("data section offset")?;
    let names_start = c.p;
    let mut out = HashMap::with_capacity(n);
    for (i, &np) in name_pos.iter().enumerate() {
        let mut nc = Cur {
            d: data,
            p: names_start + np,
        };
        let len = nc.v7("a name")? as usize;
        if len % 2 != 0 {
            return Err(format!("resource name {i} has an odd UTF-16 byte length"));
        }
        let units: Vec<u16> = nc
            .take(len, "a name")?
            .chunks(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect();
        let name = String::from_utf16(&units)
            .map_err(|_| format!("resource name {i} is not valid UTF-16"))?;
        let off = nc.count("a value offset")?;
        let mut vc = Cur {
            d: data,
            p: data_start
                .checked_add(off)
                .ok_or_else(|| format!("resource '{name}': value offset overflows"))?,
        };
        let value = match vc.v7("a type code")? {
            CODE_STRING => vc.utf8(&format!("the value of '{name}'"))?,
            CODE_BOOL | CODE_BYTE => vc.take(1, "an integer")?[0].to_string(),
            CODE_INT16 => i16::from_le_bytes(vc.take(2, "an integer")?.try_into().unwrap()).to_string(),
            CODE_UINT16 => u16::from_le_bytes(vc.take(2, "an integer")?.try_into().unwrap()).to_string(),
            CODE_INT32 => i32::from_le_bytes(vc.take(4, "an integer")?.try_into().unwrap()).to_string(),
            CODE_UINT32 => u32::from_le_bytes(vc.take(4, "an integer")?.try_into().unwrap()).to_string(),
            other => {
                return Err(format!("unknown resource layout: '{name}' has type code {other}, only strings and small integers are read"))
            }
        };
        if out.insert(name.clone(), value).is_some() {
            return Err(format!("resource set has two entries named '{name}'"));
        }
    }
    Ok(out)
}
