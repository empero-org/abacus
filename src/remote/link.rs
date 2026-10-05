//! The agent's WebSocket to the relay, as one background task per shared
//! session: fetch a fresh one-use ticket, connect, keep the connection alive
//! with application heartbeats, and reconnect with jittered exponential
//! backoff until the session stops being shared.
//!
//! The task never blocks the terminal. The bridge hands it frames over a
//! bounded queue; while the socket is down those frames are dropped, because
//! every reconnect is followed by a full snapshot that supersedes them.

use super::{
    Inbound, Notify,
    protocol::{self, AcceptResult, Accepted, Incoming, Input, NoticeFrame, Outbound, Ping},
};
use futures_util::{SinkExt, StreamExt};
use std::{collections::VecDeque, future::Future, time::Duration};
use tokio::{
    net::TcpStream,
    sync::{mpsc, oneshot},
    time::{Instant, interval_at, sleep, timeout},
};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{
        Error as WsError, Message,
        protocol::{CloseFrame, frame::coding::CloseCode},
    },
};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Why a step of setting up the connection failed, in the terms the loop
/// acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LinkError {
    /// Retrying cannot help (signed out, sharing refused); stop and say why.
    Fatal(String),
    /// Network trouble or a busy server; back off and try again.
    Retry(String),
}

/// What the link needs from the sync server. A trait so tests can run the
/// whole loop against an in-process socket server without REST.
pub(crate) trait Connector: Send + Sync + 'static {
    /// Make the session shareable: upload it and enable remote control.
    /// Called once, before the first connection.
    fn prepare(&self) -> impl Future<Output = Result<(), LinkError>> + Send;
    /// A socket URL carrying a fresh one-use ticket.
    fn socket_url(&self) -> impl Future<Output = Result<String, LinkError>> + Send;
    /// Stop the session being discoverable, best effort.
    fn disable(&self) -> impl Future<Output = ()> + Send;
}

/// Timings. The relay's `hello` overrides the heartbeat and idle timeout.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LinkConfig {
    pub heartbeat: Duration,
    pub idle_timeout: Duration,
    pub backoff_base: Duration,
    pub backoff_cap: Duration,
    pub connect_timeout: Duration,
    pub send_timeout: Duration,
    /// A connection that lived this long resets the backoff; one that drops
    /// sooner keeps growing it, so a relay that accepts and immediately
    /// closes is not hammered once a second.
    pub stable_after: Duration,
    /// Upper bound on the REST disable when sharing stops.
    pub disable_timeout: Duration,
}

impl Default for LinkConfig {
    fn default() -> Self {
        Self {
            heartbeat: Duration::from_secs(25),
            idle_timeout: Duration::from_secs(75),
            backoff_base: Duration::from_secs(1),
            backoff_cap: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(15),
            send_timeout: Duration::from_secs(10),
            stable_after: Duration::from_secs(30),
            disable_timeout: Duration::from_secs(3),
        }
    }
}

/// How the terminal asked the link to end.
#[derive(Debug)]
pub(crate) struct Stop {
    /// Told to the browsers before the socket closes.
    pub notice: String,
    /// Also stop the session being discoverable (`/remote` off, exit). Not
    /// when the bridge is merely dropped.
    pub disable: bool,
}

/// Exponential backoff, doubling from `base` to `cap`, each delay jittered by
/// ±20 % so a fleet of terminals recovering from one outage spreads out.
#[derive(Debug, Clone)]
pub(crate) struct Backoff {
    base: Duration,
    cap: Duration,
    attempt: u32,
}

impl Backoff {
    pub fn new(base: Duration, cap: Duration) -> Self {
        Self { base, cap, attempt: 0 }
    }

    pub fn next_delay(&mut self) -> Duration {
        let ceiling = self.base.saturating_mul(1_u32 << self.attempt.min(16)).min(self.cap);
        self.attempt = self.attempt.saturating_add(1);
        ceiling.mul_f64(0.8 + 0.4 * unit_random()).min(self.cap)
    }

    pub fn reset(&mut self) {
        self.attempt = 0;
    }
}

/// Uniform in `[0, 1)`, from the v4 UUID generator already in the build.
fn unit_random() -> f64 {
    (uuid::Uuid::new_v4().as_u128() as u64 >> 11) as f64 / (1_u64 << 53) as f64
}

/// The ids of the last few browser frames. A browser re-sends an input it saw
/// no `ack` for, with the same id, after reconnecting; acting on it twice
/// would run a prompt twice.
#[derive(Debug)]
pub(crate) struct RecentIds {
    ids: VecDeque<String>,
    capacity: usize,
}

