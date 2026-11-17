//! JSON-lines protocol, one handler shared by every transport.
//!
//! Request : {"id":1,"method":"ping","params":{...}}
//! Reply   : {"id":1,"result":{...}}  or  {"id":1,"error":{"code":"...","message":"..."}}
//! Event   : {"event":"progress","job":3,"done":120,"total":4000}   (no id)
//!
//! The first call on a connection must be `hello` with a matching protocol
//! version. stdout carries protocol only; logs go to stderr.

use crate::{db, import, shm::Shm, sys};
use base64::Engine;
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufWriter, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

pub const PROTOCOL_VERSION: u64 = 1;

#[derive(Deserialize)]
struct Request {
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

pub struct ApiError {
    code: &'static str,
    message: String,
}
macro_rules! internal_from {
    ($($t:ty),*) => {$(
        impl From<$t> for ApiError {
            fn from(e: $t) -> Self {
                ApiError { code: "internal", message: format!("{:#}", anyhow::Error::from(e)) }
            }
        }
    )*};
}
internal_from!(anyhow::Error, rusqlite::Error, std::io::Error);
fn err(code: &'static str, message: impl Into<String>) -> ApiError {
    ApiError {
        code,
        message: message.into(),
    }
}
type ApiResult = Result<Value, ApiError>;

#[derive(Clone, Copy)]
enum Edit {
    Rating(i64),
    Decision(i64),
}

pub struct State {
    pub writer: Mutex<Connection>,
    pub reader: Mutex<Connection>,
    pub thumbs_dir: PathBuf,
    pub shm: Mutex<Shm>,
    pub token: Option<String>,
    pub shutdown: AtomicBool,
    jobs: Mutex<HashMap<u64, Arc<AtomicBool>>>,
    next_job: AtomicU64,
    /// Deliberately unsafe control for the crash test: ack edits before they
    /// are committed and flush them on a timer.
    unsafe_pending: Option<Mutex<Vec<(i64, Edit)>>>,
    blob: Mutex<Vec<u8>>,
}

pub struct Config {
    pub data_dir: PathBuf,
    pub token: Option<String>,
    pub unsafe_ack_before_commit: bool,
    pub sync: db::Sync,
}

impl State {
    pub fn open(cfg: Config) -> anyhow::Result<Arc<State>> {
        std::fs::create_dir_all(&cfg.data_dir)?;
        let db_path = cfg.data_dir.join("catalog.db");
        let writer = db::open(&db_path, cfg.sync)?;
        let reader = db::open_reader(&db_path)?;
        let shm = Shm::create(crate::shm::default_path(&cfg.data_dir), 64 << 20)?;
        let st = Arc::new(State {
            writer: Mutex::new(writer),
            reader: Mutex::new(reader),
            thumbs_dir: cfg.data_dir.join("thumbs"),
            shm: Mutex::new(shm),
            token: cfg.token,
            shutdown: AtomicBool::new(false),
            jobs: Mutex::new(HashMap::new()),
            next_job: AtomicU64::new(1),
            unsafe_pending: cfg.unsafe_ack_before_commit.then(|| Mutex::new(Vec::new())),
            blob: Mutex::new(Vec::new()),
        });
        if st.unsafe_pending.is_some() {
            let s = Arc::clone(&st);
            std::thread::spawn(move || loop {
                std::thread::sleep(Duration::from_millis(250));
                let _ = s.flush_unsafe();
            });
        }
        Ok(st)
    }

    fn flush_unsafe(&self) -> anyhow::Result<()> {
        let Some(p) = &self.unsafe_pending else {
            return Ok(());
        };
        let edits = std::mem::take(&mut *p.lock().unwrap());
        if edits.is_empty() {
            return Ok(());
        }
        let mut w = self.writer.lock().unwrap();
        let tx = w.transaction()?;
        for (id, e) in edits {
            match e {
                Edit::Rating(v) => db::set_rating(&tx, id, v)?,
                Edit::Decision(v) => db::set_decision(&tx, id, v)?,
            };
        }
        tx.commit()?;
        Ok(())
    }

    pub fn cancel_all(&self) {
        for f in self.jobs.lock().unwrap().values() {
            f.store(true, Ordering::Relaxed);
        }
    }

