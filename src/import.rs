//! Parallel import: walk -> (rayon) decode/thumbnail/score -> bounded channel
//! -> single DB writer committing in batches. Memory stays flat because only
//! one decoded image per worker exists at a time and the channel is bounded.

use crate::db::{self, PhotoRow};
use crate::imaging;
use anyhow::Result;
use rayon::prelude::*;
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Mutex};
use std::time::{Instant, UNIX_EPOCH};

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct ImportStats {
    pub found: usize,
    pub skipped_unchanged: usize,
    pub imported: usize,
    pub failed: usize,
    pub cancelled: bool,
    pub secs: f64,
    pub batches: usize,
}

pub fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if let Some(ext) = p.extension().and_then(|e| e.to_str()) {
                if matches!(ext.to_ascii_lowercase().as_str(), "jpg" | "jpeg" | "png") {
                    out.push(p);
                }
            }
        }
    }
    out.sort();
    out
}

fn meta(p: &Path) -> Option<(u64, i64)> {
    let m = std::fs::metadata(p).ok()?;
    let mtime = m
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    Some((m.len(), mtime))
}

pub fn run(
    writer: &Mutex<Connection>,
    dir: &Path,
    thumbs_dir: &Path,
    batch: usize,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(usize, usize),
) -> Result<ImportStats> {
    let t0 = Instant::now();
    let files = walk(dir);
    let known = db::known_files(&writer.lock().unwrap())?;
    let mut stats = ImportStats {
        found: files.len(),
        ..Default::default()
    };

    let todo: Vec<(PathBuf, u64, i64)> = files
        .into_iter()
        .filter_map(|p| {
            let (size, mtime) = meta(&p)?;
            let key = p.to_string_lossy();
            match known.get(key.as_ref()) {
                Some(&(s, m)) if s == size as i64 && m == mtime => None,
                _ => Some((p, size, mtime)),
            }
        })
        .collect();
    drop(known);
    stats.skipped_unchanged = stats.found - todo.len();
    let total = todo.len();

    let (tx, rx) = mpsc::sync_channel::<Option<PhotoRow>>(256);
    std::thread::scope(|s| -> Result<()> {
        let todo = &todo;
        s.spawn(move || {
            todo.par_iter().for_each_with(tx, |tx, (p, size, mtime)| {
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                let row = imaging::analyze(p, *size, *mtime, thumbs_dir)
                    .ok()
                    .map(|a| PhotoRow {
                        path: p.to_string_lossy().into_owned(),
                        size: *size as i64,
                        mtime: *mtime,
                        width: a.width,
                        height: a.height,
                        sharpness: a.sharpness,
                        phash: a.phash,
                        camera: a.camera,
                        taken_at: a.taken_at,
                        exposure: a.exposure,
                        iso: a.iso,
                        thumb: a.thumb.to_string_lossy().into_owned(),
                    });
                let _ = tx.send(row);
            });
        });

        let mut buf = Vec::with_capacity(batch);
        let mut done = 0;
        let flush = |buf: &mut Vec<PhotoRow>, stats: &mut ImportStats| -> Result<()> {
            if !buf.is_empty() {
                db::insert_batch(&mut writer.lock().unwrap(), buf)?;
                stats.imported += buf.len();
                stats.batches += 1;
                buf.clear();
            }
            Ok(())
        };
        for r in rx {
            done += 1;
            match r {
                Some(row) => buf.push(row),
                None => stats.failed += 1,
            }
            if buf.len() >= batch {
                flush(&mut buf, &mut stats)?;
            }
            progress(done, total);
        }
        flush(&mut buf, &mut stats)?;
        Ok(())
    })?;
    stats.cancelled = cancel.load(Ordering::Relaxed);
    stats.secs = t0.elapsed().as_secs_f64();
    Ok(stats)
}