impl RecentIds {
    pub fn new(capacity: usize) -> Self {
        Self { ids: VecDeque::with_capacity(capacity), capacity }
    }

    /// Record `id`; false when it was already seen.
    pub fn insert(&mut self, id: &str) -> bool {
        if self.ids.iter().any(|seen| seen == id) {
            return false;
        }
        if self.ids.len() == self.capacity {
            self.ids.pop_front();
        }
        self.ids.push_back(id.to_owned());
        true
    }
}

/// Something that ended the link while it was not talking to the relay.
enum Halt {
    Stop(Stop),
    /// The bridge went away without asking.
    Dropped,
}

/// How one connection ended.
enum Ended {
    Halted(Halt),
    /// Worth reconnecting.
    Lost(String),
    /// The relay refused the ticket; one quick retry with a fresh one.
    Refused(String),
    /// Reconnecting cannot help.
    Fatal(String),
}

/// Run the link until the session stops being shared. `commands` carries the
/// bridge's frames; `stop` ends the link cleanly.
pub(crate) async fn run<C: Connector>(
    connector: C,
    mut commands: mpsc::Receiver<Outbound>,
    mut stop: oneshot::Receiver<Stop>,
    notify: Notify,
    config: LinkConfig,
) {
    let mut backoff = Backoff::new(config.backoff_base, config.backoff_cap);
    loop {
        match guarded(connector.prepare(), &mut commands, &mut stop).await {
            Err(halt) => return halt_link(&connector, halt, &config).await,
            Ok(Ok(())) => break,
            Ok(Err(LinkError::Fatal(reason))) => return notify(Inbound::Closed { reason }),
            Ok(Err(LinkError::Retry(reason))) => {
                let retry_in = backoff.next_delay();
                notify(Inbound::Disconnected { reason, retry_in });
                if let Err(halt) = guarded(sleep(retry_in), &mut commands, &mut stop).await {
                    return halt_link(&connector, halt, &config).await;
                }
            }
        }
    }
    backoff.reset();

    let mut seen = RecentIds::new(64);
    let mut reconnect = false;
    let mut refusals = 0;
    loop {
        let attempt = async {
            let url = match connector.socket_url().await {
                Ok(url) => url,
                Err(LinkError::Fatal(reason)) => return Err(Ended::Fatal(reason)),
                Err(LinkError::Retry(reason)) => return Err(Ended::Lost(reason)),
            };
            match timeout(config.connect_timeout, tokio_tungstenite::connect_async(url.as_str())).await {
                Err(_) => Err(Ended::Lost("timed out connecting to the relay".into())),
                Ok(Err(error)) => Err(handshake_error(&error)),
                Ok(Ok((socket, _))) => Ok(socket),
            }
        };
        let ended = match guarded(attempt, &mut commands, &mut stop).await {
            Err(halt) => Ended::Halted(halt),
            Ok(Err(ended)) => ended,
            Ok(Ok(socket)) => {
                notify(Inbound::Connected { reconnect });
                reconnect = true;
                let opened = Instant::now();
                let (ended, greeted) = serve(socket, &mut commands, &mut stop, &notify, &mut seen, &config).await;
                if greeted {
                    refusals = 0;
                }
                if opened.elapsed() >= config.stable_after {
                    backoff.reset();
                }
                ended
            }
        };
        let reason = match ended {
            Ended::Halted(halt) => return halt_link(&connector, halt, &config).await,
            Ended::Fatal(reason) => return notify(Inbound::Closed { reason }),
            Ended::Refused(reason) => {
                refusals += 1;
                if refusals > 1 {
                    return notify(Inbound::Closed { reason });
                }
                reason
            }
            Ended::Lost(reason) => reason,
        };
        let retry_in = backoff.next_delay();
        notify(Inbound::Disconnected { reason, retry_in });
        if let Err(halt) = guarded(sleep(retry_in), &mut commands, &mut stop).await {
            return halt_link(&connector, halt, &config).await;
        }
    }
}

/// Await `future` while the link is offline: frames that arrive meanwhile are
/// dropped (a snapshot follows the next connect), and a stop ends the wait.
async fn guarded<F: Future>(
    future: F,
    commands: &mut mpsc::Receiver<Outbound>,
    stop: &mut oneshot::Receiver<Stop>,
) -> Result<F::Output, Halt> {
    tokio::pin!(future);
    loop {
        tokio::select! {
            output = &mut future => return Ok(output),
            request = &mut *stop => return Err(request.map_or(Halt::Dropped, Halt::Stop)),
            frame = commands.recv() => {
                if frame.is_none() {
                    return Err(halted(stop));
                }
            }
        }
    }
}

