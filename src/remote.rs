//! Remote dictation session: streams 16 kHz mono PCM to the whisp gateway over a
//! WebSocket while the key is held, relays partial transcripts, and returns the
//! cleaned final text after release. OWNER: integration agent.
//!
//! Protocol (see desktop/compute/srv/whisp/gateway/gateway.py):
//!   -> {"type":"start", sample_rate, app_class, app_title, dictionary}
//!   -> binary float32-LE frames
//!   -> {"type":"end"} | {"type":"cancel"}
//!   <- {"type":"partial","text"}  <- {"type":"final",raw,text,cleaned,asr_ms,llm_ms}  <- {"type":"error",message}
//!
//! One background thread per session owns the socket. Audio is queued on an mpsc
//! channel, so nothing is lost while the TCP/WebSocket handshake is still in
//! flight (the first ~150 ms of a WAN session). The socket is non-blocking: the
//! loop alternates between draining queued audio and polling for server frames.

use std::net::{TcpStream, ToSocketAddrs};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use serde_json::json;
use tungstenite::{Message, WebSocket};

/// Give up on the TCP connect after this long (then the daemon falls back to local ASR).
const CONNECT_TIMEOUT: Duration = Duration::from_millis(1500);
/// After `end`, wait at most this long for the final transcript.
const FINAL_TIMEOUT: Duration = Duration::from_millis(6000);
/// Idle sleep between polls when no audio is queued and no frame is pending.
const POLL_SLEEP: Duration = Duration::from_millis(4);

/// Session context sent in the `start` frame.
pub struct StartCtx {
    pub app_class: String,
    pub app_title: String,
    pub dictionary: Vec<String>,
}

/// How a session ended.
#[derive(Debug)]
pub enum Outcome {
    /// Server produced a transcript (possibly cleaned by the LLM).
    Final {
        raw: String,
        text: String,
        cleaned: bool,
        asr_ms: u64,
        llm_ms: u64,
    },
    /// Server heard nothing.
    Empty,
    /// Connection/protocol failure; the caller should fall back to local ASR.
    Failed(String),
}

/// Commands from the owner thread to the session thread.
pub enum Cmd {
    Audio(Vec<f32>),
    End,
    Cancel,
}

/// Handle to a live session. Dropping it without `End`/`Cancel` tears the socket down.
pub struct Session {
    tx: mpsc::Sender<Cmd>,
}

impl Session {
    /// Spawn a session thread. `warm` is a pre-connected socket from `Warm::take`
    /// (saves the TCP + WebSocket handshake); `None` connects now. `on_partial`
    /// fires for each interim transcript; `on_done` fires exactly once.
    pub fn start(
        url: String,
        warm: Option<WebSocket<TcpStream>>,
        ctx: StartCtx,
        on_partial: impl Fn(String) + Send + 'static,
        on_done: impl FnOnce(Outcome) + Send + 'static,
    ) -> Session {
        let (tx, rx) = mpsc::channel::<Cmd>();
        std::thread::spawn(move || {
            let outcome = match run(&url, warm, ctx, rx, &on_partial) {
                Ok(o) => o,
                Err(e) => Outcome::Failed(format!("{e:#}")),
            };
            on_done(outcome);
        });
        Session { tx }
    }

    /// A sender handle so a forwarder thread can push audio without borrowing the session.
    pub fn sender(&self) -> mpsc::Sender<Cmd> {
        self.tx.clone()
    }
    pub fn cancel(&self) {
        let _ = self.tx.send(Cmd::Cancel);
    }
}

#[derive(Deserialize)]
struct ServerMsg {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    raw: String,
    #[serde(default)]
    cleaned: bool,
    #[serde(default)]
    asr_ms: u64,
    #[serde(default)]
    llm_ms: u64,
    #[serde(default)]
    message: String,
}

/// Host:port from a `ws://host:port[/path]` URL.
fn host_port(url: &str) -> Result<String> {
    let rest = url
        .strip_prefix("ws://")
        .ok_or_else(|| anyhow!("gateway url must start with ws:// (got {url})"))?;
    let hp = rest.split('/').next().unwrap_or(rest);
    if hp.contains(':') {
        Ok(hp.to_string())
    } else {
        Ok(format!("{hp}:80"))
    }
}

fn connect(url: &str) -> Result<WebSocket<TcpStream>> {
    let hp = host_port(url)?;
    let addr = hp
        .to_socket_addrs()
        .with_context(|| format!("resolving {hp}"))?
        .next()
        .ok_or_else(|| anyhow!("no address for {hp}"))?;
    let stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)
        .with_context(|| format!("connecting to gateway {hp}"))?;
    stream.set_nodelay(true)?;
    // Handshake blocking (fast on a direct tailnet path), then switch to non-blocking I/O.
    stream.set_read_timeout(Some(Duration::from_millis(3000)))?;
    let (ws, _resp) = tungstenite::client(url, stream).context("websocket handshake")?;
    ws.get_ref().set_read_timeout(None)?;
    ws.get_ref().set_nonblocking(true)?;
    Ok(ws)
}

