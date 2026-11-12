//! `abacus mobile`: the terminal's own phone front end, served from the
//! machine that runs the session.
//!
//! The relay in [`crate::remote`] puts a browser on Empero's server, which is
//! how a phone follows a session from anywhere — but the relay is a third
//! party, it only carries the frames it knows, and it needs an account. This
//! module is the local alternative: a small HTTP and WebSocket server on the
//! LAN, a token in the link, and the page in [`app.html`] opened on the phone.
//!
//! Nothing here knows what a session is. The terminal's [`Bridge`] talks to
//! this server through the same [`Connector`] the relay uses, so every frame
//! (snapshot, delta, tool, approval, question, status, workers) is the one the
//! relay already carries, and the phone is the same kind of client. The server
//! is a relay in miniature: one agent socket, any number of phone sockets, and
//! frames forwarded both ways.
//!
//! What the local transport adds is the `command` frame: the phone may ask the
//! terminal to start a new session, compact, switch mode or model, rename, or
//! fork. The terminal decides which names exist ([`crate::tui`] answers them);
//! the relay never sees this frame type, so a browser on the internet cannot
//! reach any of it.
//!
//! [`Bridge`]: crate::remote::Bridge

use std::{
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use anyhow::{Context as _, Result};
use futures_util::{SinkExt, StreamExt};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf},
    net::{TcpListener, TcpStream},
    sync::{Mutex, mpsc},
};
use tokio_tungstenite::WebSocketStream;

use crate::remote::link::{Connector, LinkError};

/// The app icon: Abacus Desktop geometric mark on deep violet-ink.
const ICON: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 512 512" role="img" aria-label="Abacus">
<defs>
<radialGradient id="glow" cx="78%" cy="14%" r="72%">
<stop offset="0%" stop-color="#b66bff" stop-opacity="0.28"/>
<stop offset="55%" stop-color="#b66bff" stop-opacity="0.06"/>
<stop offset="100%" stop-color="#0e0b14" stop-opacity="0"/>
</radialGradient>
</defs>
<rect width="512" height="512" rx="112" fill="#0e0b14"/>
<rect width="512" height="512" rx="112" fill="url(#glow)"/>
<rect x="116" y="116" width="280" height="280" fill="none" stroke="#e9e4f0" stroke-width="14"/>
<path d="M168 188h176M168 256h176M168 324h176M208 152v208M304 152v208" fill="none" stroke="#b66bff" stroke-width="16" stroke-linecap="square"/>
<rect x="8" y="8" width="496" height="496" rx="108" fill="none" stroke="#e9e4f0" stroke-opacity="0.16" stroke-width="4"/>
</svg>"##;

/// What a phone needs to add the page to its home screen.
const MANIFEST: &str = r##"{"name":"Abacus","short_name":"Abacus","description":"Drive an Abacus session from your phone.","start_url":"/","scope":"/","display":"standalone","orientation":"portrait","background_color":"#0e0b14","theme_color":"#0e0b14","icons":[{"src":"/icon.svg","sizes":"any","type":"image/svg+xml","purpose":"any maskable"}]}"##;

/// The phone page, compiled in: one file, no assets, so the binary stays the
/// only thing to ship.
const PAGE: &str = include_str!("app.html");

/// How often the server pings a phone that has gone quiet. A phone that stops
/// answering is dropped, which is what keeps a pocketed phone from holding a
/// slot forever.
const HEARTBEAT: Duration = Duration::from_secs(20);

/// A request head larger than this is not a phone; drop it.
const MAX_HEAD: usize = 16 * 1024;

/// The server, as the terminal holds it. Dropping it stops listening.
pub struct MobileServer {
    url: String,
    agent_url: String,
    port: u16,
    stop: mpsc::Sender<()>,
    /// The public link, when `--mobile-tunnel` asked for one.
    tunnel: Option<Tunnel>,
}

/// A cloudflared quick tunnel in front of this server: an https address that
/// reaches the loopback listener from any network, with no account and no
/// router port.
struct Tunnel {
    child: std::process::Child,
    public: Arc<std::sync::Mutex<Option<String>>>,
    /// Why there is no address, when cloudflared gave up.
    failure: Arc<std::sync::Mutex<Option<String>>>,
}