/// Why the bridge's frame queue closed. [`Bridge::stop`](super::Bridge::stop)
/// sends its request before dropping the queue, so a request is already
/// waiting when the stop was deliberate.
fn halted(stop: &mut oneshot::Receiver<Stop>) -> Halt {
    stop.try_recv().map_or(Halt::Dropped, Halt::Stop)
}

async fn halt_link<C: Connector>(connector: &C, halt: Halt, config: &LinkConfig) {
    if let Halt::Stop(Stop { disable: true, .. }) = halt {
        let _ = timeout(config.disable_timeout, connector.disable()).await;
    }
}

/// A failed WebSocket handshake. The relay answers an unusable ticket with
/// 401/403 before upgrading on some versions; that is a refusal, not an
/// outage.
fn handshake_error(error: &WsError) -> Ended {
    match error {
        WsError::Http(response) if matches!(response.status().as_u16(), 401 | 403) => Ended::Refused(REFUSED.into()),
        WsError::Http(response) if response.status().as_u16() == 404 => {
            Ended::Fatal("the server does not offer live sharing for this session".into())
        }
        WsError::Http(response) => Ended::Lost(format!("the relay answered HTTP {}", response.status().as_u16())),
        other => Ended::Lost(format!("could not reach the relay: {other}")),
    }
}

const REFUSED: &str = "the relay refused the sharing ticket";

/// Serve one open connection until it ends. Returns how, and whether the relay
/// greeted it (`hello`), which is what tells a replaced connection from one
/// refused by an older relay.
async fn serve(
    socket: Socket,
    commands: &mut mpsc::Receiver<Outbound>,
    stop: &mut oneshot::Receiver<Stop>,
    notify: &Notify,
    seen: &mut RecentIds,
    config: &LinkConfig,
) -> (Ended, bool) {
    let (sink, mut stream) = socket.split();
    let mut writer = Writer::new(sink, config);
    let mut greeted = false;
    let mut idle_timeout = config.idle_timeout;
    let mut period = config.heartbeat;
    let mut heartbeat = interval_at(Instant::now() + period, period);
    let mut last_heard = Instant::now();
    loop {
        tokio::select! {
            // Queued frames go out before a stop is honoured, so what the
            // bridge sent last reaches the browsers before the goodbye.
            biased;
            frame = commands.recv() => {
                let Some(frame) = frame else {
                    let halt = halted(stop);
                    writer.goodbye(&halt).await;
                    return (Ended::Halted(halt), greeted);
                };
                if let Err(reason) = writer.send(&frame).await {
                    return (Ended::Lost(reason), greeted);
                }
            }
            request = &mut *stop => {
                let halt = request.map_or(Halt::Dropped, Halt::Stop);
                writer.goodbye(&halt).await;
                return (Ended::Halted(halt), greeted);
            }
            message = stream.next() => {
                let message = match message {
                    None => return (Ended::Lost("the relay closed the connection".into()), greeted),
                    Some(Err(error)) => return (Ended::Lost(format!("connection lost: {error}")), greeted),
                    Some(Ok(message)) => message,
                };
                last_heard = Instant::now();
                let text = match message {
                    Message::Text(text) => text,
                    Message::Close(frame) => return (closed(frame, greeted), greeted),
                    _ => continue,
                };
                let reply = match protocol::parse_incoming(text.as_str()) {
                    Incoming::Hello { heartbeat_s, idle_timeout_s, browsers } => {
                        greeted = true;
                        if let Some(seconds) = idle_timeout_s.filter(|seconds| *seconds > 0) {
                            idle_timeout = Duration::from_secs(seconds);
                        }
                        if let Some(seconds) = heartbeat_s.filter(|seconds| *seconds > 0)
                            && Duration::from_secs(seconds) != period
                        {
                            period = Duration::from_secs(seconds);
                            heartbeat = interval_at(Instant::now() + period, period);
                        }
                        notify(Inbound::Peers { browsers: browsers.unwrap_or(0) });
                        None
                    }
                    Incoming::PeerState { role, browsers: Some(browsers), .. } if role == "browser" => {
                        notify(Inbound::Peers { browsers });
                        None
                    }
                    Incoming::Input { id, input } => {
                        if seen.insert(&id) {
                            notify(inbound(id, input));
                        }
                        None
                    }
                    Incoming::Invalid { id, kind, reason } => {
                        if seen.insert(&id) {
                            let result = AcceptResult::Rejected;
                            Some(Outbound::Accepted(Accepted { ref_id: id, kind, result, reason: Some(reason) }))
                        } else {
                            None
                        }
                    }
                    Incoming::Ping { ts } => Some(Outbound::Pong(Ping { ts })),
                    Incoming::ServerError { code, message } => {
                        notify(Inbound::Warning { code, message });
                        None
                    }
                    Incoming::PeerState { .. } | Incoming::Ignored => None,
                };
                if let Some(reply) = reply
                    && let Err(reason) = writer.send(&reply).await
                {
                    return (Ended::Lost(reason), greeted);
                }
            }
            _ = heartbeat.tick() => {
                if last_heard.elapsed() >= idle_timeout {
                    return (Ended::Lost("the relay stopped answering".into()), greeted);
                }
                let ping = Outbound::Ping(Ping { ts: serde_json::json!(chrono::Utc::now().to_rfc3339()) });
                if let Err(reason) = writer.send(&ping).await {
                    return (Ended::Lost(reason), greeted);
                }
            }
        }
    }
}

