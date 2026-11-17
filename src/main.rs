use anyhow::{bail, Context, Result};
use cull_sidecar::{dataset, db, import, server, sys, transport};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Mutex;
use std::time::Instant;

const USAGE: &str = "usage: cull-sidecar <command> [--key value ...]
  serve        --data DIR [--transport stdio|uds|tcp] [--socket PATH] [--threads N]
               [--sync full|normal] [--unsafe-ack-before-commit]
  import       --data DIR --dir PHOTOS [--batch N] [--threads N]     (headless, prints JSON)
  gen-bursts   --src DIR --out DIR [--bases N] [--burst K] [--blur-p P] [--sigma-lo A --sigma-hi B] [--seed S]
  eval         --data DIR --labels labels.csv [--threshold T]
  seed         --data DIR --rows N                                    (synthetic catalog)
  bench-sqlite --data DIR --rows N --batch B [--sync full|normal]
  synth-photos --out DIR --n N                                        (CI fixture images)";

fn main() {
    if let Err(e) = run() {
        eprintln!("cull-sidecar: {e:#}");
        std::process::exit(1);
    }
}

fn args() -> Result<(String, HashMap<String, String>)> {
    let mut it = std::env::args().skip(1);
    let cmd = it.next().unwrap_or_default();
    let mut m = HashMap::new();
    while let Some(k) = it.next() {
        let Some(k) = k.strip_prefix("--") else {
            bail!("unexpected argument '{k}'\n{USAGE}")
        };
        if k.starts_with("unsafe") {
            m.insert(k.to_string(), "1".into());
        } else {
            m.insert(
                k.to_string(),
                it.next().with_context(|| format!("--{k} needs a value"))?,
            );
        }
    }
    Ok((cmd, m))
}

fn get<'a>(m: &'a HashMap<String, String>, k: &str) -> Result<&'a str> {
    m.get(k)
        .map(String::as_str)
        .with_context(|| format!("missing --{k}\n{USAGE}"))
}
fn num<T: std::str::FromStr>(m: &HashMap<String, String>, k: &str, d: T) -> Result<T> {
    match m.get(k) {
        Some(v) => v
            .parse()
            .map_err(|_| anyhow::anyhow!("--{k}: bad number '{v}'")),
        None => Ok(d),
    }
}
fn sync_of(m: &HashMap<String, String>) -> db::Sync {
    if m.get("sync").map(String::as_str) == Some("normal") {
        db::Sync::Normal
    } else {
        db::Sync::Full
    }
}

/// Leave one core for the UI unless told otherwise.
fn init_pool(m: &HashMap<String, String>) -> Result<()> {
    let n = match m.get("threads") {
        Some(v) => v.parse()?,
        None if std::env::var_os("RAYON_NUM_THREADS").is_some() => return Ok(()),
        None => std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2)
            .saturating_sub(1)
            .max(1),
    };
    rayon::ThreadPoolBuilder::new()
        .num_threads(n)
        .build_global()?;
    Ok(())
}

