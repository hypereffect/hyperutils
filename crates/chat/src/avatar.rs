//! The picture of someone who has not set one: a square of one color, whose
//! hue comes from the first bytes of their key, as a `data:` PNG, the one
//! kind of image every hyperb renderer shows.

use base64::Engine;

/// The side of the square, in pixels.
const SIDE: usize = 16;

/// The default picture of the key written as `hex`.
pub fn default_picture(hex: &str) -> String {
    let prefix = hex.get(..4).and_then(|p| u16::from_str_radix(p, 16).ok());
    let hue = f64::from(prefix.unwrap_or(0)) / 65536.0 * 360.0;
    let png = square(hsl(hue, 0.6, 0.5));
    format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(png)
    )
}

/// An RGB color from a hue in degrees and a saturation and lightness from 0
/// to 1.
fn hsl(h: f64, s: f64, l: f64) -> [u8; 3] {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let (r, g, b) = match (h / 60.0) as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    [r, g, b].map(|v| ((v + m) * 255.0).round() as u8)
}

/// A PNG of a [`SIDE`]-pixel square of `rgb`, its data stored uncompressed.
fn square(rgb: [u8; 3]) -> Vec<u8> {
    // Each scanline starts with its filter type: none.
    let line: Vec<u8> = std::iter::once(0).chain(rgb.repeat(SIDE)).collect();
    let raw = line.repeat(SIDE);
    // A zlib stream of one stored deflate block.
    let mut zlib = vec![0x78, 0x01, 0x01];
    let len = raw.len() as u16;
    zlib.extend_from_slice(&len.to_le_bytes());
    zlib.extend_from_slice(&(!len).to_le_bytes());
    zlib.extend_from_slice(&raw);
    zlib.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&(SIDE as u32).to_be_bytes());
    ihdr.extend_from_slice(&(SIDE as u32).to_be_bytes());
    // 8 bits per channel, RGB, deflate, no filter variants, no interlace.
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);

    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    chunk(&mut png, b"IHDR", &ihdr);
    chunk(&mut png, b"IDAT", &zlib);
    chunk(&mut png, b"IEND", &[]);
    png
}

fn chunk(png: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    png.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = png.len();
    png.extend_from_slice(kind);
    png.extend_from_slice(data);
    let crc = crc32(&png[start..]);
    png.extend_from_slice(&crc.to_be_bytes());
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for b in bytes {
        crc ^= u32::from(*b);
        for _ in 0..8 {
            crc = if crc & 1 == 1 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

fn adler32(bytes: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for x in bytes {
        a = (a + u32::from(*x)) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksums() {
        assert_eq!(crc32(b"IEND"), 0xAE42_6082);
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
    }

    #[test]
    fn hues() {
        assert_eq!(hsl(0.0, 0.6, 0.5), [204, 51, 51]);
        assert_eq!(hsl(120.0, 0.6, 0.5), [51, 204, 51]);
        assert_eq!(hsl(240.0, 0.6, 0.5), [51, 51, 204]);
    }

    #[test]
    fn keys_get_their_own_color() {
        let a = default_picture(&"00".repeat(32));
        let b = default_picture(&"80".repeat(32));
        assert!(a.starts_with("data:image/png;base64,iVBORw0KGgo"));
        assert_ne!(a, b);
        assert_eq!(a, default_picture(&format!("0000{}", "ff".repeat(30))));
    }
}