fn inbound(ref_id: String, input: Input) -> Inbound {
    match input {
        Input::Prompt { text } => Inbound::Prompt { ref_id, text },
        Input::Answer { question_id, selected, custom } => Inbound::Answer { ref_id, question_id, selected, custom },
        Input::Approve { approval_id, decision } => {
            Inbound::Approve { ref_id, approval_id, decision: decision.approval() }
        }
        Input::Interrupt => Inbound::Interrupt { ref_id },
        Input::RequestSnapshot => Inbound::SnapshotRequested,
    }
}

/// What a close frame from the relay means for the link.
fn closed(frame: Option<CloseFrame>, greeted: bool) -> Ended {
    let code = frame.as_ref().map(|frame| u16::from(frame.code));
    match code {
        Some(4401) => Ended::Refused(REFUSED.into()),
        Some(4403) => Ended::Fatal("sharing was turned off on the server".into()),
        // A newer connection for this session took over — another terminal
        // resumed it. Reconnecting would only take it back and start a tug of
        // war. An older relay instead refuses a *new* connection with 4409
        // while the old one lingers, before greeting it; that one is retried.
        Some(4409) if greeted => Ended::Fatal("another terminal took over this session".into()),
        Some(4409) => Ended::Lost("an earlier connection is still open on the relay".into()),
        Some(4408) => Ended::Lost("the relay timed out the connection".into()),
        Some(4413) => Ended::Lost("a frame was too large for the relay".into()),
        Some(4429) => Ended::Lost("the relay is rate limiting this session".into()),
        Some(1012) => Ended::Lost("the relay is restarting".into()),
        Some(1013) => Ended::Lost("the relay could not keep up".into()),
        Some(code) => Ended::Lost(format!("the relay closed the connection ({code})")),
        None => Ended::Lost("the relay closed the connection".into()),
    }
}

/// The relay closes a socket that sends more than 20 frames a second (burst
/// 40) with `4429`. The link stays under that by itself, with a margin for
/// frames that leave together after a stall, so a busy turn is delayed by a
/// few milliseconds rather than cut off.
const FRAMES_PER_SECOND: f64 = 16.0;
const FRAME_BURST: f64 = 32.0;
const _: () = assert!(FRAMES_PER_SECOND < 20.0 && FRAME_BURST < 40.0, "stay under the relay's frame limit");

/// A token bucket: how long to wait before the next frame may go.
#[derive(Debug)]
pub(crate) struct Pace {
    tokens: f64,
    capacity: f64,
    rate: f64,
    refilled: Instant,
}

impl Pace {
    pub fn new(rate: f64, capacity: f64) -> Self {
        Self { tokens: capacity, capacity, rate, refilled: Instant::now() }
    }

    /// Take one frame's token, returning how long to wait for it.
    pub fn take(&mut self) -> Duration {
        let now = Instant::now();
        self.tokens = (self.tokens + now.duration_since(self.refilled).as_secs_f64() * self.rate).min(self.capacity);
        self.refilled = now;
        self.tokens -= 1.0;
        if self.tokens >= 0.0 { Duration::ZERO } else { Duration::from_secs_f64(-self.tokens / self.rate) }
    }
}

/// The sending half of one connection: numbers frames, keeps to the relay's
/// frame budget, and gives up on a peer that stops reading.
struct Writer<S> {
    sink: S,
    seq: u64,
    pace: Pace,
    send_timeout: Duration,
}