impl Tunnel {
    fn spawn(port: u16, token: &str) -> Result<Self> {
        let mut command = std::process::Command::new("cloudflared");
        command
            .args(["tunnel", "--no-autoupdate", "--url", &format!("http://127.0.0.1:{port}")])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());
        // A console window flashing up on Windows looks like a crash.
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            command.creation_flags(0x0800_0000);
        }
        let mut child = command.spawn().with_context(
            || "could not run cloudflared — install it, or start the phone link without --mobile-tunnel",
        )?;
        let public = Arc::new(std::sync::Mutex::new(None));
        let sink = Arc::clone(&public);
        let failure = Arc::new(std::sync::Mutex::new(None));
        let reason = Arc::clone(&failure);
        let query = format!("/?t={token}");
        if let Some(stderr) = child.stderr.take() {
            std::thread::spawn(move || {
                use std::io::BufRead as _;
                let mut last = String::new();
                for line in std::io::BufReader::new(stderr).lines() {
                    let Ok(line) = line else { break };
                    if !line.trim().is_empty() {
                        last = line.clone();
                    }
                    let Some(start) = line.find("https://") else {
                        continue;
                    };
                    let rest = &line[start..];
                    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
                    let address = &rest[..end];
                    if address.contains("trycloudflare.com")
                        && let Ok(mut slot) = sink.lock()
                    {
                        *slot = Some(format!("{address}{query}"));
                        break;
                    }
                }
                // The pipe closed: cloudflared is gone. If it never printed an
                // address, its last line is the only explanation there is.
                let published = sink.lock().map(|slot| slot.is_some()).unwrap_or(false);
                if !published && let Ok(mut slot) = reason.lock() {
                    *slot = Some(last);
                }
            });
        }
        Ok(Self { child, public, failure })
    }
}

impl MobileServer {
    /// Listen on `port` (0 picks a free one). Loopback by default: a phone
    /// link is remote control of the machine, so opening it to the network is
    /// a decision the person makes with `--mobile-lan`, not the default. The
    /// token in the returned URLs is the only credential the page needs.
    /// Called on the terminal's thread, inside its runtime.
    pub fn start(port: u16, lan: bool, tunnel: bool) -> Result<Self> {
        // A listener without SO_REUSEADDR cannot come back on a port whose
        // last connections are still in TIME_WAIT, which is every restart
        // right after a phone closed the page.
        let socket = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, Some(socket2::Protocol::TCP))
            .context("could not create a listening socket")?;
        socket.set_reuse_address(true).context("could not set SO_REUSEADDR")?;
        let bind = if lan { IpAddr::V4(Ipv4Addr::UNSPECIFIED) } else { IpAddr::V4(Ipv4Addr::LOCALHOST) };
        // A terminal restarted while the last one is still closing finds its own
        // port taken for a moment. That is not a reason to lose the phone link,
        // so wait it out rather than failing the start.
        let mut attempt = 0;
        loop {
            match socket.bind(&SocketAddr::new(bind, port).into()) {
                Ok(()) => break,
                Err(error) if attempt < 40 => {
                    attempt += 1;
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    if attempt == 40 {
                        return Err(error).with_context(|| format!("could not listen on port {port}"));
                    }
                }
                Err(error) => return Err(error).with_context(|| format!("could not listen on port {port}")),
            }
        }
        socket.listen(128).context("could not listen")?;
        socket.set_nonblocking(true)?;
        let listener: std::net::TcpListener = socket.into();
        let port = listener.local_addr()?.port();
        let listener = TcpListener::from_std(listener).context("no tokio runtime to serve from")?;
        let token = new_token();
        let hub = Arc::new(Mutex::new(Hub { token: token.clone(), clients: Vec::new(), agent: None }));
        let (stop, stopped) = mpsc::channel(1);
        tokio::spawn(accept_loop(listener, hub, stopped));
        let tunnel = if tunnel { Some(Tunnel::spawn(port, &token)?) } else { None };
        let host = if lan {
            lan_address().unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST))
        } else {
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        };
        Ok(Self {
            url: format!("http://{host}:{port}/?t={token}"),
            agent_url: format!("ws://127.0.0.1:{port}/ws?t={token}&role=agent"),
            port,
            stop,
            tunnel,
        })
    }

    /// The link to open on the phone, as text (the QR code encodes it).
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The port the server listens on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The public link once cloudflared has printed it.
    pub fn public_url(&self) -> Option<String> {
        let tunnel = self.tunnel.as_ref()?;
        let slot = tunnel.public.lock().ok()?;
        slot.clone()
    }

    /// What to put on a phone: the public link when there is one, the direct
    /// one otherwise.
    pub fn link(&self) -> String {
        self.public_url().unwrap_or_else(|| self.url.clone())
    }

    /// Why cloudflared produced no address: its last line, once it exits.
    pub fn tunnel_error(&self) -> Option<String> {
        let tunnel = self.tunnel.as_ref()?;
        let slot = tunnel.failure.lock().ok()?;
        slot.clone()
    }

    /// Whether a tunnel was asked for, even before it has an address.
    pub fn tunnel_pending(&self) -> bool {
        self.tunnel.is_some()
    }

    /// The [`Connector`] the terminal's bridge uses to reach this server. It
    /// never fails and never retries a connection that dropped for good: the
    /// socket is on loopback and the server outlives the session.
    pub fn connector(&self) -> MobileConnector {
        MobileConnector { url: self.agent_url.clone() }
    }
}

