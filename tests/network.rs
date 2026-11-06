use std::{net::SocketAddr, time::Duration};

use reqwest::{Client, StatusCode, header};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    process::{Child, Command},
};
use uuid::Uuid;

const TOKEN: &str = "network-tests-use-a-long-private-bearer-token";

struct Server {
    child: Child,
    address: SocketAddr,
    stderr: tokio::task::JoinHandle<()>,
}

impl Server {
    async fn start(path: &std::path::Path, settings: &[(&str, &str)]) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_nano-db"));
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("NANO_DB_") {
                command.env_remove(key);
            }
        }
        command
            .env("NANO_DB_BIND", "127.0.0.1:0")
            .env("NANO_DB_PATH", path)
            .env("NANO_DB_AUTH_TOKEN", TOKEN)
            .env("NANO_DB_CACHE_BYTES", "1048576")
            .env("NANO_DB_MAX_IN_FLIGHT", "4")
            .env("NANO_DB_QUEUE_TIMEOUT_MS", "5000")
            .env("RUST_LOG", "nano_db=info")
            .stderr(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .kill_on_drop(true);
        for (name, value) in settings {
            command.env(name, value);
        }
        let mut child = command.spawn().unwrap();
        let mut stderr = BufReader::new(child.stderr.take().unwrap());
        let address = tokio::time::timeout(Duration::from_secs(15), async {
            let mut line = String::new();
            loop {
                line.clear();
                assert!(
                    stderr.read_line(&mut line).await.unwrap() > 0,
                    "server exited before readiness"
                );
                if let Ok(log) = serde_json::from_str::<serde_json::Value>(&line)
                    && let Some(address) = log["fields"]["address"].as_str()
                {
                    break address.parse::<SocketAddr>().unwrap();
                }
            }
        })
        .await
        .unwrap();
        let stderr = tokio::spawn(async move {
            tokio::io::copy(&mut stderr, &mut tokio::io::sink())
                .await
                .unwrap();
        });
        Self {
            child,
            address,
            stderr,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.address)
    }

    async fn crash(mut self) {
        self.child.kill().await.unwrap();
        self.stderr.await.unwrap();
    }

    #[cfg(unix)]
    async fn signal_shutdown(&self) {
        let status = Command::new("kill")
            .arg("-TERM")
            .arg(self.child.id().unwrap().to_string())
            .status()
            .await
            .unwrap();
        assert!(status.success());
    }

    #[cfg(unix)]
    async fn finish(mut self) {
        assert!(
            tokio::time::timeout(Duration::from_secs(5), self.child.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
        self.stderr.await.unwrap();
    }
}

fn client() -> Client {
    Client::builder()
        .timeout(Duration::from_secs(10))
        .no_proxy()
        .build()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acknowledged_writes_and_deletes_survive_process_crashes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("persistent.redb");
    let client = client();
    let server = Server::start(&path, &[]).await;
    let raw = " \n{\"n\":9007199254740993,\"big\":1e999999} \t";
    let response = client
        .post(server.url("/v1/objects"))
        .bearer_auth(TOKEN)
        .header(header::CONTENT_TYPE, "application/json")
        .body(raw)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let location = response.headers()[header::LOCATION]
        .to_str()
        .unwrap()
        .to_owned();
    let first_tag = response.headers()[header::ETAG]
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        Uuid::parse_str(location.rsplit('/').next().unwrap())
            .unwrap()
            .get_version_num(),
        7
    );
    assert!(response.bytes().await.unwrap().is_empty());
    let deleted = format!("/v1/objects/{}", Uuid::new_v4());
    let response = client
        .put(server.url(&deleted))
        .bearer_auth(TOKEN)
        .header(header::CONTENT_TYPE, "application/json")
        .body("0")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    response.bytes().await.unwrap();
    let response = client
        .delete(server.url(&deleted))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(!response.headers().contains_key(header::CONTENT_LENGTH));
    assert!(!response.headers().contains_key(header::TRANSFER_ENCODING));
    response.bytes().await.unwrap();
    server.crash().await;

    let server = Server::start(&path, &[]).await;
    let response = client
        .get(server.url(&location))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::ETAG], first_tag);
    assert_eq!(response.bytes().await.unwrap(), raw);
    assert_eq!(
        client
            .get(server.url(&deleted))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    let response = client
        .head(server.url(&location))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_LENGTH],
        raw.len().to_string()
    );
    assert!(response.bytes().await.unwrap().is_empty());
    let response = client
        .get(server.url(&location))
        .bearer_auth(TOKEN)
        .header(header::IF_NONE_MATCH, format!("W/{first_tag}"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    assert!(!response.headers().contains_key(header::CONTENT_LENGTH));
    assert_eq!(
        response.headers()["x-nano-size-bytes"],
        raw.len().to_string()
    );
    assert!(response.bytes().await.unwrap().is_empty());

    let mut tasks = tokio::task::JoinSet::new();
    for number in 1..=16 {
        let client = client.clone();
        let url = server.url(&location);
        let etag = first_tag.clone();
        tasks.spawn(async move {
            let body = format!(r#"{{"winner":{number}}}"#);
            let response = client
                .put(url)
                .bearer_auth(TOKEN)
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::IF_MATCH, etag)
                .body(body.clone())
                .send()
                .await
                .unwrap();
            let status = response.status();
            let tag = response.headers().get(header::ETAG).cloned();
            assert!(
                !response.headers().contains_key(header::CONTENT_LENGTH)
                    || status == StatusCode::PRECONDITION_FAILED
            );
            response.bytes().await.unwrap();
            (status, tag, body)
        });
    }
    let mut winner = None;
    let mut rejected = 0;
    while let Some(result) = tasks.join_next().await {
        let (status, tag, body) = result.unwrap();
        if status == StatusCode::NO_CONTENT {
            assert!(winner.is_none());
            winner = Some((tag.unwrap(), body));
        } else {
            assert_eq!(status, StatusCode::PRECONDITION_FAILED);
            rejected += 1;
        }
    }
    assert_eq!(rejected, 15);
    let (second_tag, body) = winner.unwrap();
    server.crash().await;

    let server = Server::start(&path, &[]).await;
    let response = client
        .get(server.url(&location))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::ETAG], second_tag);
    assert_eq!(response.bytes().await.unwrap(), body);
    let response = client
        .delete(server.url(&location))
        .bearer_auth(TOKEN)
        .header(header::IF_MATCH, first_tag.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
    response.bytes().await.unwrap();
    let response = client
        .delete(server.url(&location))
        .bearer_auth(TOKEN)
        .header(header::IF_MATCH, second_tag.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    response.bytes().await.unwrap();
    let response = client
        .put(server.url(&location))
        .bearer_auth(TOKEN)
        .header(header::CONTENT_TYPE, "application/json")
        .body("null")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let recreated_tag = response.headers()[header::ETAG].clone();
    assert_ne!(recreated_tag, second_tag);
    assert_ne!(recreated_tag, first_tag);
    response.bytes().await.unwrap();
    server.crash().await;

    let server = Server::start(&path, &[]).await;
    let response = client
        .get(server.url(&location))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.headers()[header::ETAG], recreated_tag);
    assert_eq!(response.bytes().await.unwrap(), "null");
    let response = client
        .put(server.url(&location))
        .bearer_auth(TOKEN)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::IF_MATCH, second_tag)
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PRECONDITION_FAILED);
    response.bytes().await.unwrap();
    server.crash().await;
}

