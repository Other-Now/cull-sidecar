//! Per-photo analysis: decode, thumbnail, sharpness (Laplacian variance),
//! perceptual hash (DCT pHash) and a few EXIF fields.

use anyhow::{Context, Result};
use image::{imageops, DynamicImage, GrayImage};
use std::io::Cursor;
use std::path::{Path, PathBuf};

pub const THUMB_SIDE: u32 = 256;
/// Sharpness is measured on a copy capped at this long side so the score does
/// not depend on sensor resolution.
pub const SHARPNESS_SIDE: u32 = 1024;

#[derive(Debug, Clone)]
pub struct Analysis {
    pub width: u32,
    pub height: u32,
    pub sharpness: f64,
    pub phash: u64,
    pub camera: Option<String>,
    pub taken_at: Option<String>,
    pub exposure: Option<String>,
    pub iso: Option<i64>,
    pub thumb: PathBuf,
}

/// Decode `path`, write its thumbnail into `thumbs_dir` (cache keyed by
/// path+size+mtime, so an unchanged file is never re-encoded) and score it.
pub fn analyze(path: &Path, size: u64, mtime: i64, thumbs_dir: &Path) -> Result<Analysis> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let (camera, taken_at, exposure, iso) = read_exif(&bytes);
    let img =
        image::load_from_memory(&bytes).with_context(|| format!("decode {}", path.display()))?;
    drop(bytes);

    let thumb = thumb_path(thumbs_dir, path, size, mtime);
    if !thumb.exists() {
        let t = img.thumbnail(THUMB_SIDE, THUMB_SIDE).to_rgb8();
        if let Some(dir) = thumb.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // write-then-rename: a crash mid-write never leaves a torn thumbnail
        // under the final name.
        let tmp = thumb.with_extension("tmp");
        t.save_with_format(&tmp, image::ImageFormat::Jpeg)?;
        std::fs::rename(&tmp, &thumb)?;
    }

    let gray = analysis_gray(&img);
    Ok(Analysis {
        width: img.width(),
        height: img.height(),
        sharpness: laplacian_variance(&gray),
        phash: phash(&gray),
        camera,
        taken_at,
        exposure,
        iso,
        thumb,
    })
}

pub fn analysis_gray(img: &DynamicImage) -> GrayImage {
    if img.width().max(img.height()) > SHARPNESS_SIDE {
        img.thumbnail(SHARPNESS_SIDE, SHARPNESS_SIDE).to_luma8()
    } else {
        img.to_luma8()
    }
}

pub fn thumb_path(thumbs_dir: &Path, path: &Path, size: u64, mtime: i64) -> PathBuf {
    let key = fnv1a(format!("{}|{}|{}", path.display(), size, mtime).as_bytes());
    let hex = format!("{key:016x}");
    thumbs_dir.join(&hex[..2]).join(format!("{hex}.jpg"))
}

pub fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Variance of the 4-neighbour Laplacian. Low = few edges = likely blurred.
/// Only comparable between frames of similar content (see README findings).
pub fn laplacian_variance(g: &GrayImage) -> f64 {
    let (w, h) = (g.width() as usize, g.height() as usize);
    if w < 3 || h < 3 {
        return 0.0;
    }
    let px = g.as_raw();
    let (mut sum, mut sum_sq) = (0f64, 0f64);
    for y in 1..h - 1 {
        let row = y * w;
        for x in 1..w - 1 {
            let i = row + x;
            let l = px[i - w] as i32 + px[i + w] as i32 + px[i - 1] as i32 + px[i + 1] as i32
                - 4 * px[i] as i32;
            let l = l as f64;
            sum += l;
            sum_sq += l * l;
        }
    }
    let n = ((w - 2) * (h - 2)) as f64;
    let mean = sum / n;
    sum_sq / n - mean * mean
}

