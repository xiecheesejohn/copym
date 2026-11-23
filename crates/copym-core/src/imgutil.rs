//! Minimal PNG helpers (decode to RGBA8, encode, thumbnail) built on the `png` crate.

use std::io::Cursor;

/// Decode any PNG into tightly packed RGBA8.
pub fn decode_png(bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let mut dec = png::Decoder::new(Cursor::new(bytes));
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = dec.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    buf.truncate(info.buffer_size());
    let (w, h) = (info.width, info.height);
    let px = (w as usize) * (h as usize);
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => {
            let mut out = Vec::with_capacity(px * 4);
            for c in buf.chunks_exact(3) {
                out.extend_from_slice(&[c[0], c[1], c[2], 255]);
            }
            out
        }
        png::ColorType::GrayscaleAlpha => {
            let mut out = Vec::with_capacity(px * 4);
            for c in buf.chunks_exact(2) {
                out.extend_from_slice(&[c[0], c[0], c[0], c[1]]);
            }
            out
        }
        png::ColorType::Grayscale => {
            let mut out = Vec::with_capacity(px * 4);
            for &g in &buf {
                out.extend_from_slice(&[g, g, g, 255]);
            }
            out
        }
        png::ColorType::Indexed => return None, // EXPAND removes palettes
    };
    (rgba.len() == px * 4).then_some((w, h, rgba))
}

pub fn encode_png(w: u32, h: u32, rgba: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        let mut writer = enc.write_header().ok()?;
        writer.write_image_data(rgba).ok()?;
    }
    Some(out)
}

pub fn encode_gray_png(w: u32, h: u32, gray: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Grayscale);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        let mut writer = enc.write_header().ok()?;
        writer.write_image_data(gray).ok()?;
    }
    Some(out)
}

/// Composite onto a white background (transparent screenshots otherwise
/// read as black text on black for OCR engines).
pub fn flatten_on_white(rgba: &mut [u8]) {
    for p in rgba.chunks_exact_mut(4) {
        let a = p[3] as u32;
        if a < 255 {
            for c in &mut p[..3] {
                *c = ((*c as u32 * a + 255 * (255 - a)) / 255) as u8;
            }
            p[3] = 255;
        }
    }
}

/// Luma (BT.601) of opaque RGBA pixels.
pub fn to_gray(rgba: &[u8]) -> Vec<u8> {
    rgba.chunks_exact(4)
        .map(|p| ((p[0] as u32 * 299 + p[1] as u32 * 587 + p[2] as u32 * 114) / 1000) as u8)
        .collect()
}

/// Bilinear upscale of a grayscale image by an integer factor.
pub fn upscale_gray(w: u32, h: u32, gray: &[u8], factor: u32) -> (u32, u32, Vec<u8>) {
    if factor <= 1 {
        return (w, h, gray.to_vec());
    }
    let (tw, th) = (w * factor, h * factor);
    let f = factor as f32;
    let mut out = vec![0u8; (tw * th) as usize];
    let at = |x: u32, y: u32| gray[(y.min(h - 1) * w + x.min(w - 1)) as usize] as f32;
    for ty in 0..th {
        let sy = ((ty as f32 + 0.5) / f - 0.5).max(0.0);
        let (y0, fy) = (sy.floor() as u32, sy.fract());
        for tx in 0..tw {
            let sx = ((tx as f32 + 0.5) / f - 0.5).max(0.0);
            let (x0, fx) = (sx.floor() as u32, sx.fract());
            let top = at(x0, y0) * (1.0 - fx) + at(x0 + 1, y0) * fx;
            let bottom = at(x0, y0 + 1) * (1.0 - fx) + at(x0 + 1, y0 + 1) * fx;
            out[(ty * tw + tx) as usize] = (top * (1.0 - fy) + bottom * fy).round() as u8;
        }
    }
    (tw, th, out)
}

