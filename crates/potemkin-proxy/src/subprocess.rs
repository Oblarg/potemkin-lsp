//! Subprocess transport.
//!
//! Wraps a child process that speaks the newline-delimited-JSON plugin protocol
//! (see `potemkin_plugin_api::protocol`) and exposes it as an [`RpcTransport`].
//! This is the language-agnostic path: a plugin can be written in any language
//! that can read/write JSON on stdio. The JavaScript transport reuses this by
//! resolving a Node runtime as the command (see `crate::plugin`).
//!
//! ## Timeout / fault isolation
//!
//! Every `call` is bounded by a timeout. A blocking read on a pipe has no
//! timeout of its own, so a dedicated reader thread forwards response lines over
//! a channel and the request side waits with [`Receiver::recv_timeout`]. If a
//! plugin hangs (or is pathologically slow), the call kills the child, marks the
//! transport **dead**, and returns an error; the pipeline then passes the text
//! through untouched. Every later call short-circuits, so one bad plugin drops
//! out of the chain instead of stalling the editor's hover/inlay stream.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde_json::Value;

use potemkin_plugin_api::protocol::{Request, Response};

use crate::plugin::RpcTransport;

/// A transport backed by a long-lived child process speaking JSONL over stdio.
pub struct SubprocessRpc {
    /// Human label (the command) for diagnostics.
    label: String,
    io: Mutex<Io>,
    next_id: AtomicU64,
    /// Per-call timeout for a single request/response round trip.
    timeout: Duration,
    /// Set once the child has been killed (timeout / I/O error / protocol
    /// desync). Checked before every call so a broken plugin fails fast.
    dead: AtomicBool,
}

struct Io {
    /// Kept so we can `kill()` the child on timeout and on drop.
    child: Child,
    stdin: ChildStdin,
    /// Response lines produced by the reader thread. The plugin protocol is
    /// strictly request→response with no unsolicited messages, so each received
    /// line corresponds to the request we just wrote.
    lines: Receiver<String>,
}

impl SubprocessRpc {
    /// Spawn `command` with `args`, applying any `extra_env` overrides (used by
    /// the JS transport to pass `ELECTRON_RUN_AS_NODE=1`). `timeout` bounds each
    /// subsequent `call`.
    pub fn spawn(
        command: &str,
        args: &[String],
        extra_env: &[(&str, &str)],
        timeout: Duration,
    ) -> Result<Self> {
        let mut cmd = Command::new(command);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        for (k, v) in extra_env {
            cmd.env(k, v);
        }

        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn plugin process ({command})"))?;

        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");

        // A dedicated reader thread forwards each response line over a channel so
        // the request side can wait with a timeout. When the child is killed (or
        // exits), `read_line` returns EOF and the thread ends; the channel then
        // disconnects, which the request side treats as a dead plugin.
        let (tx, rx) = mpsc::channel::<String>();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => break, // EOF: child exited / was killed
                    Ok(_) => {
                        if tx.send(line).is_err() {
                            break; // receiver (SubprocessRpc) dropped
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(Self {
            label: command.to_string(),
            io: Mutex::new(Io {
                child,
                stdin,
                lines: rx,
            }),
            next_id: AtomicU64::new(1),
            timeout,
            dead: AtomicBool::new(false),
        })
    }

    /// Kill the child and disable the transport so subsequent calls fail fast.
    fn disable(&self, io: &mut Io) {
        self.dead.store(true, Ordering::Relaxed);
        let _ = io.child.kill();
    }
}

impl Drop for SubprocessRpc {
    fn drop(&mut self) {
        if let Ok(mut io) = self.io.lock() {
            let _ = io.child.kill();
        }
    }
}

impl RpcTransport for SubprocessRpc {
    fn call(&self, method: &str, params: Value) -> Result<Value> {
        if self.dead.load(Ordering::Relaxed) {
            return Err(anyhow!(
                "plugin '{}' is disabled (previous timeout or I/O error)",
                self.label
            ));
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = Request {
            id,
            method: method.to_string(),
            params: Some(params),
        };
        let line = serde_json::to_string(&req)?;

        let mut io = self.io.lock().unwrap();

        if let Err(e) = write_line(&mut io.stdin, &line) {
            self.disable(&mut io);
            return Err(anyhow!("plugin '{}' write failed: {e}", self.label));
        }

        let resp_line = match io.lines.recv_timeout(self.timeout) {
            Ok(l) => l,
            Err(RecvTimeoutError::Timeout) => {
                self.disable(&mut io);
                return Err(anyhow!(
                    "plugin '{}' timed out after {} ms; disabling it for this session",
                    self.label,
                    self.timeout.as_millis()
                ));
            }
            Err(RecvTimeoutError::Disconnected) => {
                self.disable(&mut io);
                return Err(anyhow!("plugin '{}' closed its stdout", self.label));
            }
        };

        let resp: Response = serde_json::from_str(resp_line.trim_end())
            .with_context(|| format!("plugin '{}' returned invalid JSON", self.label))?;
        if resp.id != id {
            // Protocol desync: a stale/mismatched response means the request and
            // response streams are no longer aligned. Disable rather than risk
            // handing later calls the wrong plugin's output.
            self.disable(&mut io);
            return Err(anyhow!(
                "plugin '{}' response id {} does not match request id {id}",
                self.label,
                resp.id
            ));
        }
        if let Some(err) = resp.error {
            return Err(anyhow!("plugin '{}' error: {err}", self.label));
        }
        resp.result
            .ok_or_else(|| anyhow!("plugin '{}' returned no result", self.label))
    }
}

fn write_line(stdin: &mut ChildStdin, line: &str) -> std::io::Result<()> {
    stdin.write_all(line.as_bytes())?;
    stdin.write_all(b"\n")?;
    stdin.flush()
}