/// Classic DCT pHash: 32x32 grayscale -> 2D DCT-II -> top-left 8x8 block ->
/// one bit per coefficient, set when above the median (DC excluded from median).
pub fn phash(g: &GrayImage) -> u64 {
    const N: usize = 32;
    const K: usize = 8;
    let small = imageops::resize(g, N as u32, N as u32, imageops::FilterType::Triangle);
    let p = small.as_raw();
    let mut cos = [[0f64; N]; K];
    for (u, row) in cos.iter_mut().enumerate() {
        for (x, c) in row.iter_mut().enumerate() {
            *c =
                (((2 * x + 1) as f64) * (u as f64) * std::f64::consts::PI / (2.0 * N as f64)).cos();
        }
    }
    // rows first (only the K lowest frequencies are needed), then columns
    let mut r = [[0f64; K]; N];
    for y in 0..N {
        for u in 0..K {
            r[y][u] = (0..N).map(|x| p[y * N + x] as f64 * cos[u][x]).sum();
        }
    }
    let mut d = [0f64; K * K];
    for v in 0..K {
        for u in 0..K {
            d[v * K + u] = (0..N).map(|y| r[y][u] * cos[v][y]).sum();
        }
    }
    let mut ac: Vec<f64> = d[1..].to_vec();
    ac.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = (ac[31] + ac[32]) / 2.0;
    d.iter()
        .enumerate()
        .fold(0u64, |h, (i, &c)| if c > median { h | (1 << i) } else { h })
}

pub fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

type ExifFields = (Option<String>, Option<String>, Option<String>, Option<i64>);

fn read_exif(bytes: &[u8]) -> ExifFields {
    let Ok(ex) = exif::Reader::new().read_from_container(&mut Cursor::new(bytes)) else {
        return (None, None, None, None);
    };
    let s = |tag| {
        ex.get_field(tag, exif::In::PRIMARY)
            .map(|f| f.display_value().to_string().trim_matches('"').to_string())
    };
    let iso = ex
        .get_field(exif::Tag::PhotographicSensitivity, exif::In::PRIMARY)
        .and_then(|f| f.value.get_uint(0))
        .map(|v| v as i64);
    (
        s(exif::Tag::Model),
        s(exif::Tag::DateTimeOriginal),
        s(exif::Tag::ExposureTime),
        iso,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{GrayImage, Luma};

    /// Sum of Gaussian blobs (rich low-frequency spectrum, like a real scene)
    /// plus a fine checker texture that blur destroys.
    fn scene(w: u32, h: u32, shift: u32, seed: u64) -> GrayImage {
        let mut s = seed;
        let mut r = || {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (s >> 33) as f64 / (1u64 << 31) as f64
        };
        let blobs: Vec<(f64, f64, f64, f64)> = (0..14)
            .map(|_| {
                (
                    r() * 360.0,
                    r() * 260.0,
                    20.0 + r() * 60.0,
                    r() * 160.0 - 80.0,
                )
            })
            .collect();
        GrayImage::from_fn(w, h, |x, y| {
            let (xf, yf) = ((x + shift) as f64, y as f64);
            let mut v = 128.0;
            for &(cx, cy, rad, amp) in &blobs {
                v += amp * (-((xf - cx).powi(2) + (yf - cy).powi(2)) / (2.0 * rad * rad)).exp();
            }
            v += if ((x + shift) / 2 + y / 2).is_multiple_of(2) {
                12.0
            } else {
                -12.0
            };
            Luma([v.clamp(0.0, 255.0) as u8])
        })
    }

    #[test]
    fn blur_lowers_laplacian_variance() {
        let s = scene(300, 200, 0, 1);
        let b = imageops::blur(&s, 2.5);
        assert!(laplacian_variance(&b) < laplacian_variance(&s) / 5.0);
    }

    #[test]
    fn phash_close_for_same_scene_far_for_different() {
        for seed in 1..20 {
            let a = scene(320, 240, 0, seed);
            let ha = phash(&a);
            let d_shift = hamming(ha, phash(&scene(320, 240, 4, seed)));
            let d_blur = hamming(ha, phash(&imageops::blur(&a, 2.0)));
            let d_other = hamming(ha, phash(&scene(320, 240, 0, seed + 1000)));
            assert!(
                d_shift <= 10 && d_blur <= 10 && d_other > 10,
                "seed {seed}: shift={d_shift} blur={d_blur} other={d_other}"
            );
        }
    }

    #[test]
    fn thumb_key_changes_with_mtime() {
        let d = Path::new("/c");
        let p = Path::new("/a/b.jpg");
        assert_ne!(thumb_path(d, p, 10, 1), thumb_path(d, p, 10, 2));
    }
}