impl Drop for MobileServer {
    fn drop(&mut self) {
        let _ = self.stop.try_send(());
        if let Some(tunnel) = &mut self.tunnel {
            let _ = tunnel.child.kill();
            let _ = tunnel.child.wait();
        }
    }
}

/// The local [`Connector`]: one loopback URL, no upload, nothing to disable.
pub struct MobileConnector {
    url: String,
}

impl Connector for MobileConnector {
    async fn prepare(&self) -> Result<(), LinkError> {
        Ok(())
    }

    async fn socket_url(&self) -> Result<String, LinkError> {
        Ok(self.url.clone())
    }

    async fn disable(&self) {}
}

// ---------------------------------------------------------------------------
// Hub
// ---------------------------------------------------------------------------

/// Who is connected. Both sides are channels of text frames: the server never
/// looks inside a frame except to answer heartbeats and to learn who is there.
struct Hub {
    token: String,
    /// One sender per phone; a phone whose receiver is gone is dropped on the
    /// next broadcast.
    clients: Vec<mpsc::UnboundedSender<String>>,
    /// The terminal's socket, while it is open.
    agent: Option<mpsc::UnboundedSender<String>>,
}

impl Hub {
    fn phones(&self) -> usize {
        self.clients.len()
    }

    fn broadcast(&mut self, text: String) {
        self.clients.retain(|client| client.send(text.clone()).is_ok());
    }

    fn send_to_agent(&mut self, text: String) {
        if let Some(agent) = &self.agent
            && agent.send(text).is_err()
        {
            self.agent = None;
        }
    }
}

