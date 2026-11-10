//! A stand-in voice service in this process: it speaks the voice protocol on a loopback port so
//! the client, the conversation and the UI can be tested and shown without Python, a microphone or
//! speakers. It hears nothing by itself: a test (or a demo) makes it "hear" by pushing events
//! ([`StandInService::push`]), and it answers commands the way the real service would, without
//! sound. Every command it gets is kept ([`StandInService::commands`]).
//!
//! It binds `127.0.0.1:0` (an ephemeral port), serves one client at a time (a new connection
//! replaces the old one, like the real service), answers `GET /health`, and stops when dropped.

use craft_anything_evidence::{ErrorCode, ToolError};
use craft_anything_platform::sync::Mutex;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, PoisonError};
use std::time::{Duration, Instant};

/// How often blocked reads wake to look at the stop flag and the outbox.
const SLICE: Duration = Duration::from_millis(15);
/// The largest message taken from a client.
const MAX_MESSAGE: usize = 1 << 20;

#[derive(Default)]
struct Inner {
    /// Every command received, in order.
    commands: Mutex<Vec<Value>>,
    /// Events waiting for the current connection.
    outbox: Mutex<VecDeque<Value>>,
    /// Events sent to every new connection after its state.
    script: Mutex<Vec<Value>>,
    connections: AtomicUsize,
    /// Which connection is current (a new one replaces the old).
    current: AtomicUsize,
    stop: AtomicBool,
    kick: AtomicBool,
    shutdown_asked: AtomicBool,
    capturing: AtomicBool,
    mode: Mutex<String>,
}

/// A voice service stand-in on a loopback port. Dropping it stops it.
pub struct StandInService {
    port: u16,
    inner: Arc<Inner>,
}

impl std::fmt::Debug for StandInService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StandInService").field("port", &self.port).finish()
    }
}

