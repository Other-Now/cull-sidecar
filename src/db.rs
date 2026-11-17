//! SQLite catalog (WAL). One writer connection, one read-only connection:
//! WAL lets the UI's reads proceed while an import transaction is open.

use anyhow::{bail, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

/// On-disk schema version (PRAGMA user_version). Separate from the wire
/// protocol version: an old app must refuse a catalog written by a newer one.
pub const SCHEMA_VERSION: i64 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sync {
    Full,
    Normal,
}

pub fn open(path: &Path, sync: Sync) -> Result<Connection> {
    let c = Connection::open(path)?;
    let mode: String = c.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
    if mode != "wal" {
        bail!("could not enable WAL (got {mode})");
    }
    c.pragma_update(
        None,
        "synchronous",
        if sync == Sync::Full { "FULL" } else { "NORMAL" },
    )?;
    c.busy_timeout(Duration::from_secs(10))?;
    migrate(&c)?;
    Ok(c)
}

pub fn open_reader(path: &Path) -> Result<Connection> {
    let c = Connection::open(path)?;
    c.busy_timeout(Duration::from_secs(10))?;
    c.pragma_update(None, "query_only", true)?;
    Ok(c)
}

fn migrate(c: &Connection) -> Result<()> {
    let v: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if v > SCHEMA_VERSION {
        bail!("catalog schema v{v} is newer than this build (v{SCHEMA_VERSION})");
    }
    if v < 1 {
        c.execute_batch(
            "BEGIN;
             CREATE TABLE IF NOT EXISTS photos(
               id        INTEGER PRIMARY KEY,
               path      TEXT NOT NULL UNIQUE,
               size      INTEGER NOT NULL,
               mtime     INTEGER NOT NULL,
               width     INTEGER, height INTEGER,
               sharpness REAL, phash INTEGER,
               camera TEXT, taken_at TEXT, exposure TEXT, iso INTEGER,
               thumb     TEXT,
               rating    INTEGER NOT NULL DEFAULT 0,
               decision  INTEGER NOT NULL DEFAULT 0,   -- 0 none, 1 keep, 2 reject
               group_id  INTEGER,
               is_pick   INTEGER NOT NULL DEFAULT 0
             );
             CREATE INDEX IF NOT EXISTS idx_photos_group ON photos(group_id);
             PRAGMA user_version = 1;
             COMMIT;",
        )?;
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct PhotoRow {
    pub path: String,
    pub size: i64,
    pub mtime: i64,
    pub width: u32,
    pub height: u32,
    pub sharpness: f64,
    pub phash: u64,
    pub camera: Option<String>,
    pub taken_at: Option<String>,
    pub exposure: Option<String>,
    pub iso: Option<i64>,
    pub thumb: String,
}

/// Upsert analysis results in ONE transaction. Re-importing a file refreshes
/// its analysis columns but never touches rating/decision (user edits).
pub fn insert_batch(c: &mut Connection, rows: &[PhotoRow]) -> Result<()> {
    let tx = c.transaction()?;
    {
        let mut st = tx.prepare_cached(
            "INSERT INTO photos(path,size,mtime,width,height,sharpness,phash,camera,taken_at,exposure,iso,thumb)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)
             ON CONFLICT(path) DO UPDATE SET size=excluded.size, mtime=excluded.mtime,
               width=excluded.width, height=excluded.height, sharpness=excluded.sharpness,
               phash=excluded.phash, camera=excluded.camera, taken_at=excluded.taken_at,
               exposure=excluded.exposure, iso=excluded.iso, thumb=excluded.thumb",
        )?;
        for r in rows {
            st.execute(params![
                r.path,
                r.size,
                r.mtime,
                r.width,
                r.height,
                r.sharpness,
                r.phash as i64,
                r.camera,
                r.taken_at,
                r.exposure,
                r.iso,
                r.thumb
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// path -> (size, mtime) for incremental re-import.
pub fn known_files(c: &Connection) -> Result<HashMap<String, (i64, i64)>> {
    let mut st = c.prepare("SELECT path,size,mtime FROM photos")?;
    let rows = st.query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn count(c: &Connection) -> Result<i64> {
    Ok(c.query_row("SELECT count(*) FROM photos", [], |r| r.get(0))?)
}

pub fn set_rating(c: &Connection, id: i64, rating: i64) -> Result<bool> {
    Ok(c.execute(
        "UPDATE photos SET rating=?2 WHERE id=?1",
        params![id, rating],
    )? == 1)
}

pub fn set_decision(c: &Connection, id: i64, decision: i64) -> Result<bool> {
    Ok(c.execute(
        "UPDATE photos SET decision=?2 WHERE id=?1",
        params![id, decision],
    )? == 1)
}

pub fn thumb_of(c: &Connection, id: i64) -> Result<Option<String>> {
    Ok(
        c.query_row("SELECT thumb FROM photos WHERE id=?1", [id], |r| r.get(0))
            .optional()?
            .flatten(),
    )
}

pub fn list(c: &Connection, offset: i64, limit: i64, only_picks: bool) -> Result<Value> {
    let filter = if only_picks {
        "WHERE group_id IS NULL OR is_pick=1"
    } else {
        ""
    };
    let total: i64 = c.query_row(&format!("SELECT count(*) FROM photos {filter}"), [], |r| {
        r.get(0)
    })?;
    let mut st = c.prepare_cached(&format!(
        "SELECT id,path,thumb,rating,decision,sharpness,group_id,is_pick,width,height
         FROM photos {filter} ORDER BY id LIMIT ?1 OFFSET ?2"
    ))?;
    let photos: Vec<Value> = st
        .query_map(params![limit, offset], |r| {
            Ok(json!({
                "id": r.get::<_, i64>(0)?, "path": r.get::<_, String>(1)?,
                "thumb": r.get::<_, Option<String>>(2)?, "rating": r.get::<_, i64>(3)?,
                "decision": r.get::<_, i64>(4)?, "sharpness": r.get::<_, Option<f64>>(5)?,
                "group": r.get::<_, Option<i64>>(6)?, "pick": r.get::<_, i64>(7)? == 1,
                "w": r.get::<_, Option<i64>>(8)?, "h": r.get::<_, Option<i64>>(9)?,
            }))
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(json!({ "total": total, "photos": photos }))
}

/// Group near-duplicates: union-find over all pairs with pHash Hamming
/// distance <= threshold, then mark the sharpest frame of each group as pick.
pub fn group(c: &mut Connection, threshold: u32) -> Result<Value> {
    let rows: Vec<(i64, u64, f64)> = {
        let mut st = c.prepare(
            "SELECT id,phash,coalesce(sharpness,0) FROM photos WHERE phash IS NOT NULL ORDER BY id",
        )?;
        let it = st.query_map([], |r| {
            Ok((r.get(0)?, r.get::<_, i64>(1)? as u64, r.get(2)?))
        })?;
        it.collect::<rusqlite::Result<_>>()?
    };
    let n = rows.len();
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
            if crate::imaging::hamming(rows[i].1, rows[j].1) <= threshold {
                let (a, b) = (find(&mut parent, i), find(&mut parent, j));
                if a != b {
                    parent[a.max(b)] = a.min(b);
                }
            }
        }
    }
    let mut members: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..n {
        let r = find(&mut parent, i);
        members.entry(r).or_default().push(i);
    }
    let tx = c.transaction()?;
    tx.execute("UPDATE photos SET group_id=NULL, is_pick=0", [])?;
    let (mut groups, mut grouped) = (0, 0);
    {
        let mut st = tx.prepare("UPDATE photos SET group_id=?2, is_pick=?3 WHERE id=?1")?;
        for m in members.values().filter(|m| m.len() > 1) {
            groups += 1;
            grouped += m.len();
            let gid = rows[m[0]].0;
            let best = *m
                .iter()
                .max_by(|&&a, &&b| rows[a].2.total_cmp(&rows[b].2))
                .unwrap();
            for &i in m {
                st.execute(params![rows[i].0, gid, (i == best) as i64])?;
            }
        }
    }
    tx.commit()?;
    Ok(json!({ "photos": n, "groups": groups, "grouped_photos": grouped, "threshold": threshold }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(p: &str) -> PhotoRow {
        PhotoRow {
            path: p.into(),
            size: 1,
            mtime: 1,
            width: 10,
            height: 10,
            sharpness: 1.0,
            phash: 7,
            camera: None,
            taken_at: None,
            exposure: None,
            iso: None,
            thumb: "t".into(),
        }
    }

    #[test]
    fn reimport_keeps_user_edits() {
        let d = tempfile::tempdir().unwrap();
        let mut c = open(&d.path().join("c.db"), Sync::Full).unwrap();
        insert_batch(&mut c, &[row("a.jpg")]).unwrap();
        assert!(set_rating(&c, 1, 4).unwrap());
        let mut r = row("a.jpg");
        r.sharpness = 99.0;
        insert_batch(&mut c, &[r]).unwrap();
        let (rating, sharp): (i64, f64) = c
            .query_row("SELECT rating,sharpness FROM photos", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!((rating, sharp), (4, 99.0));
    }

    #[test]
    fn newer_schema_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("c.db");
        drop(open(&p, Sync::Full).unwrap());
        Connection::open(&p)
            .unwrap()
            .pragma_update(None, "user_version", 2)
            .unwrap();
        assert!(open(&p, Sync::Full).is_err());
    }

    #[test]
    fn group_marks_sharpest_pick() {
        let d = tempfile::tempdir().unwrap();
        let mut c = open(&d.path().join("c.db"), Sync::Normal).unwrap();
        let mut a = row("a");
        a.phash = 0b1111;
        a.sharpness = 5.0;
        let mut b = row("b");
        b.phash = 0b1110;
        b.sharpness = 9.0;
        let mut z = row("z");
        z.phash = u64::MAX;
        z.sharpness = 1.0;
        insert_batch(&mut c, &[a, b, z]).unwrap();
        let g = group(&mut c, 4).unwrap();
        assert_eq!(g["groups"], 1);
        let pick: String = c
            .query_row("SELECT path FROM photos WHERE is_pick=1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(pick, "b");
    }
}
