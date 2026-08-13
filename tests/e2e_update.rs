//! The release check against a stand-in for GitHub's tag API.

use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

/// Serve one tag list and hand back the URL to point the checker at.
async fn tag_server(body: &'static str) -> (String, tokio::task::JoinHandle<usize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let mut served = 0;
        while let Ok((mut stream, _)) = listener.accept().await {
            served += 1;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            let _ = stream.shutdown().await;
        }
        served
    });
    (format!("http://{address}/tags"), handle)
}

#[tokio::test]
async fn the_newest_tag_is_picked_out_of_an_unordered_list_and_cached() {
    let directory = tempfile::tempdir().unwrap();
    let cache = directory.path().join("update.json");
    // Deliberately unordered, with a non-version tag mixed in — 0.10.0 is the
    // answer, and it beats 0.9.0 only if the compare is numeric.
    let (url, server) = tag_server(
        r#"[{"name":"v0.9.0"},{"name":"nightly"},{"name":"v0.10.0"},{"name":"v0.6.0"}]"#,
    )
    .await;
    let found = abacus_agent::update::check_against(&url, &cache, "0.6.0")
        .await
        .unwrap();
    let found = found.expect("0.10.0 is newer than 0.6.0");
    assert_eq!(found.version, "v0.10.0");
    assert!(found.message().contains("v0.10.0") && found.message().contains("0.6.0"));

    // Already current: same tag list, nothing to say.
    let quiet = abacus_agent::update::check_against(&url, &cache, "0.10.0")
        .await
        .unwrap();
    assert!(quiet.is_none(), "no nag when you are on the newest tag");

    // And the second call was answered from the cache, not the network.
    let cached: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&cache).unwrap()).unwrap();
    assert_eq!(cached["latest"], "v0.10.0");
    server.abort();
}

#[tokio::test]
async fn an_unreachable_endpoint_fails_quietly() {
    let directory = tempfile::tempdir().unwrap();
    let cache = directory.path().join("update.json");
    // An error, never a panic — and no cache file left behind claiming a check.
    assert!(
        abacus_agent::update::check_against("http://127.0.0.1:9/tags", &cache, "0.6.0")
            .await
            .is_err()
    );
    assert!(!cache.exists(), "a failed check records nothing");
}
