//! The SearXNG backend against a stand-in instance.

use std::sync::Arc;

use abacus_agent::web::{SearchBackend, SearchSettings};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Serve one canned response, recording the request line it was asked for.
async fn instance(
    body: &'static str,
    content_type: &'static str,
) -> (String, Arc<std::sync::Mutex<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let seen: Arc<std::sync::Mutex<String>> = Arc::default();
    let probe = seen.clone();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let mut buffer = vec![0_u8; 8192];
            let read = stream.read(&mut buffer).await.unwrap_or(0);
            *probe.lock().unwrap() = String::from_utf8_lossy(&buffer[..read]).to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.shutdown().await;
        }
    });
    (format!("http://{address}"), seen)
}

#[tokio::test]
async fn a_searxng_instance_answers_and_is_asked_for_json() {
    let body = r#"{"results":[
        {"title":"signal-hook","url":"https://vorner.github.io/signal-hook.html","content":"handling unix signals"},
        {"title":"tokio::signal","url":"https://docs.rs/tokio/latest/tokio/signal/","content":"async signal handling"},
        {"title":"third","url":"https://example.com/3","content":"third result"}
    ]}"#;
    let (url, seen) = instance(body, "application/json").await;

    let settings = SearchSettings {
        instance_url: Some(format!("{url}/")), // trailing slash, as people paste it
        ..SearchSettings::default()
    };
    let config = settings.resolve_with(|_| None);
    // A configured instance is chosen over every keyless fallback.
    assert_eq!(config.backend, SearchBackend::Searxng);

    let rendered = config.search("tokio signal handler", 2).await.unwrap();
    assert!(rendered.contains("signal-hook"), "{rendered}");
    assert!(
        rendered.contains("https://docs.rs/tokio/latest/tokio/signal/"),
        "{rendered}"
    );
    assert!(
        !rendered.contains("third result"),
        "max_results is honoured: {rendered}"
    );

    let request = seen.lock().unwrap().clone();
    assert!(request.contains("format=json"), "asks for JSON: {request}");
    assert!(
        request.contains("GET /search?"),
        "no doubled slash: {request}"
    );
}

/// SearXNG ships with the JSON format disabled, so this is the mistake users
/// will actually hit. The error has to name the fix, not just fail to parse.
#[tokio::test]
async fn an_instance_without_json_enabled_says_exactly_that() {
    let (url, _seen) = instance(
        "<!DOCTYPE html><html><body>results</body></html>",
        "text/html",
    )
    .await;
    let settings = SearchSettings {
        instance_url: Some(url),
        ..SearchSettings::default()
    };
    let error = settings
        .resolve_with(|_| None)
        .search("anything", 3)
        .await
        .expect_err("HTML is not a usable answer");
    let message = format!("{error:#}");
    assert!(message.contains("settings.yml"), "names the fix: {message}");
    assert!(message.contains("json"), "{message}");
}