impl StandInService {
    /// Start one; every client that connects gets its state, then `script` (events, in order).
    pub fn start(script: Vec<Value>) -> Result<StandInService, ToolError> {
        let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| ToolError::new(ErrorCode::Internal, format!("the voice stand-in couldn't listen: {e}")))?;
        let port = listener.local_addr().map(|a| a.port()).map_err(|e| ToolError::internal(e.to_string()))?;
        listener.set_nonblocking(true).map_err(|e| ToolError::internal(e.to_string()))?;
        let inner = Arc::new(Inner { script: Mutex::new(script), mode: Mutex::new("push_to_talk".into()), ..Default::default() });
        let st = inner.clone();
        std::thread::Builder::new()
            .name("voice-stand-in".into())
            .spawn(move || accept_loop(&listener, &st))
            .map_err(|e| ToolError::internal(format!("the voice stand-in couldn't start: {e}")))?;
        Ok(StandInService { port, inner })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Every command received so far, in order.
    pub fn commands(&self) -> Vec<Value> {
        self.inner.commands.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// The commands received so far, by their `cmd`.
    pub fn command_names(&self) -> Vec<String> {
        self.commands().iter().map(|c| c.get("cmd").and_then(Value::as_str).unwrap_or_default().to_string()).collect()
    }

    /// Wait (up to `timeout`) until a command with this `cmd` has come; whether it did.
    pub fn wait_for_command(&self, cmd: &str, timeout: Duration) -> bool {
        let t0 = Instant::now();
        while t0.elapsed() < timeout {
            if self.command_names().iter().any(|c| c == cmd) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    /// How many clients have connected.
    pub fn connections(&self) -> usize {
        self.inner.connections.load(Ordering::SeqCst)
    }

    /// Send an event to the connected client (or the next one, when none is connected now).
    pub fn push(&self, event: Value) {
        self.inner.outbox.lock().unwrap_or_else(PoisonError::into_inner).push_back(event);
    }

    /// Drop the current connection, as a network hiccup would; the service stays up.
    pub fn drop_connection(&self) {
        self.inner.kick.store(true, Ordering::SeqCst);
    }

    /// A client asked it to shut down.
    pub fn shutdown_asked(&self) -> bool {
        self.inner.shutdown_asked.load(Ordering::SeqCst)
    }

    /// Stop serving: the connection closes and nothing is accepted any more.
    pub fn stop(&self) {
        self.inner.stop.store(true, Ordering::SeqCst);
    }
}

impl Drop for StandInService {
    fn drop(&mut self) {
        self.stop();
    }
}

fn accept_loop(listener: &TcpListener, st: &Arc<Inner>) {
    while !st.stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                let st = st.clone();
                let _ = std::thread::Builder::new().name("voice-stand-in-conn".into()).spawn(move || {
                    let _ = serve(stream, &st);
                });
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => std::thread::sleep(SLICE),
            Err(_) => std::thread::sleep(SLICE),
        }
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn accept_token(key: &str) -> String {
    use base64::Engine as _;
    use sha1::{Digest, Sha1};
    let mut h = Sha1::new();
    h.update(key.as_bytes());
    h.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    base64::engine::general_purpose::STANDARD.encode(h.finalize())
}

fn state_event(st: &Inner) -> Value {
    json!({
        "type": "state", "listening": true, "capturing": st.capturing.load(Ordering::SeqCst), "speaking": false,
        "mode": st.mode.lock().unwrap_or_else(PoisonError::into_inner).clone(), "enrolled": false,
    })
}

/// One connection: plain HTTP (`/health`) or the WebSocket.
fn serve(mut stream: TcpStream, st: &Inner) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut head = Vec::new();
    let mut buf = [0u8; 4096];
    let end = loop {
        if let Some(p) = find(&head, b"\r\n\r\n") {
            break p;
        }
        if head.len() > 64 * 1024 {
            return Ok(());
        }
        let n = stream.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        head.extend_from_slice(buf.get(..n).unwrap_or_default());
    };
    let text = String::from_utf8_lossy(head.get(..end).unwrap_or_default()).into_owned();
    let leftover = head.get(end + 4..).unwrap_or_default().to_vec();
    let path = text.lines().next().and_then(|l| l.split_whitespace().nth(1)).unwrap_or("/").to_string();
    let header = |name: &str| {
        text.lines().filter_map(|l| l.split_once(':')).find(|(k, _)| k.trim().eq_ignore_ascii_case(name)).map(|(_, v)| v.trim().to_string()).unwrap_or_default()
    };
    if path != "/ws" || !header("upgrade").eq_ignore_ascii_case("websocket") {
        let (status, body) = if path == "/health" {
            ("200 OK", json!({"ok": true, "version": super::PROTOCOL_VERSION, "state": state_event(st)}).to_string())
        } else {
            ("404 Not Found", "{\"ok\":false}".to_string())
        };
        let answer = format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        stream.write_all(answer.as_bytes())?;
        return Ok(());
    }
    let accept = accept_token(&header("sec-websocket-key"));
    stream.write_all(
        format!("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n").as_bytes(),
    )?;
    let me = st.connections.fetch_add(1, Ordering::SeqCst) + 1;
    st.current.store(me, Ordering::SeqCst);
    st.kick.store(false, Ordering::SeqCst);
    let mut ws = Server { stream, buf: leftover };
    ws.send(&state_event(st))?;
    let script = st.script.lock().unwrap_or_else(PoisonError::into_inner).clone();
    for ev in script {
        ws.send(&ev)?;
    }
    loop {
        if st.stop.load(Ordering::SeqCst) || st.current.load(Ordering::SeqCst) != me {
            let _ = ws.send_frame(0x8, &[]);
            let _ = ws.stream.shutdown(Shutdown::Both);
            return Ok(());
        }
        if st.kick.swap(false, Ordering::SeqCst) {
            let _ = ws.stream.shutdown(Shutdown::Both);
            return Ok(());
        }
        loop {
            let next = st.outbox.lock().unwrap_or_else(PoisonError::into_inner).pop_front();
            match next {
                Some(ev) => ws.send(&ev)?,
                None => break,
            }
        }
        match ws.read_text()? {
            Got::Text(t) => {
                let cmd: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
                st.commands.lock().unwrap_or_else(PoisonError::into_inner).push(cmd.clone());
                for reply in answer(st, &cmd) {
                    ws.send(&reply)?;
                }
                if cmd.get("cmd").and_then(Value::as_str) == Some("shutdown") {
                    st.shutdown_asked.store(true, Ordering::SeqCst);
                    let _ = ws.send_frame(0x8, &[]);
                    let _ = ws.stream.shutdown(Shutdown::Both);
                    st.stop.store(true, Ordering::SeqCst);
                    return Ok(());
                }
            }
            Got::Nothing => {}
            Got::Closed => return Ok(()),
        }
    }
}

/// What the real service would answer (without sound).
fn answer(st: &Inner, cmd: &Value) -> Vec<Value> {
    let id = cmd.get("id").cloned();
    let with_id = |mut v: Value| {
        if let (Some(id), Some(o)) = (&id, v.as_object_mut()) {
            o.entry("id").or_insert(id.clone());
        }
        v
    };
    match cmd.get("cmd").and_then(Value::as_str).unwrap_or_default() {
        "config" => {
            if let Some(m) = cmd.get("listen_mode").and_then(Value::as_str) {
                *st.mode.lock().unwrap_or_else(PoisonError::into_inner) = m.to_string();
            }
            vec![with_id(json!({"type": "config_ok"})), state_event(st)]
        }
        "ptt" => {
            let down = cmd.get("down").and_then(Value::as_bool).unwrap_or(false);
            st.capturing.store(down, Ordering::SeqCst);
            // A made-up microphone level: someone talking while the key is down, quiet after.
            let level = if down { 0.6 } else { 0.0 };
            vec![state_event(st), json!({"type": "level", "in": level, "out": 0.0})]
        }
        "chime" => vec![],
        "speak" => vec![json!({"type": "spoken", "id": cmd.get("id").and_then(Value::as_str).unwrap_or_default(), "interrupted": false})],
        "stop_speaking" => vec![state_event(st)],
        "enroll_start" => vec![with_id(json!({"type": "enroll_progress", "seconds": 0.0, "needed": 20.0}))],
        "enroll_stop" => vec![with_id(json!({"type": "enrolled", "ok": true}))],
        "design_voice" => {
            vec![with_id(json!({"type": "voice_designed", "ref_audio": "stand-in.wav", "ref_text": cmd.get("sample_text").cloned().unwrap_or(Value::Null)}))]
        }
        "devices" => vec![with_id(json!({"type": "devices", "inputs": ["Stand-in microphone"], "outputs": ["Stand-in speakers"]}))],
        "status" => {
            let mut s = state_event(st);
            if let Some(o) = s.as_object_mut() {
                o.insert("type".into(), json!("status"));
                o.insert("version".into(), json!(super::PROTOCOL_VERSION));
            }
            vec![with_id(s)]
        }
        "shutdown" => Vec::new(),
        other => vec![json!({"type": "error", "message": format!("Unknown command: {other}"), "fatal": false})],
    }
}

enum Got {
    Text(String),
    Nothing,
    Closed,
}

/// The server's end of a WebSocket (client frames are masked; ours aren't).
struct Server {
    stream: TcpStream,
    buf: Vec<u8>,
}

impl Server {
    fn send(&mut self, v: &Value) -> std::io::Result<()> {
        self.send_frame(0x1, v.to_string().as_bytes())
    }

    fn send_frame(&mut self, opcode: u8, payload: &[u8]) -> std::io::Result<()> {
        let mut f = vec![0x80 | opcode];
        let n = payload.len();
        if n < 126 {
            f.push(n as u8);
        } else if n <= usize::from(u16::MAX) {
            f.push(126);
            f.extend_from_slice(&(n as u16).to_be_bytes());
        } else {
            f.push(127);
            f.extend_from_slice(&(n as u64).to_be_bytes());
        }
        f.extend_from_slice(payload);
        self.stream.write_all(&f)
    }

    /// One complete frame from the buffer: (opcode, payload), or None for "need more".
    fn frame(&mut self) -> std::io::Result<Option<(u8, Vec<u8>)>> {
        let (Some(&b0), Some(&b1)) = (self.buf.first(), self.buf.get(1)) else { return Ok(None) };
        let short = usize::from(b1 & 0x7f);
        let (len, mut off) = match short {
            126 => match self.buf.get(2..4) {
                Some(b) => (usize::from(u16::from_be_bytes([b.first().copied().unwrap_or(0), b.get(1).copied().unwrap_or(0)])), 4),
                None => return Ok(None),
            },
            127 => match self.buf.get(2..10) {
                Some(b) => {
                    let mut a = [0u8; 8];
                    a.copy_from_slice(b);
                    (usize::try_from(u64::from_be_bytes(a)).unwrap_or(usize::MAX), 10)
                }
                None => return Ok(None),
            },
            n => (n, 2),
        };
        if len > MAX_MESSAGE {
            return Err(std::io::Error::new(ErrorKind::InvalidData, "message too large"));
        }
        let masked = b1 & 0x80 != 0;
        let mask = if masked {
            let Some(m) = self.buf.get(off..off + 4) else { return Ok(None) };
            let m = [m.first().copied().unwrap_or(0), m.get(1).copied().unwrap_or(0), m.get(2).copied().unwrap_or(0), m.get(3).copied().unwrap_or(0)];
            off += 4;
            m
        } else {
            [0; 4]
        };
        let Some(raw) = self.buf.get(off..off + len) else { return Ok(None) };
        let payload: Vec<u8> = raw.iter().enumerate().map(|(i, b)| b ^ mask.get(i & 3).copied().unwrap_or(0)).collect();
        self.buf.drain(..off + len);
        Ok(Some((b0 & 0x0f, payload)))
    }

    /// A text message if one is there within a short wait.
    fn read_text(&mut self) -> std::io::Result<Got> {
        loop {
            if let Some((op, payload)) = self.frame()? {
                match op {
                    0x1 | 0x2 => return Ok(Got::Text(String::from_utf8_lossy(&payload).into_owned())),
                    0x8 => {
                        let _ = self.send_frame(0x8, &[]);
                        return Ok(Got::Closed);
                    }
                    0x9 => self.send_frame(0xA, &payload)?,
                    _ => {}
                }
                continue;
            }
            self.stream.set_read_timeout(Some(SLICE))?;
            let mut tmp = [0u8; 16 * 1024];
            match self.stream.read(&mut tmp) {
                Ok(0) => return Ok(Got::Closed),
                Ok(n) => self.buf.extend_from_slice(tmp.get(..n).unwrap_or_default()),
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted) => return Ok(Got::Nothing),
                Err(_) => return Ok(Got::Closed),
            }
        }
    }
}