    /// Clean shutdown: wait for jobs to stop, flush, checkpoint the WAL.
    pub fn close(&self) {
        self.cancel_all();
        let t0 = Instant::now();
        while !self.jobs.lock().unwrap().is_empty() && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.flush_unsafe();
        let _ = self
            .writer
            .lock()
            .unwrap()
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    }
}

/// Serve one client connection until EOF or `shutdown`. Returns true if the
/// client asked the process to shut down.
pub fn serve_conn<R: BufRead, W: Write + Send + 'static>(
    state: &Arc<State>,
    input: R,
    output: W,
) -> bool {
    let (tx, rx) = mpsc::channel::<String>();
    // One writer thread owns the output stream so replies and events from
    // import threads never interleave mid-line. Flushes once per burst.
    let writer = std::thread::spawn(move || {
        let mut w = BufWriter::new(output);
        while let Ok(line) = rx.recv() {
            if writeln!(w, "{line}").is_err() {
                return;
            }
            while let Ok(more) = rx.try_recv() {
                if writeln!(w, "{more}").is_err() {
                    return;
                }
            }
            if w.flush().is_err() {
                return;
            }
        }
    });

    let mut greeted = false;
    for line in input.lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let msg = match serde_json::from_str::<Request>(&line) {
            Err(e) => {
                json!({"id": null, "error": {"code": "bad_request", "message": e.to_string()}})
            }
            Ok(req) => match handle(state, &tx, &mut greeted, &req) {
                Ok(v) => json!({"id": req.id, "result": v}),
                Err(e) => json!({"id": req.id, "error": {"code": e.code, "message": e.message}}),
            },
        };
        let _ = tx.send(msg.to_string());
        if state.shutdown.load(Ordering::SeqCst) {
            break;
        }
    }
    // Client gone (or shutdown): don't keep burning CPU on its imports.
    state.cancel_all();
    drop(tx);
    let _ = writer.join();
    state.shutdown.load(Ordering::SeqCst)
}

fn p_i64(p: &Value, k: &str) -> Result<i64, ApiError> {
    p.get(k)
        .and_then(Value::as_i64)
        .ok_or_else(|| err("bad_params", format!("missing integer '{k}'")))
}

