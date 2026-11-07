//! Minimal image support: PNG/JPEG decode, box-filter downscale, fit helper.
//! Only the picture currently on screen is ever kept in memory.

use std::io::Cursor;

/// Refuse to decode anything bigger than this (decompression-bomb guard).
const MAX_PIXELS: u64 = 40_000_000;

#[derive(Clone, Debug)]
pub struct Pic {
    pub w: u32,
    pub h: u32,
    pub rgb: Vec<u8>,
}

impl Pic {
    pub fn px(&self, x: u32, y: u32) -> [u8; 3] {
        let i = ((y.min(self.h - 1) * self.w + x.min(self.w - 1)) * 3) as usize;
        [self.rgb[i], self.rgb[i + 1], self.rgb[i + 2]]
    }
}

pub fn decode(bytes: &[u8]) -> Result<Pic, String> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        decode_png(bytes)
    } else if bytes.starts_with(&[0xFF, 0xD8]) {
        decode_jpeg(bytes)
    } else {
        Err("unsupported image format (PNG/JPEG only)".into())
    }
}

fn blend(c: u8, a: u8) -> u8 {
    ((c as u16 * a as u16) / 255) as u8 // composite over black
}

fn decode_png(bytes: &[u8]) -> Result<Pic, String> {
    let mut dec = png::Decoder::new(Cursor::new(bytes));
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut r = dec.read_info().map_err(|e| e.to_string())?;
    let (iw, ih) = (r.info().width, r.info().height);
    if iw == 0 || ih == 0 || iw as u64 * ih as u64 > MAX_PIXELS {
        return Err("image too large".into());
    }
    let mut buf = vec![0u8; r.output_buffer_size()];
    let fr = r.next_frame(&mut buf).map_err(|e| e.to_string())?;
    let data = &buf[..fr.buffer_size()];
    let mut rgb = Vec::with_capacity(fr.width as usize * fr.height as usize * 3);
    match fr.color_type {
        png::ColorType::Rgb => rgb.extend_from_slice(data),
        png::ColorType::Rgba => {
            for p in data.chunks_exact(4) {
                rgb.extend_from_slice(&[blend(p[0], p[3]), blend(p[1], p[3]), blend(p[2], p[3])]);
            }
        }
        png::ColorType::Grayscale => {
            for &g in data {
                rgb.extend_from_slice(&[g, g, g]);
            }
        }
        png::ColorType::GrayscaleAlpha => {
            for p in data.chunks_exact(2) {
                let g = blend(p[0], p[1]);
                rgb.extend_from_slice(&[g, g, g]);
            }
        }
        png::ColorType::Indexed => return Err("unexpected indexed PNG".into()),
    }
    Ok(Pic { w: fr.width, h: fr.height, rgb })
}

fn decode_jpeg(bytes: &[u8]) -> Result<Pic, String> {
    let mut d = jpeg_decoder::Decoder::new(Cursor::new(bytes));
    d.read_info().map_err(|e| e.to_string())?;
    let info = d.info().ok_or("no JPEG info")?;
    if info.width == 0 || info.height == 0 || info.width as u64 * info.height as u64 > MAX_PIXELS {
        return Err("image too large".into());
    }
    let px = d.decode().map_err(|e| e.to_string())?;
    let rgb = match info.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => px,
        jpeg_decoder::PixelFormat::L8 => px.iter().flat_map(|&g| [g, g, g]).collect(),
        _ => return Err("unsupported JPEG color format".into()),
    };
    Ok(Pic { w: info.width as u32, h: info.height as u32, rgb })
}

/// Largest size that fits in max_w x max_h and keeps the aspect ratio.
pub fn fit(w: u32, h: u32, max_w: u32, max_h: u32, allow_upscale: bool) -> (u32, u32) {
    let (w, h, max_w, max_h) = (w.max(1) as u64, h.max(1) as u64, max_w.max(1) as u64, max_h.max(1) as u64);
    let (mut dw, mut dh) = if w * max_h > h * max_w {
        (max_w, (h * max_w / w).max(1))
    } else {
        ((w * max_h / h).max(1), max_h)
    };
    if !allow_upscale && (dw > w || dh > h) {
        dw = w;
        dh = h;
    }
    (dw as u32, dh as u32)
}