impl<S> Writer<S>
where
    S: futures_util::Sink<Message, Error = WsError> + Unpin,
{
    fn new(sink: S, config: &LinkConfig) -> Self {
        Self { sink, seq: 0, pace: Pace::new(FRAMES_PER_SECOND, FRAME_BURST), send_timeout: config.send_timeout }
    }

    async fn send(&mut self, frame: &Outbound) -> Result<(), String> {
        let wait = self.pace.take();
        if !wait.is_zero() {
            sleep(wait).await;
        }
        self.seq += 1;
        let id = uuid::Uuid::new_v4().to_string();
        let mut text = protocol::encode(frame, &id, self.seq);
        if text.len() > protocol::MAX_FRAME_BYTES {
            // Clipping should make this impossible; if it happens anyway, say
            // so instead of having the relay close the socket over it.
            let notice = NoticeFrame::new(
                "An update was too large to show live; it appears after a refresh.",
                protocol::Level::Warning,
            );
            text = protocol::encode(&Outbound::Notice(notice), &id, self.seq);
        }
        match timeout(self.send_timeout, self.sink.send(Message::Text(text.into()))).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(format!("connection lost: {error}")),
            Err(_) => Err("the relay stopped accepting data".into()),
        }
    }

    /// Tell the browsers why sharing ended (when it was asked to end), then
    /// close the socket cleanly.
    async fn goodbye(&mut self, halt: &Halt) {
        if let Halt::Stop(request) = halt {
            let notice = Outbound::Notice(NoticeFrame::new(&request.notice, protocol::Level::Info));
            let _ = self.send(&notice).await;
        }
        let frame = CloseFrame { code: CloseCode::Normal, reason: "session closed".into() };
        let _ = timeout(self.send_timeout, self.sink.send(Message::Close(Some(frame)))).await;
        let _ = timeout(self.send_timeout, self.sink.close()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_to_the_cap_within_jitter() {
        let mut backoff = Backoff::new(Duration::from_secs(1), Duration::from_secs(30));
        for attempt in 0..12 {
            let nominal = Duration::from_secs(1 << attempt.min(5)).min(Duration::from_secs(30));
            let delay = backoff.next_delay();
            assert!(delay >= nominal.mul_f64(0.8), "attempt {attempt}: {delay:?} < 80% of {nominal:?}");
            assert!(delay <= nominal.mul_f64(1.2), "attempt {attempt}: {delay:?} > 120% of {nominal:?}");
            assert!(delay <= Duration::from_secs(30));
        }
        backoff.reset();
        assert!(backoff.next_delay() <= Duration::from_millis(1_200));
    }

    #[test]
    fn backoff_jitter_spreads_delays() {
        let delays: std::collections::HashSet<u128> = (0..20)
            .map(|_| Backoff::new(Duration::from_secs(8), Duration::from_secs(30)).next_delay().as_micros())
            .collect();
        assert!(delays.len() > 10, "jitter should vary the delay");
    }

    #[test]
    fn pacing_allows_a_burst_then_spaces_frames_under_the_relay_limit() {
        let mut pace = Pace::new(FRAMES_PER_SECOND, FRAME_BURST);
        for _ in 0..FRAME_BURST as usize {
            assert_eq!(pace.take(), Duration::ZERO, "a burst within capacity goes at once");
        }
        let wait = pace.take();
        assert!(
            wait > Duration::ZERO
                && wait <= Duration::from_secs_f64(1.0 / FRAMES_PER_SECOND) + Duration::from_millis(5)
        );
    }

    #[test]
    fn recent_ids_forget_the_oldest_beyond_capacity() {
        let mut seen = RecentIds::new(64);
        assert!(seen.insert("b1"));
        assert!(!seen.insert("b1"), "a repeat is a duplicate");
        for n in 0..64 {
            assert!(seen.insert(&format!("x{n}")));
        }
        // 64 newer ids pushed b1 out; it would be accepted again.
        assert!(seen.insert("b1"));
        assert!(!seen.insert("x63"));
    }

    #[test]
    fn close_codes_decide_whether_to_reconnect() {
        let frame = |code: u16| Some(CloseFrame { code: CloseCode::from(code), reason: "".into() });
        assert!(matches!(closed(frame(4401), true), Ended::Refused(_)));
        assert!(matches!(closed(frame(4403), true), Ended::Fatal(_)));
        assert!(matches!(closed(frame(4409), true), Ended::Fatal(_)));
        assert!(matches!(closed(frame(4409), false), Ended::Lost(_)));
        for code in [4408, 4413, 4429, 1012, 1000, 1001] {
            assert!(matches!(closed(frame(code), true), Ended::Lost(_)), "{code}");
        }
        assert!(matches!(closed(None, true), Ended::Lost(_)));
    }
}
