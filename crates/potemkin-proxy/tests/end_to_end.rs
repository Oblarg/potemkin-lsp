//! Full-flow test: run the real `potemkin` binary wrapping the real
//! `rust-analyzer`, and drive a complete LSP session (initialize -> didOpen ->
//! hover) against a hermetic temp crate. This proves the transport (spawn,
//! bidirectional `Content-Length` framing, pass-through) actually works with a
//! real language server, not just a mock.
//!
//! Skips gracefully if `rust-analyzer` is not installed.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

fn rust_analyzer_available() -> bool {
    let direct = Command::new("rust-analyzer").arg("--version").output();
    if direct.map(|o| o.status.success()).unwrap_or(false) {
        return true;
    }
    let home = std::env::var("HOME").unwrap_or_default();
    Command::new(format!("{home}/.cargo/bin/rust-analyzer"))
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// A running `potemkin` process plus a channel of parsed messages from it.
struct Session {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<Value>,
    next_id: u64,
}

impl Session {
    fn spawn(cwd: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_potemkin"))
            .current_dir(cwd)
            // Potemkin has no default backend; tell it which server to wrap.
            .env("POTEMKIN_SERVER", "rust-analyzer")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn potemkin");

        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();

        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            while let Some(json) = read_frame(&mut reader) {
                if let Ok(v) = serde_json::from_str::<Value>(&json) {
                    if tx.send(v).is_err() {
                        break;
                    }
                }
            }
        });

        Session {
            child,
            stdin,
            rx,
            next_id: 1,
        }
    }

    fn send(&mut self, msg: &Value) {
        let body = serde_json::to_string(msg).unwrap();
        write!(self.stdin, "Content-Length: {}\r\n\r\n{}", body.len(), body).unwrap();
        self.stdin.flush().unwrap();
    }

    fn request(&mut self, method: &str, params: Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        id
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    /// Pump messages until one satisfies `pred` or we time out. Auto-answers
    /// server->client requests so rust-analyzer isn't left waiting.
    fn wait_for(&mut self, timeout: Duration, mut pred: impl FnMut(&Value) -> bool) -> Option<Value> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.checked_duration_since(Instant::now())?;
            match self.rx.recv_timeout(remaining) {
                Ok(msg) => {
                    // Reply to server-initiated requests (e.g. registerCapability).
                    if msg.get("id").is_some() && msg.get("method").is_some() {
                        let id = msg.get("id").cloned().unwrap();
                        self.send(&json!({"jsonrpc": "2.0", "id": id, "result": null}));
                    }
                    if pred(&msg) {
                        return Some(msg);
                    }
                }
                Err(_) => return None,
            }
        }
    }

    fn wait_for_id(&mut self, id: u64, timeout: Duration) -> Option<Value> {
        self.wait_for(timeout, |m| m.get("id").and_then(Value::as_u64) == Some(id))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Read one `Content-Length`-framed message body (blocking).
fn read_frame(reader: &mut impl BufRead) -> Option<String> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None; // EOF
        }
        let header = line.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some(rest) = header.strip_prefix("Content-Length:") {
            content_length = Some(rest.trim().parse().ok()?);
        }
    }
    let len = content_length?;
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).ok()?;
    String::from_utf8(buf).ok()
}

fn write_fixture(dir: &Path) -> String {
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        "[workspace]\n\n[package]\nname = \"potemkin_e2e_fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[[bin]]\nname = \"fixture\"\npath = \"src/main.rs\"\n",
    )
    .unwrap();
    // `value` is declared on line 1 (0-based), starting at character 8.
    let src = "fn main() {\n    let value: i32 = 42;\n    println!(\"{}\", value);\n}\n";
    std::fs::write(dir.join("src/main.rs"), src).unwrap();
    src.to_string()
}

#[test]
fn full_flow_hover_through_proxy() {
    if !rust_analyzer_available() {
        eprintln!("skipping full_flow_hover_through_proxy: rust-analyzer not found");
        return;
    }

    // Hermetic temp crate so rust-analyzer indexing is fast and isolated.
    let dir = std::env::temp_dir().join(format!("potemkin_e2e_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let src = write_fixture(&dir);
    let root_uri = format!("file://{}", dir.display());
    let file_uri = format!("file://{}", dir.join("src/main.rs").display());

    let mut s = Session::spawn(&dir);

    // 1) initialize -> expect capabilities back (proves pass-through both ways).
    let init_id = s.request(
        "initialize",
        json!({
            "processId": null,
            "rootUri": root_uri,
            "capabilities": {},
            "workspaceFolders": null,
        }),
    );
    let init_resp = s
        .wait_for_id(init_id, Duration::from_secs(30))
        .expect("initialize response");
    assert!(
        init_resp.pointer("/result/capabilities").is_some(),
        "expected server capabilities, got: {init_resp}"
    );

    // 2) initialized + didOpen.
    s.notify("initialized", json!({}));
    s.notify(
        "textDocument/didOpen",
        json!({
            "textDocument": {
                "uri": file_uri,
                "languageId": "rust",
                "version": 1,
                "text": src,
            }
        }),
    );

    // 3) hover over `value` (line 1, char 10). Retry while rust-analyzer indexes.
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut hover_text = String::new();
    while Instant::now() < deadline {
        let id = s.request(
            "textDocument/hover",
            json!({
                "textDocument": { "uri": file_uri },
                "position": { "line": 1, "character": 10 },
            }),
        );
        if let Some(resp) = s.wait_for_id(id, Duration::from_secs(15)) {
            if let Some(result) = resp.get("result") {
                if !result.is_null() {
                    hover_text = result.to_string();
                    if hover_text.contains("i32") {
                        break;
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }

    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        hover_text.contains("i32"),
        "expected hover to report `i32` through the proxy; last hover: {hover_text:?}"
    );
}
