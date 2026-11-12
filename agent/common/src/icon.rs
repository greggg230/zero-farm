//! The round "Z" (DESIGN 3.1): the tray icon (drawn at runtime, coloured by status) and the
//! executables' icon (embedded at build time, green). Plain std, so build scripts include this
//! file with `#[path]`.

/// The "working" green: the exe icon and the tray icon while working.
pub const BRAND_RGB: u32 = 0x2E7D32;

/// Straight (not premultiplied) RGBA pixels, top row first, of a `size`×`size` disc filled with
/// `rgb` (0xRRGGBB) carrying a white "Z", anti-aliased.
pub fn round_z(size: u32, rgb: u32) -> Vec<[u8; 4]> {
    let s = size as f32;
    let (r0, g0, b0) = (((rgb >> 16) & 255) as f32, ((rgb >> 8) & 255) as f32, (rgb & 255) as f32);
    let seg = |x: f32, y: f32, ax: f32, ay: f32, bx: f32, by: f32| -> f32 {
        let (dx, dy) = (bx - ax, by - ay);
        let t = (((x - ax) * dx + (y - ay) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
        ((x - ax - t * dx).powi(2) + (y - ay - t * dy).powi(2)).sqrt()
    };
    let mut out = Vec::with_capacity((size * size) as usize);
    for y in 0..size {
        for x in 0..size {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let d = ((fx - s / 2.0).powi(2) + (fy - s / 2.0).powi(2)).sqrt();
            let a = (s / 2.0 - d).clamp(0.0, 1.0);
            // "Z": top bar, diagonal, bottom bar.
            let (l, r, t, b) = (s * 0.3, s * 0.7, s * 0.3, s * 0.7);
            let w = s * 0.07;
            let dz = seg(fx, fy, l, t, r, t).min(seg(fx, fy, r, t, l, b)).min(seg(fx, fy, l, b, r, b));
            let z = (w + 0.5 - dz).clamp(0.0, 1.0);
            let mix = |c: f32| (c * (1.0 - z) + 255.0 * z) as u8;
            out.push([mix(r0), mix(g0), mix(b0), (a * 255.0) as u8]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn disc_with_a_white_z() {
        let px = super::round_z(32, super::BRAND_RGB);
        assert_eq!(px.len(), 32 * 32);
        assert_eq!(px[0][3], 0, "corners are transparent");
        // Middle of the top bar of the Z: white and opaque; between the bars: green.
        assert_eq!(px[(10 * 32 + 16) as usize], [255, 255, 255, 255]);
        let g = px[(13 * 32 + 9) as usize];
        assert_eq!((g[0], g[1], g[2], g[3]), (0x2E, 0x7D, 0x32, 255));
    }
}