fn handle(
    st: &Arc<State>,
    out: &mpsc::Sender<String>,
    greeted: &mut bool,
    req: &Request,
) -> ApiResult {
    let p = &req.params;
    if req.method == "hello" {
        let v = p_i64(p, "protocol")? as u64;
        if v != PROTOCOL_VERSION {
            return Err(err(
                "version_mismatch",
                format!("client speaks protocol {v}, server speaks {PROTOCOL_VERSION}"),
            ));
        }
        if let Some(t) = &st.token {
            if p.get("token").and_then(Value::as_str) != Some(t.as_str()) {
                return Err(err("unauthorized", "bad token"));
            }
        }
        *greeted = true;
        return Ok(json!({
            "protocol": PROTOCOL_VERSION,
            "server": concat!("cull-sidecar/", env!("CARGO_PKG_VERSION")),
            "schema": db::SCHEMA_VERSION,
            "pid": std::process::id(),
            "photos": db::count(&st.reader.lock().unwrap())?,
            "shm": st.shm.lock().unwrap().path.to_string_lossy(),
            "threads": rayon::current_num_threads(),
        }));
    }
    if !*greeted {
        return Err(err("hello_required", "call hello first"));
    }
    match req.method.as_str() {
        "ping" => Ok(json!({ "payload": p.get("payload").cloned().unwrap_or(Value::Null) })),

        "import" => {
            let dir = PathBuf::from(
                p.get("dir")
                    .and_then(Value::as_str)
                    .ok_or_else(|| err("bad_params", "missing 'dir'"))?,
            );
            if !dir.is_dir() {
                return Err(err(
                    "not_found",
                    format!("{} is not a directory", dir.display()),
                ));
            }
            let batch = p.get("batch").and_then(Value::as_u64).unwrap_or(256).max(1) as usize;
            let job = st.next_job.fetch_add(1, Ordering::Relaxed);
            let cancel = Arc::new(AtomicBool::new(false));
            st.jobs.lock().unwrap().insert(job, Arc::clone(&cancel));
            let (st2, out2) = (Arc::clone(st), out.clone());
            std::thread::spawn(move || {
                let mut last = Instant::now() - Duration::from_secs(1);
                let mut progress = |done: usize, total: usize| {
                    if done == total || last.elapsed() >= Duration::from_millis(100) {
                        last = Instant::now();
                        let _ = out2.send(
                            json!({"event":"progress","job":job,"done":done,"total":total})
                                .to_string(),
                        );
                    }
                };
                let r = import::run(
                    &st2.writer,
                    &dir,
                    &st2.thumbs_dir,
                    batch,
                    &cancel,
                    &mut progress,
                );
                let ev = match r {
                    Ok(s) => json!({"event":"import_done","job":job,"stats":s}),
                    Err(e) => json!({"event":"import_failed","job":job,"message":format!("{e:#}")}),
                };
                st2.jobs.lock().unwrap().remove(&job);
                let _ = out2.send(ev.to_string());
            });
            Ok(json!({ "job": job }))
        }

        "cancel" => {
            let job = p_i64(p, "job")? as u64;
            let f = st.jobs.lock().unwrap().get(&job).cloned();
            if let Some(f) = &f {
                f.store(true, Ordering::Relaxed);
            }
            Ok(json!({ "cancelled": f.is_some() }))
        }

        "list" => {
            let offset = p.get("offset").and_then(Value::as_i64).unwrap_or(0);
            let limit = p
                .get("limit")
                .and_then(Value::as_i64)
                .unwrap_or(100)
                .clamp(1, 100_000);
            let picks = p
                .get("picks_only")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            Ok(db::list(&st.reader.lock().unwrap(), offset, limit, picks)?)
        }

        "set_rating" | "set_decision" => {
            let id = p_i64(p, "id")?;
            let edit = if req.method == "set_rating" {
                let r = p_i64(p, "rating")?;
                if !(0..=5).contains(&r) {
                    return Err(err("bad_params", "rating must be 0..=5"));
                }
                Edit::Rating(r)
            } else {
                let d = match p.get("decision").and_then(Value::as_str) {
                    Some("none") => 0,
                    Some("keep") => 1,
                    Some("reject") => 2,
                    _ => return Err(err("bad_params", "decision must be none|keep|reject")),
                };
                Edit::Decision(d)
            };
            if let Some(pending) = &st.unsafe_pending {
                pending.lock().unwrap().push((id, edit));
                return Ok(json!({ "updated": true }));
            }
            // Reply only after COMMIT returns: an acknowledged edit is durable.
            let w = st.writer.lock().unwrap();
            let updated = match edit {
                Edit::Rating(v) => db::set_rating(&w, id, v)?,
                Edit::Decision(v) => db::set_decision(&w, id, v)?,
            };
            if !updated {
                return Err(err("not_found", format!("no photo {id}")));
            }
            Ok(json!({ "updated": true }))
        }

        "group" => {
            let t = p
                .get("threshold")
                .and_then(Value::as_u64)
                .unwrap_or(crate::DEFAULT_GROUP_THRESHOLD as u64) as u32;
            Ok(db::group(&mut st.writer.lock().unwrap(), t)?)
        }

        "thumb" => {
            let id = p_i64(p, "id")?;
            let path = db::thumb_of(&st.reader.lock().unwrap(), id)?
                .ok_or_else(|| err("not_found", format!("no thumb for {id}")))?;
            match p.get("via").and_then(Value::as_str).unwrap_or("path") {
                "path" => Ok(json!({ "path": path })),
                via => send_bytes(st, &std::fs::read(&path)?, via),
            }
        }

        // Transport benchmark: a buffer of `len` bytes, delivered `via`.
        "bench_blob" => {
            let len = p_i64(p, "len")? as usize;
            let mut b = st.blob.lock().unwrap();
            if b.len() != len {
                *b = (0..len).map(|i| (i * 31 + 7) as u8).collect();
            }
            send_bytes(
                st,
                &b,
                p.get("via").and_then(Value::as_str).unwrap_or("base64"),
            )
        }

        "stats" => {
            let (rss, peak) = sys::rss_kb();
            Ok(json!({
                "rss_kb": rss, "peak_rss_kb": peak,
                "photos": db::count(&st.reader.lock().unwrap())?,
                "jobs": st.jobs.lock().unwrap().len(),
                "threads": rayon::current_num_threads(),
            }))
        }

        "shutdown" => {
            st.shutdown.store(true, Ordering::SeqCst);
            st.cancel_all();
            Ok(json!({ "bye": true }))
        }

        m => Err(err("unknown_method", format!("unknown method '{m}'"))),
    }
}

fn send_bytes(st: &State, data: &[u8], via: &str) -> ApiResult {
    match via {
        "base64" => Ok(
            json!({ "len": data.len(), "data": base64::engine::general_purpose::STANDARD.encode(data) }),
        ),
        "shm" => {
            let (offset, seq) = st.shm.lock().unwrap().put(data)?;
            Ok(json!({ "len": data.len(), "offset": offset, "seq": seq }))
        }
        v => Err(err(
            "bad_params",
            format!("via must be base64|shm|path, got '{v}'"),
        )),
    }
}
