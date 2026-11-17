//! Shared transfer region for large buffers: a fixed-size file mapped into
//! this process. On Linux it lives in /dev/shm (tmpfs), so the client reads it
//! straight from page cache instead of receiving base64 inside JSON.
//!
//! Allocation is a bump pointer that wraps: a buffer stays valid until ~`cap`
//! more bytes have been handed out. The client reads it as soon as the reply
//! arrives, so with one reader this is safe; `seq` lets a client detect reuse.

use anyhow::{bail, Result};
use memmap2::MmapMut;
use std::fs::OpenOptions;
use std::path::PathBuf;

pub struct Shm {
    pub path: PathBuf,
    map: Option<MmapMut>,
    head: usize,
    pub seq: u64,
}

impl Shm {
    pub fn create(path: PathBuf, cap: usize) -> Result<Self> {
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;
        f.set_len(cap as u64)?;
        // SAFETY: the file is created and owned by this process; no other
        // process truncates it while mapped (the client opens it read-only).
        let map = unsafe { MmapMut::map_mut(&f)? };
        Ok(Self {
            path,
            map: Some(map),
            head: 0,
            seq: 0,
        })
    }

    /// Copy `data` into the region; returns (offset, seq).
    pub fn put(&mut self, data: &[u8]) -> Result<(usize, u64)> {
        let map = self.map.as_mut().expect("mapped until drop");
        let cap = map.len();
        if data.len() > cap {
            bail!(
                "buffer of {} bytes exceeds transfer region ({cap})",
                data.len()
            );
        }
        if self.head + data.len() > cap {
            self.head = 0;
        }
        let off = self.head;
        map[off..off + data.len()].copy_from_slice(data);
        self.head = (off + data.len() + 63) & !63;
        self.seq += 1;
        Ok((off, self.seq))
    }
}

impl Drop for Shm {
    fn drop(&mut self) {
        // unmap first: Windows refuses to delete a file that is still mapped
        drop(self.map.take());
        let _ = std::fs::remove_file(&self.path);
    }
}

pub fn default_path(data_dir: &std::path::Path) -> PathBuf {
    let name = format!("cull-sidecar-{}.xfer", std::process::id());
    let dev_shm = std::path::Path::new("/dev/shm");
    if cfg!(target_os = "linux") && dev_shm.is_dir() {
        dev_shm.join(name)
    } else {
        data_dir.join(name)
    }
}
