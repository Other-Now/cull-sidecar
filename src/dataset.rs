//! Labelled evaluation data built from real photos.
//!
//! `gen_bursts` turns each base photo into a "burst" of K frames, the way a
//! photographer's camera does: small framing shifts, small exposure changes,
//! and some frames blurred (missed focus / motion). Labels record which frames
//! share a burst and which are blurred, so grouping and blur detection can be
//! scored. The bursts are SYNTHETIC; the base images are real photographs.

use crate::{db, imaging, import};
use anyhow::{Context, Result};
use image::codecs::jpeg::JpegEncoder;
use image::imageops;
use rayon::prelude::*;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }
    fn f(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

pub fn gen_bursts(
    src: &Path,
    out: &Path,
    bases: usize,
    burst: usize,
    blur_p: f64,
    sigma: (f64, f64),
    seed: u64,
) -> Result<usize> {
    let files = import::walk(src);
    anyhow::ensure!(!files.is_empty(), "no images under {}", src.display());
    let step = (files.len() / bases.max(1)).max(1);
    let picked: Vec<_> = files.iter().step_by(step).take(bases).collect();
    std::fs::create_dir_all(out)?;
    let labels: Vec<String> = picked
        .par_iter()
        .enumerate()
        .map(|(b, p)| -> Result<Vec<String>> {
            let img = image::open(p)
                .with_context(|| p.display().to_string())?
                .to_rgb8();
            let mut rng = Rng(seed ^ (b as u64).wrapping_mul(0x2545F4914F6CDD1D));
            let (w, h) = img.dimensions();
            let (cw, ch) = (w * 96 / 100, h * 96 / 100);
            let mut lines = Vec::new();
            for j in 0..burst {
                let (dx, dy) = if j == 0 {
                    ((w - cw) / 2, (h - ch) / 2)
                } else {
                    (
                        (rng.f() * (w - cw) as f64) as u32,
                        (rng.f() * (h - ch) as f64) as u32,
                    )
                };
                let mut f = imageops::crop_imm(&img, dx, dy, cw, ch).to_image();
                if j > 0 {
                    f = imageops::colorops::brighten(&f, (rng.f() * 24.0 - 12.0) as i32);
                }
                let sigma = if j > 0 && rng.f() < blur_p {
                    sigma.0 + rng.f() * (sigma.1 - sigma.0)
                } else {
                    0.0
                };
                if sigma > 0.0 {
                    f = imageops::blur(&f, sigma as f32);
                }
                let name = format!("b{b:05}_{j}.jpg");
                let mut fh = std::io::BufWriter::new(std::fs::File::create(out.join(&name))?);
                JpegEncoder::new_with_quality(&mut fh, 90).encode_image(&f)?;
                lines.push(format!("{name},{b},{},{sigma:.2}", (sigma > 0.0) as u8));
            }
            Ok(lines)
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect();
    let mut lf = std::fs::File::create(out.join("labels.csv"))?;
    writeln!(lf, "file,burst,blurred,sigma")?;
    for l in &labels {
        writeln!(lf, "{l}")?;
    }
    Ok(labels.len())
}

/// Score grouping (cluster-level pairwise precision/recall over a threshold
/// sweep) and blur detection (global AUC vs. within-burst pick accuracy).
pub fn eval(data_dir: &Path, labels: &Path) -> Result<Value> {
    let mut lab: HashMap<String, (usize, bool)> = HashMap::new();
    for l in std::fs::read_to_string(labels)?.lines().skip(1) {
        let c: Vec<&str> = l.split(',').collect();
        lab.insert(c[0].to_string(), (c[1].parse()?, c[2] == "1"));
    }
    let c = db::open_reader(&data_dir.join("catalog.db"))?;
    let mut st = c.prepare("SELECT path,phash,sharpness FROM photos")?;
    let rows: Vec<(usize, bool, u64, f64)> = st
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)? as u64,
                r.get::<_, f64>(2)?,
            ))
        })?
        .filter_map(|r| r.ok())
        .filter_map(|(p, h, s)| {
            let name = Path::new(&p).file_name()?.to_str()?.to_string();
            lab.get(&name).map(|&(b, blur)| (b, blur, h, s))
        })
        .collect();
    let n = rows.len();
    anyhow::ensure!(n > 0, "no catalog rows match labels");

    // grouping sweep
    let mut sweep = Vec::new();
    for t in [4u32, 6, 8, 10, 12, 14, 16, 20] {
        let mut parent: Vec<usize> = (0..n).collect();
        fn find(p: &mut [usize], mut x: usize) -> usize {
            while p[x] != x {
                p[x] = p[p[x]];
                x = p[x];
            }
            x
        }
        for i in 0..n {
            for j in i + 1..n {
                if imaging::hamming(rows[i].2, rows[j].2) <= t {
                    let (a, b) = (find(&mut parent, i), find(&mut parent, j));
                    if a != b {
                        parent[a.max(b)] = a.min(b);
                    }
                }
            }
        }
        let roots: Vec<usize> = (0..n).map(|i| find(&mut parent, i)).collect();
        let (mut tp, mut fp, mut fneg) = (0u64, 0u64, 0u64);
        for i in 0..n {
            for j in i + 1..n {
                match (rows[i].0 == rows[j].0, roots[i] == roots[j]) {
                    (true, true) => tp += 1,
                    (false, true) => fp += 1,
                    (true, false) => fneg += 1,
                    _ => {}
                }
            }
        }
        let largest = {
            let mut sz: HashMap<usize, usize> = HashMap::new();
            roots.iter().for_each(|r| *sz.entry(*r).or_default() += 1);
            sz.into_values().max().unwrap_or(0)
        };
        sweep.push(json!({
            "threshold": t,
            "precision": tp as f64 / (tp + fp).max(1) as f64,
            "recall": tp as f64 / (tp + fneg).max(1) as f64,
            "largest_group": largest,
        }));
    }

    // blur: global AUC (does one sharpness threshold work across scenes?)
    let sharp: Vec<f64> = rows.iter().filter(|r| !r.1).map(|r| r.3).collect();
    let blurred: Vec<f64> = rows.iter().filter(|r| r.1).map(|r| r.3).collect();
    let mut wins = 0f64;
    for &s in &sharp {
        for &b in &blurred {
            wins += if s > b {
                1.0
            } else if s == b {
                0.5
            } else {
                0.0
            };
        }
    }
    let auc = wins / (sharp.len() * blurred.len()).max(1) as f64;

    // blur: within-burst (is the sharpest frame of each burst a sharp one?)
    let mut bursts: HashMap<usize, Vec<(bool, f64)>> = HashMap::new();
    rows.iter()
        .for_each(|r| bursts.entry(r.0).or_default().push((r.1, r.3)));
    let mixed: Vec<_> = bursts
        .values()
        .filter(|v| v.iter().any(|x| x.0) && v.iter().any(|x| !x.0))
        .collect();
    let pick_ok = mixed
        .iter()
        .filter(|v| !v.iter().max_by(|a, b| a.1.total_cmp(&b.1)).unwrap().0)
        .count();

    Ok(json!({
        "photos": n, "bursts": bursts.len(), "blurred_frames": blurred.len(),
        "grouping_sweep": sweep,
        "blur_global_auc": auc,
        "blur_within_burst": {"bursts_with_blur": mixed.len(), "sharpest_is_unblurred": pick_ok,
                               "accuracy": pick_ok as f64 / mixed.len().max(1) as f64},
    }))
}