#[tokio::test]
async fn slow_bodies_overload_and_chunked_limits_fail_without_creating_documents() {
    let directory = tempfile::tempdir().unwrap();
    let server = Server::start(
        &directory.path().join("limits.redb"),
        &[
            ("NANO_DB_MAX_IN_FLIGHT", "1"),
            ("NANO_DB_MAX_QUEUED", "0"),
            ("NANO_DB_BODY_TIMEOUT_MS", "200"),
            ("NANO_DB_MAX_DOCUMENT_BYTES", "16"),
            ("NANO_DB_CONNECTION_IDLE_TIMEOUT_MS", "1000"),
        ],
    )
    .await;
    let client = client();
    let uri = format!("/v1/objects/{}", Uuid::new_v4());
    let mut socket = TcpStream::connect(server.address).await.unwrap();
    let headers = format!(
        "PUT {uri} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n"
    );
    socket.write_all(headers.as_bytes()).await.unwrap();
    let response = tokio::time::timeout(Duration::from_millis(150), async {
        loop {
            let response = client
                .get(server.url(&uri))
                .bearer_auth(TOKEN)
                .send()
                .await
                .unwrap();
            if response.status() == StatusCode::SERVICE_UNAVAILABLE {
                break response;
            }
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
            response.bytes().await.unwrap();
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(response.headers()[header::RETRY_AFTER], "1");
    response.bytes().await.unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), socket.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    assert!(
        bytes.starts_with(b"HTTP/1.1 408"),
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    assert_eq!(
        client
            .get(server.url(&uri))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );

    let mut socket = TcpStream::connect(server.address).await.unwrap();
    let headers = format!(
        "PUT {uri} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n10\r\n               1\r\n1\r\n \r\n0\r\n\r\n"
    );
    socket.write_all(headers.as_bytes()).await.unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), socket.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    assert!(
        bytes.starts_with(b"HTTP/1.1 413"),
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    assert_eq!(
        client
            .get(server.url(&uri))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    server.crash().await;
}

#[tokio::test]
async fn idle_connections_release_the_connection_limit() {
    let directory = tempfile::tempdir().unwrap();
    let server = Server::start(
        &directory.path().join("connections.redb"),
        &[
            ("NANO_DB_MAX_CONNECTIONS", "1"),
            ("NANO_DB_CONNECTION_IDLE_TIMEOUT_MS", "200"),
        ],
    )
    .await;
    let mut idle = TcpStream::connect(server.address).await.unwrap();
    let mut next = TcpStream::connect(server.address).await.unwrap();
    let request = format!(
        "GET /unknown HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {TOKEN}\r\nConnection: close\r\n\r\n"
    );
    next.write_all(request.as_bytes()).await.unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), next.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    assert!(
        bytes.starts_with(b"HTTP/1.1 404"),
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    let mut byte = [0; 1];
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), idle.read(&mut byte))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    server.crash().await;
}

#[cfg(unix)]
#[tokio::test]
async fn graceful_shutdown_drains_an_accepted_write_before_exiting() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("shutdown.redb");
    let server = Server::start(
        &path,
        &[("NANO_DB_MAX_IN_FLIGHT", "1"), ("NANO_DB_MAX_QUEUED", "0")],
    )
    .await;
    let client = client();
    let uri = format!("/v1/objects/{}", Uuid::new_v4());
    let mut socket = TcpStream::connect(server.address).await.unwrap();
    let headers = format!(
        "PUT {uri} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n["
    );
    socket.write_all(headers.as_bytes()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let response = client
                .get(server.url(&uri))
                .bearer_auth(TOKEN)
                .send()
                .await
                .unwrap();
            let status = response.status();
            response.bytes().await.unwrap();
            if status == StatusCode::SERVICE_UNAVAILABLE {
                break;
            }
            assert_eq!(status, StatusCode::NOT_FOUND);
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    server.signal_shutdown().await;
    socket.write_all(b"]").await.unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), socket.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    assert!(
        bytes.starts_with(b"HTTP/1.1 201"),
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    server.finish().await;
    let server = Server::start(&path, &[]).await;
    let response = client
        .get(server.url(&uri))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.bytes().await.unwrap(), "[]");
    server.signal_shutdown().await;
    server.finish().await;
}