async fn accept_loop(listener: TcpListener, hub: Arc<Mutex<Hub>>, mut stop: mpsc::Receiver<()>) {
    loop {
        tokio::select! {
            _ = stop.recv() => return,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let hub = hub.clone();
                    tokio::spawn(async move {
                        let _ = serve_connection(stream, hub).await;
                    });
                }
                Err(_) => return,
            },
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

/// Read one request head, then answer it: a page, a health check, or a
/// WebSocket that becomes a client or the agent.
async fn serve_connection(mut stream: TcpStream, hub: Arc<Mutex<Hub>>) -> Result<()> {
    let (head, raw) = match read_head(&mut stream).await? {
        Some(parts) => parts,
        None => return Ok(()),
    };
    let request = head.lines().next().unwrap_or_default().to_owned();
    let mut words = request.split_whitespace();
    let method = words.next().unwrap_or_default();
    let target = words.next().unwrap_or_default();
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let params = query_params(query);
    let token = params.get("t").map(String::as_str).unwrap_or_default();

    if method != "GET" && method != "HEAD" {
        return respond(&mut stream, 405, "text/plain", "method not allowed").await;
    }
    match path {
        "/health" => return respond(&mut stream, 200, "text/plain", "ok").await,
        "/favicon.ico" => return respond(&mut stream, 204, "text/plain", "").await,
        // The icon and the manifest hold nothing about the session, and the
        // browser fetches them without the link's query string.
        "/icon.svg" => return respond(&mut stream, 200, "image/svg+xml", ICON).await,
        "/manifest.webmanifest" => {
            return respond(&mut stream, 200, "application/manifest+json", MANIFEST).await;
        }
        "/ws" => {
            if token != hub.lock().await.token {
                return respond(&mut stream, 401, "text/plain", "that link is not valid").await;
            }
            let role = params.get("role").map(String::as_str).unwrap_or("client");
            let rewind = Rewind { head: raw, at: 0, inner: stream };
            let socket = match tokio_tungstenite::accept_hdr_async(rewind, accept_all).await {
                Ok(socket) => socket,
                Err(_) => return Ok(()),
            };
            if role == "agent" { agent_session(socket, hub).await } else { client_session(socket, hub).await }
        }
        "/" | "/index.html" => {
            if token != hub.lock().await.token {
                return respond(
                    &mut stream,
                    401,
                    "text/html; charset=utf-8",
                    "<!doctype html><meta name=viewport content='width=device-width,initial-scale=1'>\
                     <body style='font:16px system-ui;background:#0b0c0f;color:#e6e6e6;padding:2rem'>\
                     <h1 style='font-size:1.1rem'>Link expired</h1>\
                     <p>Open the link Abacus printed for this session, or run <code>/mobile</code> again.</p>",
                )
                .await;
            }
            return respond(&mut stream, 200, "text/html; charset=utf-8", PAGE).await;
        }
        _ => return respond(&mut stream, 404, "text/plain", "not found").await,
    }
}

/// The request head as text, and every byte that was read — the whole request,
/// which the WebSocket handshake has to read again from the start.
async fn read_head(stream: &mut TcpStream) -> Result<Option<(String, Vec<u8>)>> {
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 1024];
    loop {
        if let Some(end) = find_head_end(&buffer) {
            let head = String::from_utf8_lossy(&buffer[..end]).into_owned();
            return Ok(Some((head, buffer)));
        }
        if buffer.len() > MAX_HEAD {
            return Ok(None);
        }
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Ok(None);
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
}

fn find_head_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n").map(|at| at + 4)
}

fn query_params(query: &str) -> std::collections::HashMap<String, String> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (decode(key), decode(value)))
        .collect()
}

/// `%XX` and `+`, the two a link needs.
fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => out.push(b' '),
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            byte => out.push(byte),
        }
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The WebSocket handshake accepts whatever the HTTP layer already routed:
/// the token and the role were checked before this point. The error type is
/// tungstenite's, and large; the signature is not ours to narrow.
#[allow(clippy::result_large_err)]
fn accept_all(
    _request: &tokio_tungstenite::tungstenite::handshake::server::Request,
    response: tokio_tungstenite::tungstenite::handshake::server::Response,
) -> Result<
    tokio_tungstenite::tungstenite::handshake::server::Response,
    tokio_tungstenite::tungstenite::handshake::server::ErrorResponse,
> {
    Ok(response)
}

async fn respond(stream: &mut TcpStream, status: u16, kind: &str, body: &str) -> Result<()> {
    let reason = match status {
        200 => "OK",
        204 => "No Content",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: no-store\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body.as_bytes()).await?;
    stream.flush().await?;
    let _ = stream.shutdown().await;
    Ok(())
}

/// A stream whose first bytes are a buffer already read off the socket. The
/// WebSocket handshake reads its own request, so the head the HTTP layer
/// consumed has to be handed back to it.
struct Rewind {
    head: Vec<u8>,
    at: usize,
    inner: TcpStream,
}