fn run() -> Result<()> {
    let (cmd, m) = args()?;
    match cmd.as_str() {
        "serve" => {
            init_pool(&m)?;
            let data = PathBuf::from(get(&m, "data")?);
            let state = server::State::open(server::Config {
                data_dir: data.clone(),
                token: std::env::var("CULL_TOKEN").ok(),
                unsafe_ack_before_commit: m.contains_key("unsafe-ack-before-commit"),
                sync: sync_of(&m),
            })?;
            match m.get("transport").map(String::as_str).unwrap_or("stdio") {
                "stdio" => transport::stdio(&state)?,
                "tcp" => transport::tcp(&state)?,
                "uds" => {
                    let sock = m
                        .get("socket")
                        .map(PathBuf::from)
                        .unwrap_or_else(|| data.join("cull.sock"));
                    transport::uds(&state, &sock)?
                }
                t => bail!("unknown transport '{t}'"),
            }
            state.close();
        }
        "import" => {
            init_pool(&m)?;
            let data = PathBuf::from(get(&m, "data")?);
            std::fs::create_dir_all(&data)?;
            let w = Mutex::new(db::open(&data.join("catalog.db"), sync_of(&m))?);
            let s = import::run(
                &w,
                &PathBuf::from(get(&m, "dir")?),
                &data.join("thumbs"),
                num(&m, "batch", 256)?,
                &AtomicBool::new(false),
                &mut |_, _| {},
            )?;
            let (_, peak) = sys::rss_kb();
            println!(
                "{}",
                serde_json::json!({
                    "stats": s, "threads": rayon::current_num_threads(),
                    "imgs_per_sec": s.imported as f64 / s.secs, "peak_rss_kb": peak,
                })
            );
        }
        "gen-bursts" => {
            let n = dataset::gen_bursts(
                &PathBuf::from(get(&m, "src")?),
                &PathBuf::from(get(&m, "out")?),
                num(&m, "bases", 1000)?,
                num(&m, "burst", 4)?,
                num(&m, "blur-p", 0.3)?,
                (num(&m, "sigma-lo", 1.5)?, num(&m, "sigma-hi", 3.0)?),
                num(&m, "seed", 42)?,
            )?;
            eprintln!("wrote {n} frames");
        }
        "eval" => {
            let data = PathBuf::from(get(&m, "data")?);
            let mut c = db::open(&data.join("catalog.db"), db::Sync::Normal)?;
            db::group(
                &mut c,
                num(&m, "threshold", cull_sidecar::DEFAULT_GROUP_THRESHOLD)?,
            )?;
            let r = dataset::eval(&data, &PathBuf::from(get(&m, "labels")?))?;
            println!("{}", serde_json::to_string_pretty(&r)?);
        }
        "synth-photos" => {
            let out = PathBuf::from(get(&m, "out")?);
            std::fs::create_dir_all(&out)?;
            for i in 0..num::<u32>(&m, "n", 20)? {
                // pairs of near-identical frames so grouping has work to do
                let k = i / 2;
                image::RgbImage::from_fn(240, 160, |x, y| {
                    let v = ((x + i % 2) * (k + 1) / 7 + y * (k + 3) / 5) % 256;
                    image::Rgb([v as u8, (v / 2 + k * 30) as u8, (255 - v) as u8])
                })
                .save(out.join(format!("s{i:04}.jpg")))?;
            }
        }
        "seed" => {
            let data = PathBuf::from(get(&m, "data")?);
            std::fs::create_dir_all(&data)?;
            let mut c = db::open(&data.join("catalog.db"), db::Sync::Normal)?;
            let rows: usize = num(&m, "rows", 100_000)?;
            let mut x: u64 = 0x1234_5678;
            for chunk in (0..rows).collect::<Vec<_>>().chunks(5000) {
                let batch: Vec<db::PhotoRow> = chunk
                    .iter()
                    .map(|&i| {
                        x ^= x << 13;
                        x ^= x >> 7;
                        x ^= x << 17;
                        synthetic_row(i, x)
                    })
                    .collect();
                db::insert_batch(&mut c, &batch)?;
            }
            eprintln!("seeded {rows} rows");
        }
        "bench-sqlite" => {
            let data = PathBuf::from(get(&m, "data")?);
            std::fs::create_dir_all(&data)?;
            let p = data.join("bench.db");
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", p.display()));
            }
            let mut c = db::open(&p, sync_of(&m))?;
            let rows: usize = num(&m, "rows", 5000)?;
            let batch: usize = num(&m, "batch", 1)?;
            let all: Vec<db::PhotoRow> = (0..rows)
                .map(|i| synthetic_row(i, i as u64 * 7919))
                .collect();
            let t0 = Instant::now();
            for ch in all.chunks(batch) {
                db::insert_batch(&mut c, ch)?;
            }
            let secs = t0.elapsed().as_secs_f64();
            println!(
                "{}",
                serde_json::json!({"rows": rows, "batch": batch, "sync": format!("{:?}", sync_of(&m)),
                                   "secs": secs, "rows_per_sec": rows as f64 / secs})
            );
        }
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    }
    Ok(())
}

fn synthetic_row(i: usize, h: u64) -> db::PhotoRow {
    db::PhotoRow {
        path: format!("/synthetic/{:03}/IMG_{i:07}.jpg", i / 1000),
        size: 8_000_000 + (h % 4_000_000) as i64,
        mtime: 1_700_000_000 + i as i64,
        width: 6000,
        height: 4000,
        sharpness: (h % 5000) as f64,
        phash: h,
        camera: Some("SYNTH-1".into()),
        taken_at: None,
        exposure: Some("1/250".into()),
        iso: Some(400),
        thumb: String::new(),
    }
}
