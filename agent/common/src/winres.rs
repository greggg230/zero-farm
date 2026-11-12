//! Windows resources for our executables, without rc.exe: build scripts write a `.res` file (the
//! round "Z" icon group + VERSIONINFO) and hand it to the MSVC linker; the agent reads an
//! installed launcher's version back out of its VS_FIXEDFILEINFO (PROTOCOL 6: a launcher is only
//! replaced by a newer one).
//!
//! Plain std, so build scripts include this file with `#[path]` (next to `icon.rs`).
#![allow(dead_code)]

/// VS_FIXEDFILEINFO.dwSignature, then dwStrucVersion 1.0, as they appear in a file.
const FIXED_SIG: [u8; 8] = [0xBD, 0x04, 0xEF, 0xFE, 0x00, 0x00, 0x01, 0x00];

/// Where the VS_FIXEDFILEINFO of an executable's VERSIONINFO starts: right after the UTF-16 key
/// "VS_VERSION_INFO" (so a stray copy of the signature elsewhere in the file can't match).
fn fixed_offset(exe: &[u8]) -> Option<usize> {
    let key: Vec<u8> = "VS_VERSION_INFO".encode_utf16().flat_map(|c| c.to_le_bytes()).collect();
    let k = exe.windows(key.len()).position(|w| w == key.as_slice())?;
    let end = (k + 64).min(exe.len());
    Some(k + exe[k..end].windows(FIXED_SIG.len()).position(|w| w == FIXED_SIG)?)
}