impl AsyncRead for Rewind {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.at < this.head.len() {
            let room = buf.remaining().min(this.head.len() - this.at);
            buf.put_slice(&this.head[this.at..this.at + room]);
            this.at += room;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for Rewind {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

// ---------------------------------------------------------------------------
// Sockets
// ---------------------------------------------------------------------------

type Socket = WebSocketStream<Rewind>;

/// A phone: registered with the hub, told who is here, then a pipe between
/// the socket and the hub.
async fn client_session(socket: Socket, hub: Arc<Mutex<Hub>>) -> Result<()> {
    let (mut sink, mut source) = socket.split();
    let (frames, mut incoming) = mpsc::unbounded_channel::<String>();
    let phones = {
        let mut hub = hub.lock().await;
        hub.clients.push(frames);
        hub.phones()
    };
    let _ = sink.send(tokio_tungstenite::tungstenite::Message::Text(hello(phones, true).into())).await;
    let mut beat = tokio::time::interval(HEARTBEAT);
    beat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            frame = incoming.recv() => {
                let Some(frame) = frame else { break };
                if sink.send(tokio_tungstenite::tungstenite::Message::Text(frame.into())).await.is_err() {
                    break;
                }
            }
            message = source.next() => {
                let Some(message) = message else { break };
                match message {
                    Ok(tokio_tungstenite::tungstenite::Message::Text(text)) => {
                        if kind_of(&text) == Some("ping") {
                            let _ = sink.send(tokio_tungstenite::tungstenite::Message::Text(pong(&text).into())).await;
                        } else if kind_of(&text) != Some("pong") {
                            hub.lock().await.send_to_agent(text.to_string());
                        }
                    }
                    Ok(tokio_tungstenite::tungstenite::Message::Close(_)) | Err(_) => break,
                    Ok(_) => {}
                }
            }
            _ = beat.tick() => {
                let ping = format!(
                    "{{\"v\":1,\"id\":\"server\",\"seq\":0,\"type\":\"ping\",\"payload\":{{\"ts\":\"{}\"}}}}",
                    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
                );
                if sink.send(tokio_tungstenite::tungstenite::Message::Text(ping.into())).await.is_err() {
                    break;
                }
            }
        }
    }
    let mut hub = hub.lock().await;
    hub.clients.retain(|client| !client.is_closed());
    let phones = hub.phones();
    let agent = hub.agent.is_some();
    hub.broadcast(peer_state(phones, agent));
    Ok(())
}

/// The terminal: frames from it go to every phone, frames from the phones go
/// back to it, and its heartbeat is answered so the link stays up.
async fn agent_session(socket: Socket, hub: Arc<Mutex<Hub>>) -> Result<()> {
    let (mut sink, mut source) = socket.split();
    let (frames, mut incoming) = mpsc::unbounded_channel::<String>();
    let phones = {
        let mut hub = hub.lock().await;
        hub.agent = Some(frames);
        hub.phones()
    };
    let _ = sink.send(tokio_tungstenite::tungstenite::Message::Text(hello(phones, true).into())).await;
    hub.lock().await.broadcast(peer_state(phones, true));
    loop {
        tokio::select! {
            frame = incoming.recv() => {
                let Some(frame) = frame else { break };
                if sink.send(tokio_tungstenite::tungstenite::Message::Text(frame.into())).await.is_err() {
                    break;
                }
            }
            message = source.next() => {
                let Some(message) = message else { break };
                match message {
                    Ok(tokio_tungstenite::tungstenite::Message::Text(text)) => {
                        if kind_of(&text) == Some("ping") {
                            let _ = sink.send(tokio_tungstenite::tungstenite::Message::Text(pong(&text).into())).await;
                        } else {
                            hub.lock().await.broadcast(text.to_string());
                        }
                    }
                    Ok(tokio_tungstenite::tungstenite::Message::Close(_)) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        }
    }
    let mut hub = hub.lock().await;
    hub.agent = None;
    let phones = hub.phones();
    hub.broadcast(peer_state(phones, false));
    Ok(())
}

/// The frame type of a text frame, without parsing the rest of it.
fn kind_of(text: &str) -> Option<&str> {
    let at = text.find("\"type\"")?;
    let rest = &text[at + 6..];
    let start = rest.find('"')? + 1;
    let end = rest[start..].find('"')? + start;
    Some(&rest[start..end])
}

fn pong(ping: &str) -> String {
    let ts = serde_json::from_str::<serde_json::Value>(ping)
        .ok()
        .and_then(|frame| frame.pointer("/payload/ts").cloned())
        .unwrap_or(serde_json::Value::Null);
    serde_json::json!({"v": 1, "id": "server", "seq": 0, "type": "pong", "payload": {"ts": ts}}).to_string()
}

fn hello(phones: usize, agent: bool) -> String {
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    serde_json::json!({
        "v": 1,
        "id": "server",
        "seq": 0,
        "type": "hello",
        "payload": {
            "role": "relay",
            "session_id": "local",
            "server_time": now,
            "heartbeat_interval_s": HEARTBEAT.as_secs(),
            "idle_timeout_s": 75,
            "peers": { "browsers": phones, "agent": agent },
        }
    })
    .to_string()
}

fn peer_state(phones: usize, agent: bool) -> String {
    serde_json::json!({
        "v": 1,
        "id": "server",
        "seq": 0,
        "type": "peer_state",
        "payload": { "role": "browser", "online": true, "browsers": phones, "agent": agent }
    })
    .to_string()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A random 128-bit token, hex: the only thing standing between the LAN and a
/// session's transcript.
fn new_token() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// This machine's address on the network a phone is likely to share: whatever
/// the route to the internet picks, with loopback when there is no route.
fn lan_address() -> Option<IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    Some(socket.local_addr().ok()?.ip())
}

/// The address to listen on, for a caller that wants to say where it listens.
pub fn address(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    /// One plain HTTP request against the server, answered in full.
    fn get(port: u16, target: &str) -> (u16, String) {
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
        stream.set_read_timeout(Some(Duration::from_secs(5))).expect("timeout");
        let request = format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).expect("write");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("read");
        let status = response.split_whitespace().nth(1).and_then(|code| code.parse().ok()).expect("status line");
        (status, response)
    }

    fn token_of(server: &MobileServer) -> String {
        server.url().split("t=").nth(1).expect("token in the url").to_owned()
    }

    // A blocking client needs a runtime with its own workers: a current-thread
    // runtime would stall the server that has to answer it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_page_answers_only_with_the_link_token() {
        let server = MobileServer::start(0, false, false).expect("start");
        let port = server.port();
        let token = token_of(&server);

        let (status, body) = get(port, "/");
        assert_eq!(status, 401, "a bare URL is not a link");
        assert!(body.contains("Link expired"), "{body}");

        let (status, body) = get(port, &format!("/?t={token}"));
        assert_eq!(status, 200);
        assert!(body.contains("<title>Abacus</title>"), "the page itself");
        assert!(body.contains("app.html") || body.contains("Message Abacus"), "the phone page");

        let (status, _) = get(port, "/?t=not-the-token");
        assert_eq!(status, 401);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_icon_and_manifest_need_no_token() {
        let server = MobileServer::start(0, false, false).expect("start");
        let port = server.port();

        let (status, body) = get(port, "/icon.svg");
        assert_eq!(status, 200);
        assert!(body.contains("<svg") && body.contains("#b66bff"), "an abacus, not a placeholder");

        let (status, body) = get(port, "/manifest.webmanifest");
        assert_eq!(status, 200);
        assert!(body.contains("\"short_name\":\"Abacus\""), "{body}");

        let (status, body) = get(port, "/health");
        assert_eq!(status, 200);
        assert!(body.contains("ok"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_websocket_url_carries_the_agent_role() {
        let server = MobileServer::start(0, false, false).expect("start");
        assert!(server.connector().url.contains("role=agent"), "the bridge is the agent end");
        assert!(server.url().starts_with("http://"), "the phone gets a page, not a socket");
    }
}