/// Crop rows `y0..y1` of an RGBA image.
pub fn crop_rows(w: u32, rgba: &[u8], y0: u32, y1: u32) -> Vec<u8> {
    rgba[(y0 * w * 4) as usize..(y1 * w * 4) as usize].to_vec()
}

/// Read width/height straight from the IHDR chunk.
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
pub fn png_size(bytes: &[u8]) -> Option<(u32, u32)> {
    const SIG: &[u8] = b"\x89PNG\r\n\x1a\n";
    if bytes.len() < 24 || &bytes[..8] != SIG || &bytes[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
    Some((w, h))
}

/// Box-filter downscale so the longest side is at most `max`.
pub fn thumbnail(w: u32, h: u32, rgba: &[u8], max: u32) -> (u32, u32, Vec<u8>) {
    if w <= max && h <= max {
        return (w, h, rgba.to_vec());
    }
    let scale = max as f64 / w.max(h) as f64;
    let tw = ((w as f64 * scale).round() as u32).max(1);
    let th = ((h as f64 * scale).round() as u32).max(1);
    let mut out = vec![0u8; (tw * th * 4) as usize];
    for ty in 0..th {
        let y0 = (ty as u64 * h as u64 / th as u64) as u32;
        let y1 = (((ty + 1) as u64 * h as u64 / th as u64) as u32).max(y0 + 1).min(h);
        for tx in 0..tw {
            let x0 = (tx as u64 * w as u64 / tw as u64) as u32;
            let x1 = (((tx + 1) as u64 * w as u64 / tw as u64) as u32).max(x0 + 1).min(w);
            let mut acc = [0u64; 4];
            for y in y0..y1 {
                let row = (y * w) as usize * 4;
                for x in x0..x1 {
                    let i = row + x as usize * 4;
                    for c in 0..4 {
                        acc[c] += rgba[i + c] as u64;
                    }
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as u64;
            let o = ((ty * tw + tx) * 4) as usize;
            for c in 0..4 {
                out[o + c] = (acc[c] / n) as u8;
            }
        }
    }
    (tw, th, out)
}

/// Produce a PNG thumbnail from PNG bytes.
pub fn png_thumbnail(png_bytes: &[u8], max: u32) -> Option<Vec<u8>> {
    let (w, h, rgba) = decode_png(png_bytes)?;
    let (tw, th, t) = thumbnail(w, h, &rgba, max);
    encode_png(tw, th, &t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_thumb() {
        let (w, h) = (800, 400);
        let rgba: Vec<u8> = (0..w * h).flat_map(|i| [(i % 255) as u8, 10, 200, 255]).collect();
        let png = encode_png(w, h, &rgba).unwrap();
        assert_eq!(png_size(&png), Some((w, h)));
        let (dw, dh, d) = decode_png(&png).unwrap();
        assert_eq!((dw, dh), (w, h));
        assert_eq!(d, rgba);
        let t = png_thumbnail(&png, 200).unwrap();
        assert_eq!(png_size(&t), Some((200, 100)));
    }

    #[test]
    fn ocr_helpers() {
        let mut px = vec![0, 0, 0, 0, 0, 0, 0, 255, 200, 100, 50, 128];
        flatten_on_white(&mut px);
        assert_eq!(&px[..4], &[255, 255, 255, 255]); // transparent -> white
        assert_eq!(&px[4..8], &[0, 0, 0, 255]); // opaque black stays
        let g = to_gray(&px);
        assert_eq!(g[0], 255);
        let (w, h, up) = upscale_gray(2, 1, &[0, 255], 2);
        assert_eq!((w, h), (4, 2));
        assert_eq!(up[0], 0);
        assert_eq!(up[3], 255);
        assert!(up[1] > 0 && up[1] < up[2]);
        let png = encode_gray_png(4, 2, &up).unwrap();
        assert_eq!(png_size(&png), Some((4, 2)));
    }
}