/// The file version (major, minor, patch, build) from an executable's VERSIONINFO; `None` when it
/// has none (launchers before 0.2.0).
pub fn fixed_file_version(exe: &[u8]) -> Option<(u16, u16, u16, u16)> {
    let i = fixed_offset(exe)?;
    let dw = |k: usize| -> Option<u32> {
        let b = exe.get(i + 4 * k..i + 4 * k + 4)?;
        Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    let (ms, ls) = (dw(2)?, dw(3)?);
    Some(((ms >> 16) as u16, ms as u16, (ls >> 16) as u16, ls as u16))
}

/// Rewrites the file and product version of an executable's VERSIONINFO in place (tests make an
/// "older" copy of a launcher this way). False when it has none.
pub fn patch_fixed_file_version(exe: &mut [u8], v: (u16, u16, u16)) -> bool {
    let Some(i) = fixed_offset(exe) else { return false };
    let (ms, ls) = (((v.0 as u32) << 16) | v.1 as u32, (v.2 as u32) << 16);
    for (k, x) in [(2, ms), (3, ls), (4, ms), (5, ls)] {
        exe[i + 4 * k..i + 4 * k + 4].copy_from_slice(&x.to_le_bytes());
    }
    true
}

/// "0.2.0" (or "0.2.0-rc.1") → (0, 2, 0).
pub fn version_triple(v: &str) -> (u16, u16, u16) {
    let core = v.split(['-', '+']).next().unwrap_or("");
    let mut it = core.split('.').map(|p| p.parse::<u16>().unwrap_or(0));
    (it.next().unwrap_or(0), it.next().unwrap_or(0), it.next().unwrap_or(0))
}

fn pad4(v: &mut Vec<u8>) {
    while v.len() % 4 != 0 {
        v.push(0);
    }
}

fn utf16z(s: &str) -> Vec<u8> {
    s.encode_utf16().chain(std::iter::once(0)).flat_map(|c| c.to_le_bytes()).collect()
}

/// One VERSIONINFO block: wLength, wValueLength, wType, key, padding, value, then each child on a
/// 32-bit boundary. wLength covers the block and its children.
fn block(key: &str, value: &[u8], value_len: u16, text: bool, children: &[Vec<u8>]) -> Vec<u8> {
    let mut v = vec![0u8; 6];
    v.extend(utf16z(key));
    pad4(&mut v);
    v.extend_from_slice(value);
    for c in children {
        pad4(&mut v);
        v.extend_from_slice(c);
    }
    let len = v.len() as u16;
    v[0..2].copy_from_slice(&len.to_le_bytes());
    v[2..4].copy_from_slice(&value_len.to_le_bytes());
    v[4..6].copy_from_slice(&(text as u16).to_le_bytes());
    v
}

/// The RT_VERSION resource: file and product version `v`, US English strings.
pub fn version_info(v: (u16, u16, u16), strings: &[(&str, &str)]) -> Vec<u8> {
    let (ms, ls) = (((v.0 as u32) << 16) | v.1 as u32, (v.2 as u32) << 16);
    let fixed: Vec<u8> = [0xFEEF04BDu32, 0x0001_0000, ms, ls, ms, ls, 0x3F, 0, 0x0004_0004, 1, 0, 0, 0]
        .iter()
        .flat_map(|d| d.to_le_bytes())
        .collect();
    let entries: Vec<Vec<u8>> = strings
        .iter()
        .map(|(k, val)| {
            let w = utf16z(val);
            block(k, &w, (w.len() / 2) as u16, true, &[])
        })
        .collect();
    let table = block("040904B0", &[], 0, true, &entries);
    let sfi = block("StringFileInfo", &[], 0, true, &[table]);
    let translation: Vec<u8> = [0x0409u16, 0x04B0].iter().flat_map(|x| x.to_le_bytes()).collect();
    let var = block("Translation", &translation, 4, false, &[]);
    let vfi = block("VarFileInfo", &[], 0, true, &[var]);
    block("VS_VERSION_INFO", &fixed, fixed.len() as u16, false, &[sfi, vfi])
}

/// An icon image in an icon group: a 32-bit DIB (with AND mask) for small sizes, PNG for 256.
pub struct IconImage {
    pub size: u32,
    pub data: Vec<u8>,
}

/// A 32-bit BGRA DIB as icon resources store it: BITMAPINFOHEADER (height doubled), bottom-up
/// pixels, then the 1-bit AND mask (set where fully transparent).
pub fn icon_dib(size: u32, px: &[[u8; 4]]) -> IconImage {
    let mask_stride = size.div_ceil(32) * 4;
    let mut v = Vec::new();
    for x in [40u32, size, size * 2] {
        v.extend(x.to_le_bytes());
    }
    v.extend(1u16.to_le_bytes());
    v.extend(32u16.to_le_bytes());
    v.extend(0u32.to_le_bytes()); // BI_RGB
    v.extend((size * size * 4 + mask_stride * size).to_le_bytes());
    v.extend([0u8; 16]);
    for y in (0..size).rev() {
        for x in 0..size {
            let [r, g, b, a] = px[(y * size + x) as usize];
            v.extend([b, g, r, a]);
        }
    }
    for y in (0..size).rev() {
        let mut row = vec![0u8; mask_stride as usize];
        for x in 0..size {
            if px[(y * size + x) as usize][3] == 0 {
                row[(x / 8) as usize] |= 0x80 >> (x % 8);
            }
        }
        v.extend(row);
    }
    IconImage { size, data: v }
}

fn crc32(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
    }
    !c
}

/// A PNG (RGBA, 8 bits) icon image; `zlib` compresses a byte string into a zlib stream.
pub fn icon_png(size: u32, px: &[[u8; 4]], zlib: &dyn Fn(&[u8]) -> Vec<u8>) -> IconImage {
    let mut raw = Vec::with_capacity((size * (size * 4 + 1)) as usize);
    for y in 0..size {
        raw.push(0); // filter: none
        for x in 0..size {
            raw.extend(px[(y * size + x) as usize]);
        }
    }
    let mut png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut chunk = |kind: &[u8; 4], body: &[u8]| {
        png.extend((body.len() as u32).to_be_bytes());
        let mut c = kind.to_vec();
        c.extend_from_slice(body);
        png.extend(&c);
        png.extend(crc32(&c).to_be_bytes());
    };
    let mut ihdr = Vec::new();
    ihdr.extend(size.to_be_bytes());
    ihdr.extend(size.to_be_bytes());
    ihdr.extend([8, 6, 0, 0, 0]); // 8-bit RGBA, deflate, no filter set, no interlace
    chunk(b"IHDR", &ihdr);
    chunk(b"IDAT", &zlib(&raw));
    chunk(b"IEND", &[]);
    IconImage { size, data: png }
}