/// Box-filter downscale to fit max_w x max_h (never upscales).
pub fn downscale(p: Pic, max_w: u32, max_h: u32) -> Pic {
    let (dw, dh) = fit(p.w, p.h, max_w, max_h, false);
    if dw >= p.w && dh >= p.h {
        return p;
    }
    let mut out = Vec::with_capacity(dw as usize * dh as usize * 3);
    for y in 0..dh {
        let sy0 = (y as u64 * p.h as u64 / dh as u64) as u32;
        let sy1 = (((y as u64 + 1) * p.h as u64 / dh as u64) as u32).max(sy0 + 1).min(p.h);
        for x in 0..dw {
            let sx0 = (x as u64 * p.w as u64 / dw as u64) as u32;
            let sx1 = (((x as u64 + 1) * p.w as u64 / dw as u64) as u32).max(sx0 + 1).min(p.w);
            let (mut r, mut g, mut b, mut n) = (0u64, 0u64, 0u64, 0u64);
            for sy in sy0..sy1 {
                let row = (sy * p.w) as usize * 3;
                for sx in sx0..sx1 {
                    let i = row + sx as usize * 3;
                    r += p.rgb[i] as u64;
                    g += p.rgb[i + 1] as u64;
                    b += p.rgb[i + 2] as u64;
                    n += 1;
                }
            }
            out.extend_from_slice(&[(r / n) as u8, (g / n) as u8, (b / n) as u8]);
        }
    }
    Pic { w: dw, h: dh, rgb: out }
}

/// Synthetic test picture used by --demo.
pub fn gradient(w: u32, h: u32) -> Pic {
    let mut rgb = Vec::with_capacity(w as usize * h as usize * 3);
    for y in 0..h {
        for x in 0..w {
            rgb.extend_from_slice(&[(x * 255 / w.max(1)) as u8, (y * 255 / h.max(1)) as u8, 160]);
        }
    }
    Pic { w, h, rgb }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_bytes(w: u32, h: u32, color: png::ColorType, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(color);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header().unwrap().write_image_data(data).unwrap();
        out
    }

    #[test]
    fn decodes_rgb_and_rgba_png() {
        let rgb = png_bytes(2, 1, png::ColorType::Rgb, &[255, 0, 0, 0, 255, 0]);
        let p = decode(&rgb).unwrap();
        assert_eq!((p.w, p.h), (2, 1));
        assert_eq!(p.px(0, 0), [255, 0, 0]);
        assert_eq!(p.px(1, 0), [0, 255, 0]);
        // fully transparent pixel becomes black, opaque stays
        let rgba = png_bytes(2, 1, png::ColorType::Rgba, &[200, 200, 200, 0, 10, 20, 30, 255]);
        let p = decode(&rgba).unwrap();
        assert_eq!(p.px(0, 0), [0, 0, 0]);
        assert_eq!(p.px(1, 0), [10, 20, 30]);
    }

    #[test]
    fn decodes_jpeg() {
        // 8x8 solid-ish JPEG generated with Pillow (see tests/gen note in README)
        let jpg = include_bytes!("../tests/tiny.jpg");
        let p = decode(jpg).unwrap();
        assert_eq!((p.w, p.h), (8, 8));
        let [r, g, b] = p.px(4, 4);
        assert!(r > 200 && g < 60 && b < 60, "expected red, got {r},{g},{b}");
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode(b"GIF89a....").is_err());
        assert!(decode(&[]).is_err());
    }

    #[test]
    fn fit_keeps_aspect_and_limits_upscale() {
        assert_eq!(fit(200, 100, 100, 100, false), (100, 50));
        assert_eq!(fit(10, 10, 100, 100, false), (10, 10));
        assert_eq!(fit(10, 10, 100, 100, true), (100, 100));
        assert_eq!(fit(100, 400, 50, 50, false), (12, 50));
    }

    #[test]
    fn downscale_averages() {
        let p = Pic { w: 2, h: 2, rgb: vec![0, 0, 0, 100, 100, 100, 100, 100, 100, 200, 200, 200] };
        let d = downscale(p, 1, 1);
        assert_eq!((d.w, d.h), (1, 1));
        assert_eq!(d.rgb, vec![100, 100, 100]);
    }
}
