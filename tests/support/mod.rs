//! Shared scaffolding for the end-to-end tests: a throwaway project, a
//! scripted model endpoint, and a driver that runs one turn to its end.
#![allow(dead_code)]

use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use abacus_agent::{
    agent::{AgentEvent, DoneReason, TurnOptions, initial_messages, run_turn},
    config::{AbacusPaths, Config},
    provider::Provider,
    services::AgentServices,
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::mpsc,
    task::JoinHandle,
};

/// A temporary directory holding a workspace and an Abacus home beside it.
pub struct Project {
    pub directory: tempfile::TempDir,
    pub workspace: PathBuf,
}

/// A fresh project containing `files`.
pub fn project(files: &[(&str, &str)]) -> Project {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("project");
    std::fs::create_dir(&workspace).unwrap();
    for (name, content) in files {
        std::fs::write(workspace.join(name), content).unwrap();
    }
    Project { workspace: workspace.canonicalize().unwrap(), directory }
}

impl Project {
    pub fn config(&self, address: SocketAddr) -> Config {
        Config {
            profile: "test".into(),
            max_steps: 4,
            yes: true,
            no_session: true,
            ..Config::for_endpoint(
                self.workspace.clone(),
                format!("http://{address}/v1"),
                "test-model".into(),
                AbacusPaths::under(self.directory.path().join("home")),
            )
        }
    }

    pub fn provider(&self, mock: &Mock) -> Provider {
        Provider::new(&self.config(mock.address)).unwrap()
    }

    /// Defaults for tests that only care about one or two options.
    pub fn options(&self) -> TurnOptions {
        TurnOptions {
            max_steps: 4,
            ..TurnOptions::bare(self.workspace.clone(), Arc::new(AgentServices::empty(self.workspace.clone())))
        }
    }

    /// The opening of a conversation: the system prompt and one user message.
    pub fn asks(&self, prompt: &str) -> Vec<Value> {
        let mut messages = initial_messages(&self.workspace);
        messages.push(json!({"role": "user", "content": prompt}));
        messages
    }

    pub fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.workspace.join(name)).unwrap()
    }
}

/// What a mock does with one request.
pub enum Reply {
    Send(String),
    /// Send, then stop serving.
    Last(String),
    /// Accept the connection and never answer it.
    Hang,
}

/// A scripted model endpoint that records every request it receives.
pub struct Mock {
    pub address: SocketAddr,
    requests: Arc<Mutex<Vec<String>>>,
    server: JoinHandle<()>,
}

impl Mock {
    /// Answer successive requests with `replies`, in order.
    pub async fn script(replies: impl IntoIterator<Item = String>) -> Self {
        let mut replies: Vec<Reply> = replies.into_iter().map(Reply::Send).collect();
        if let Some(Reply::Send(last)) = replies.pop() {
            replies.push(Reply::Last(last));
        }
        let mut replies = replies.into_iter();
        Self::answering(move |_| replies.next().unwrap_or(Reply::Hang)).await
    }

    /// Answer each request with whatever `reply` makes of it.
    pub async fn answering(mut reply: impl FnMut(&str) -> Reply + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests: Arc<Mutex<Vec<String>>> = Arc::default();
        let seen = requests.clone();
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_request(&mut stream).await;
                let answer = reply(&request);
                seen.lock().unwrap().push(request);
                match answer {
                    Reply::Send(body) => respond(&mut stream, &body).await,
                    Reply::Last(body) => return respond(&mut stream, &body).await,
                    Reply::Hang => std::future::pending::<()>().await,
                }
            }
        });
        Self { address, requests, server }
    }

    /// Wait for the script to play out, and return what was asked.
    pub async fn finish(self) -> Vec<String> {
        self.server.await.unwrap();
        self.requests.lock().unwrap().clone()
    }

    /// Stop a mock that is holding a connection open on purpose.
    pub fn abort(self) {
        self.server.abort();
    }
}

fn stream_of(delta: Value) -> String {
    format!("data: {}\n\ndata: [DONE]\n\n", json!({"choices": [{"delta": delta}]}))
}

/// A streamed reply of plain text.
pub fn says(text: &str) -> String {
    stream_of(json!({"content": text}))
}

/// A streamed reply of native tool calls: `(id, name, arguments)`.
pub fn calls(calls: &[(&str, &str, Value)]) -> String {
    let calls: Vec<Value> = calls
        .iter()
        .enumerate()
        .map(|(index, (id, name, arguments))| {
            let function = json!({"name": name, "arguments": arguments.to_string()});
            json!({"index": index, "id": id, "function": function})
        })
        .collect();
    stream_of(json!({"tool_calls": calls}))
}

pub async fn respond(stream: &mut TcpStream, body: &str) {
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len(),
    );
    stream.write_all(response.as_bytes()).await.unwrap();
    let _ = stream.shutdown().await;
}

pub async fn read_request(stream: &mut TcpStream) -> String {
    let mut buffer = vec![0_u8; 1_000_000];
    let mut used = 0;
    let mut expected = None;
    loop {
        let read = stream.read(&mut buffer[used..]).await.unwrap();
        used += read;
        if expected.is_none()
            && let Some(header_end) = buffer[..used].windows(4).position(|part| part == b"\r\n\r\n")
        {
            let headers = String::from_utf8_lossy(&buffer[..header_end]).to_ascii_lowercase();
            let length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:")?.trim().parse::<usize>().ok())
                .unwrap_or(0);
            expected = Some(header_end + 4 + length);
        }
        if read == 0 || expected.is_some_and(|expected| used >= expected) {
            break;
        }
    }
    String::from_utf8_lossy(&buffer[..used]).into_owned()
}

/// Run one turn to its end, handing every event before that to `on_event`.
/// A failed turn fails the test.
pub async fn turn(
    provider: Provider,
    messages: Vec<Value>,
    options: TurnOptions,
    mut on_event: impl FnMut(AgentEvent),
) -> (Vec<Value>, DoneReason) {
    let (events, mut receiver) = mpsc::unbounded_channel();
    let agent = tokio::spawn(run_turn(provider, messages, options, events));
    let mut outcome = None;
    while let Some(event) = receiver.recv().await {
        match event {
            AgentEvent::Done { messages, reason } => {
                outcome = Some((messages, reason));
                break;
            }
            AgentEvent::Failed { error, .. } => panic!("agent failed: {error}"),
            event => on_event(event),
        }
    }
    agent.await.unwrap();
    outcome.expect("the turn reports how it ended")
}

/// The text of every message in `messages` that has any.
pub fn contents(messages: &[Value]) -> Vec<&str> {
    messages.iter().filter_map(|message| message["content"].as_str()).collect()
}