fn res_entry(out: &mut Vec<u8>, kind: u16, name: u16, flags: u16, lang: u16, data: &[u8]) {
    out.extend((data.len() as u32).to_le_bytes());
    out.extend(32u32.to_le_bytes());
    out.extend([0xFF, 0xFF]);
    out.extend(kind.to_le_bytes());
    out.extend([0xFF, 0xFF]);
    out.extend(name.to_le_bytes());
    out.extend(0u32.to_le_bytes());
    out.extend(flags.to_le_bytes());
    out.extend(lang.to_le_bytes());
    out.extend(0u32.to_le_bytes());
    out.extend(0u32.to_le_bytes());
    out.extend_from_slice(data);
    pad4(out);
}

const RT_ICON: u16 = 3;
const RT_GROUP_ICON: u16 = 14;
const RT_VERSION: u16 = 16;

/// A complete `.res` file: icon group 1 (images 1..n) and VERSIONINFO 1.
pub fn res_file(images: &[IconImage], version: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    res_entry(&mut out, 0, 0, 0, 0, &[]); // the empty entry that marks a 32-bit .res file
    let mut group = Vec::new();
    group.extend(0u16.to_le_bytes());
    group.extend(1u16.to_le_bytes());
    group.extend((images.len() as u16).to_le_bytes());
    for (i, im) in images.iter().enumerate() {
        let id = i as u16 + 1;
        res_entry(&mut out, RT_ICON, id, 0x1010, 0x0409, &im.data);
        let dim = if im.size >= 256 { 0 } else { im.size as u8 };
        group.extend([dim, dim, 0, 0]);
        group.extend(1u16.to_le_bytes());
        group.extend(32u16.to_le_bytes());
        group.extend((im.data.len() as u32).to_le_bytes());
        group.extend(id.to_le_bytes());
    }
    res_entry(&mut out, RT_GROUP_ICON, 1, 0x1030, 0x0409, &group);
    res_entry(&mut out, RT_VERSION, 1, 0x0030, 0x0409, version);
    out
}

/// The `.res` for one of our executables: the green round "Z" at the usual sizes, and a
/// VERSIONINFO saying `description` and `version`.
pub fn app_res(description: &str, version: &str, zlib: &dyn Fn(&[u8]) -> Vec<u8>) -> Vec<u8> {
    let mut images: Vec<IconImage> = [16u32, 20, 24, 32, 40, 48, 64, 96]
        .iter()
        .map(|&s| icon_dib(s, &crate::icon::round_z(s, crate::icon::BRAND_RGB)))
        .collect();
    images.push(icon_png(256, &crate::icon::round_z(256, crate::icon::BRAND_RGB), zlib));
    let v = version_triple(version);
    let info = version_info(
        v,
        &[
            ("FileDescription", description),
            ("ProductName", "Zero Farm"),
            ("FileVersion", version),
            ("ProductVersion", version),
        ],
    );
    res_file(&images, &info)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_roundtrip_and_patch() {
        let info = version_info((0, 2, 0), &[("FileDescription", "Zero Farm")]);
        let mut file = b"MZ....padding....".to_vec();
        file.extend(&info);
        assert_eq!(fixed_file_version(&file), Some((0, 2, 0, 0)));
        assert!(patch_fixed_file_version(&mut file, (0, 0, 1)));
        assert_eq!(fixed_file_version(&file), Some((0, 0, 1, 0)));
        assert_eq!(fixed_file_version(b"MZ no resources here"), None);
        assert_eq!(version_triple("1.20.3-rc.1"), (1, 20, 3));
        // wLength of the root block is the whole resource.
        assert_eq!(u16::from_le_bytes([info[0], info[1]]) as usize, info.len());
    }

    #[test]
    fn res_file_layout() {
        let px = crate::icon::round_z(16, crate::icon::BRAND_RGB);
        let res = res_file(&[icon_dib(16, &px)], &version_info((1, 0, 0), &[]));
        assert_eq!(&res[0..8], &[0, 0, 0, 0, 32, 0, 0, 0], "leading empty entry");
        assert_eq!(res.len() % 4, 0);
        // icon (16x16 DIB: 40 + 1024 + 64 bytes), then the group, then the version
        let size = u32::from_le_bytes(res[32..36].try_into().unwrap());
        assert_eq!(size, 40 + 16 * 16 * 4 + 16 * 4);
        assert_eq!(u16::from_le_bytes([res[42], res[43]]), RT_ICON);
    }
}