fn is_would_block(e: &tungstenite::Error) -> bool {
    matches!(e, tungstenite::Error::Io(io) if io.kind() == std::io::ErrorKind::WouldBlock)
}

/// Send, retrying flushes while the socket is momentarily unwritable.
fn send_all(ws: &mut WebSocket<TcpStream>, msg: Message) -> Result<()> {
    match ws.send(msg) {
        Ok(()) => Ok(()),
        Err(e) if is_would_block(&e) => {
            // Queued inside tungstenite; flush until it drains.
            let start = Instant::now();
            loop {
                match ws.flush() {
                    Ok(()) => return Ok(()),
                    Err(e) if is_would_block(&e) => {
                        if start.elapsed() > Duration::from_secs(2) {
                            bail!("gateway socket stalled while sending");
                        }
                        std::thread::sleep(POLL_SLEEP);
                    }
                    Err(e) => return Err(e.into()),
                }
            }
        }
        Err(e) => Err(e.into()),
    }
}

fn run(
    url: &str,
    warm: Option<WebSocket<TcpStream>>,
    ctx: StartCtx,
    rx: mpsc::Receiver<Cmd>,
    on_partial: &(impl Fn(String) + Send + 'static),
) -> Result<Outcome> {
    let t0 = Instant::now();
    let mut ws = match warm {
        Some(ws) => {
            eprintln!("[whisp] remote: using warm connection");
            ws
        }
        None => {
            let ws = connect(url)?;
            eprintln!("[whisp] remote: connected in {} ms", t0.elapsed().as_millis());
            ws
        }
    };

    let start = json!({
        "type": "start",
        "sample_rate": 16000,
        "app_class": ctx.app_class,
        "app_title": ctx.app_title,
        "dictionary": ctx.dictionary,
    });
    send_all(&mut ws, Message::Text(start.to_string().into()))?;

    let mut ended_at: Option<Instant> = None;
    let mut sender_alive = true;

    loop {
        // 1. Drain queued commands.
        let mut did_work = false;
        while sender_alive {
            match rx.try_recv() {
                Ok(Cmd::Audio(pcm)) => {
                    if ended_at.is_none() {
                        let mut bytes = Vec::with_capacity(pcm.len() * 4);
                        for s in &pcm {
                            bytes.extend_from_slice(&s.to_le_bytes());
                        }
                        send_all(&mut ws, Message::Binary(bytes.into()))?;
                        did_work = true;
                    }
                }
                Ok(Cmd::End) => {
                    if ended_at.is_none() {
                        send_all(&mut ws, Message::Text(r#"{"type":"end"}"#.into()))?;
                        ended_at = Some(Instant::now());
                        did_work = true;
                    }
                }
                Ok(Cmd::Cancel) => {
                    let _ = send_all(&mut ws, Message::Text(r#"{"type":"cancel"}"#.into()));
                    let _ = ws.close(None);
                    return Ok(Outcome::Failed("cancelled".into()));
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    sender_alive = false;
                    if ended_at.is_none() {
                        // Owner dropped the session without End: nothing to wait for.
                        let _ = ws.close(None);
                        return Ok(Outcome::Failed("session dropped".into()));
                    }
                }
            }
        }

        // 2. Poll for a server frame.
        match ws.read() {
            Ok(Message::Text(t)) => {
                did_work = true;
                let msg: ServerMsg = serde_json::from_str(&t).context("bad gateway frame")?;
                match msg.kind.as_str() {
                    "partial" => on_partial(msg.text),
                    "final" => {
                        let _ = ws.close(None);
                        return Ok(if msg.raw.trim().is_empty() && msg.text.trim().is_empty() {
                            Outcome::Empty
                        } else {
                            Outcome::Final {
                                raw: msg.raw,
                                text: msg.text,
                                cleaned: msg.cleaned,
                                asr_ms: msg.asr_ms,
                                llm_ms: msg.llm_ms,
                            }
                        });
                    }
                    "error" => bail!("gateway error: {}", msg.message),
                    other => eprintln!("[whisp] remote: ignoring frame type {other:?}"),
                }
            }
            Ok(Message::Close(_)) => bail!("gateway closed the connection"),
            Ok(_) => {} // ping/pong/binary: nothing to do
            Err(e) if is_would_block(&e) => {}
            Err(e) => return Err(anyhow!("gateway read: {e}")),
        }

        // 3. Final-timeout guard.
        if let Some(t) = ended_at {
            if t.elapsed() > FINAL_TIMEOUT {
                bail!("no final transcript within {:?}", FINAL_TIMEOUT);
            }
        }

        if !did_work {
            std::thread::sleep(POLL_SLEEP);
        }
    }
}

// ---------------------------------------------------------------------------
// Warm connection: one spare WebSocket kept open to the gateway while idle, so a
// key press skips the TCP + WebSocket handshake. The gateway pings every 20 s;
// the keeper answers pongs and reconnects when the socket dies.
// ---------------------------------------------------------------------------

pub struct Warm {
    slot: Arc<Mutex<Option<WebSocket<TcpStream>>>>,
}

impl Warm {
    pub fn start(url: String) -> Warm {
        let slot: Arc<Mutex<Option<WebSocket<TcpStream>>>> = Arc::new(Mutex::new(None));
        let keeper = Arc::clone(&slot);
        std::thread::Builder::new()
            .name("whisp-warm".into())
            .spawn(move || {
                let mut backoff = Duration::from_secs(2);
                loop {
                    let have = keeper.lock().map(|g| g.is_some()).unwrap_or(false);
                    if !have {
                        match connect(&url) {
                            Ok(ws) => {
                                if let Ok(mut g) = keeper.lock() {
                                    *g = Some(ws);
                                }
                                eprintln!("[whisp] warm: spare connection ready");
                                backoff = Duration::from_secs(2);
                            }
                            Err(e) => {
                                eprintln!("[whisp] warm: connect failed ({e:#}); retry in {backoff:?}");
                                std::thread::sleep(backoff);
                                backoff = (backoff * 2).min(Duration::from_secs(60));
                                continue;
                            }
                        }
                    }
                    std::thread::sleep(Duration::from_millis(500));
                    // Service the idle socket: answer pings, notice closes.
                    if let Ok(mut g) = keeper.lock() {
                        let dead = match g.as_mut() {
                            Some(ws) => match ws.read() {
                                Ok(Message::Close(f)) => Some(format!("server closed: {f:?}")),
                                Ok(_) => match ws.flush() {
                                    Ok(()) => None,
                                    Err(e) if is_would_block(&e) => None,
                                    Err(e) => Some(format!("flush: {e}")),
                                },
                                Err(e) if is_would_block(&e) => None,
                                Err(e) => Some(format!("read: {e}")),
                            },
                            None => None,
                        };
                        if let Some(why) = dead {
                            eprintln!("[whisp] warm: connection dropped ({why}); reconnecting");
                            *g = None;
                        }
                    }
                }
            })
            .expect("spawning warm-connection thread");
        Warm { slot }
    }

    /// Take the spare socket, if one is ready. The keeper opens a new one.
    pub fn take(&self) -> Option<WebSocket<TcpStream>> {
        let got = self.slot.lock().ok().and_then(|mut g| g.take());
        if got.is_none() {
            eprintln!("[whisp] warm: no spare connection at press time; connecting cold");
        }
        got
    }
}

/// Active Hyprland window (class, title) via `hyprctl activewindow -j`; empty on failure.
pub fn active_window() -> (String, String) {
    let out = std::process::Command::new("hyprctl")
        .args(["activewindow", "-j"])
        .output();
    let Ok(out) = out else {
        return (String::new(), String::new());
    };
    let v: serde_json::Value = match serde_json::from_slice(&out.stdout) {
        Ok(v) => v,
        Err(_) => return (String::new(), String::new()),
    };
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    (s("class"), s("title"))
}

/// Personal dictionary: `~/.config/whisp/dictionary.txt`, one term per line, `#` comments.
pub fn load_dictionary() -> Vec<String> {
    let Some(home) = std::env::var_os("HOME") else {
        return Vec::new();
    };
    let path = std::path::PathBuf::from(home).join(".config/whisp/dictionary.txt");
    match std::fs::read_to_string(path) {
        Ok(s) => s
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(str::to_string)
            .collect(),
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_port_parsing() {
        assert_eq!(host_port("ws://10.0.0.1:8082").unwrap(), "10.0.0.1:8082");
        assert_eq!(host_port("ws://host:8082/path").unwrap(), "host:8082");
        assert_eq!(host_port("ws://host").unwrap(), "host:80");
        assert!(host_port("http://host:1").is_err());
    }

    #[test]
    fn server_msg_defaults() {
        let m: ServerMsg = serde_json::from_str(r#"{"type":"partial","text":"hi"}"#).unwrap();
        assert_eq!(m.kind, "partial");
        assert_eq!(m.text, "hi");
        assert!(!m.cleaned);
        let f: ServerMsg =
            serde_json::from_str(r#"{"type":"final","raw":"a","text":"A.","cleaned":true,"asr_ms":1,"llm_ms":2}"#)
                .unwrap();
        assert!(f.cleaned);
        assert_eq!(f.llm_ms, 2);
    }
}
