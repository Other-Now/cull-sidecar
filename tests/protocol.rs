//! End-to-end over the real binary on the stdio transport.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};

struct Client {
    child: Child,
    stdin: ChildStdin,
    out: BufReader<std::process::ChildStdout>,
    next: u64,
    events: Vec<Value>,
}

impl Client {
    fn start(data: &std::path::Path) -> Client {
        let mut child = Command::new(env!("CARGO_BIN_EXE_cull-sidecar"))
            .args(["serve", "--data", data.to_str().unwrap(), "--threads", "2"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let out = BufReader::new(child.stdout.take().unwrap());
        Client {
            child,
            stdin,
            out,
            next: 1,
            events: vec![],
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next += 1;
        writeln!(
            self.stdin,
            "{}",
            json!({"id": id, "method": method, "params": params})
        )
        .unwrap();
        self.stdin.flush().unwrap();
        loop {
            let m = self.read();
            if m.get("event").is_some() {
                self.events.push(m);
            } else {
                assert_eq!(m["id"], id);
                return m;
            }
        }
    }

    fn read(&mut self) -> Value {
        let mut l = String::new();
        assert!(
            self.out.read_line(&mut l).unwrap() > 0,
            "sidecar closed stdout"
        );
        serde_json::from_str(&l).unwrap()
    }

    fn wait_event(&mut self, name: &str) -> Value {
        if let Some(i) = self.events.iter().position(|e| e["event"] == name) {
            return self.events.remove(i);
        }
        loop {
            let m = self.read();
            if m["event"] == name {
                return m;
            }
            self.events.push(m);
        }
    }
}

fn write_photos(dir: &std::path::Path, n: u32) {
    for i in 0..n {
        let img = image::RgbImage::from_fn(200, 150, |x, y| {
            image::Rgb([
                ((x * (i + 1)) % 256) as u8,
                ((y * 3 + i * 40) % 256) as u8,
                ((x + y) % 256) as u8,
            ])
        });
        img.save(dir.join(format!("p{i:03}.jpg"))).unwrap();
    }
}

#[test]
fn handshake_import_edit_thumb_shutdown() {
    let data = tempfile::tempdir().unwrap();
    let photos = tempfile::tempdir().unwrap();
    write_photos(photos.path(), 12);
    let mut c = Client::start(data.path());

    // must greet first, with a matching version
    assert_eq!(c.call("ping", json!({}))["error"]["code"], "hello_required");
    assert_eq!(
        c.call("hello", json!({"protocol": 99}))["error"]["code"],
        "version_mismatch"
    );
    let h = c.call("hello", json!({"protocol": 1}));
    assert_eq!(h["result"]["protocol"], 1);
    assert_eq!(
        c.call("ping", json!({"payload": "x"}))["result"]["payload"],
        "x"
    );
    assert_eq!(c.call("nope", json!({}))["error"]["code"], "unknown_method");

    // import streams progress then a done event
    let job = c.call("import", json!({"dir": photos.path()}))["result"]["job"].clone();
    let done = c.wait_event("import_done");
    assert_eq!(done["job"], job);
    assert_eq!(done["stats"]["imported"], 12);
    assert!(c.events.iter().any(|e| e["event"] == "progress"));

    // re-import of unchanged files is skipped
    c.call("import", json!({"dir": photos.path()}));
    assert_eq!(
        c.wait_event("import_done")["stats"]["skipped_unchanged"],
        12
    );

    let list = c.call("list", json!({"limit": 5}));
    assert_eq!(list["result"]["total"], 12);
    let id = list["result"]["photos"][0]["id"].as_i64().unwrap();
    assert_eq!(
        c.call("set_rating", json!({"id": id, "rating": 5}))["result"]["updated"],
        true
    );
    assert_eq!(
        c.call("set_rating", json!({"id": id, "rating": 9}))["error"]["code"],
        "bad_params"
    );
    assert_eq!(
        c.call("set_decision", json!({"id": id, "decision": "keep"}))["result"]["updated"],
        true
    );
    assert_eq!(
        c.call("set_rating", json!({"id": 999999, "rating": 1}))["error"]["code"],
        "not_found"
    );

    // thumbnail bytes are identical over base64 and the shared region
    let path = c.call("thumb", json!({"id": id}))["result"]["path"]
        .as_str()
        .unwrap()
        .to_string();
    let on_disk = std::fs::read(&path).unwrap();
    let b64 = c.call("thumb", json!({"id": id, "via": "base64"}));
    use base64::Engine;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(b64["result"]["data"].as_str().unwrap())
        .unwrap();
    assert_eq!(decoded, on_disk);
    let shm = c.call("thumb", json!({"id": id, "via": "shm"}))["result"].clone();
    let region = std::fs::read(h["result"]["shm"].as_str().unwrap()).unwrap();
    let off = shm["offset"].as_u64().unwrap() as usize;
    assert_eq!(&region[off..off + on_disk.len()], &on_disk[..]);

    assert!(
        c.call("group", json!({}))["result"]["photos"]
            .as_u64()
            .unwrap()
            == 12
    );

    assert_eq!(c.call("shutdown", json!({}))["result"]["bye"], true);
    let status = c.child.wait().unwrap();
    assert!(status.success());

    // edits survived the restart
    let mut c = Client::start(data.path());
    c.call("hello", json!({"protocol": 1}));
    let p = &c.call("list", json!({"limit": 1}))["result"]["photos"][0];
    assert_eq!(
        (p["rating"].as_i64(), p["decision"].as_i64()),
        (Some(5), Some(1))
    );
    drop(c.stdin); // parent "dies": EOF must make the sidecar exit by itself
    assert!(c.child.wait().unwrap().success());
}
