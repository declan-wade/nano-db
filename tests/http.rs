use std::{collections::HashMap, sync::Arc, time::Duration};

use axum::{
    Router,
    body::{Body, Bytes, to_bytes},
    http::{HeaderMap, HeaderValue, Request, StatusCode, header},
};
use nano_db::{Config, Store, app};
use tempfile::TempDir;
use tower::ServiceExt;
use uuid::Uuid;

const TOKEN: &str = "test-token-with-at-least-thirty-two-bytes";

struct Fixture {
    router: Router,
    _directory: TempDir,
}

fn fixture(mut config: Config) -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    config.path = directory.path().join("test.redb");
    config.auth_token = Some(TOKEN.into());
    config.cache_bytes = 1024 * 1024;
    let store = Arc::new(Store::open(&config.path, config.cache_bytes).unwrap());
    Fixture {
        router: app(config, store).unwrap(),
        _directory: directory,
    }
}

fn request(method: &str, uri: &str, bytes: impl Into<Body>) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"));
    if matches!(method, "PUT" | "POST") {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
    }
    builder.body(bytes.into()).unwrap()
}

fn conditional(mut request: Request<Body>, name: header::HeaderName, value: &str) -> Request<Body> {
    request.headers_mut().insert(name, value.parse().unwrap());
    request
}

async fn send(router: &Router, request: Request<Body>) -> (StatusCode, HeaderMap, Bytes) {
    let response = router.clone().oneshot(request).await.unwrap();
    let (parts, body) = response.into_parts();
    (
        parts.status,
        parts.headers,
        to_bytes(body, 64 * 1024 * 1024).await.unwrap(),
    )
}

fn tag(headers: &HeaderMap) -> String {
    headers[header::ETAG].to_str().unwrap().to_owned()
}

fn error_code(body: &Bytes) -> String {
    serde_json::from_slice::<serde_json::Value>(body).unwrap()["error"]["code"]
        .as_str()
        .unwrap()
        .into()
}

#[tokio::test]
async fn generated_ids_round_trip_exact_bytes_and_metadata() {
    let fixture = fixture(Config::default());
    let raw = " \n{\"big\":9007199254740993,\"huge\":1e999999,\"tiny\":0.000000000000000000001} \t";
    let (status, headers, body) = send(&fixture.router, request("POST", "/v1/objects", raw)).await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(body.is_empty());
    let location = headers[header::LOCATION].to_str().unwrap();
    let id = Uuid::parse_str(location.rsplit('/').next().unwrap()).unwrap();
    assert_eq!(id.get_version_num(), 7);
    let first_tag = tag(&headers);
    let created = headers["x-nano-created-at-ms"].clone();

    let (status, get_headers, body) = send(&fixture.router, request("GET", location, "")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, raw);
    assert_eq!(get_headers[header::CONTENT_TYPE], "application/json");
    assert_eq!(get_headers[header::CONTENT_LENGTH], raw.len().to_string());
    assert_eq!(tag(&get_headers), first_tag);

    let (status, head_headers, body) = send(&fixture.router, request("HEAD", location, "")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.is_empty());
    assert_eq!(head_headers, get_headers);

    let (status, headers, body) = send(
        &fixture.router,
        request("PUT", location, r#"{"replacement":true}"#),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(body.is_empty());
    assert!(!headers.contains_key(header::CONTENT_LENGTH));
    assert!(!headers.contains_key(header::LOCATION));
    assert_ne!(tag(&headers), first_tag);
    assert_eq!(headers["x-nano-created-at-ms"], created);
    let (_, _, body) = send(&fixture.router, request("GET", location, "")).await;
    assert_eq!(body, r#"{"replacement":true}"#);

    for _ in 0..2 {
        let (status, headers, body) = send(&fixture.router, request("DELETE", location, "")).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(!headers.contains_key(header::CONTENT_LENGTH));
        assert!(body.is_empty());
    }
    let (status, _, body) = send(&fixture.router, request("GET", location, "")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(error_code(&body), "not_found");
    let (status, _, body) = send(&fixture.router, request("HEAD", location, "")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.is_empty());
}

#[tokio::test]
async fn caller_ids_are_validated_and_locations_are_canonical() {
    let fixture = fixture(Config::default());
    let id = Uuid::new_v4();
    let uri = format!("/v1/objects/{}", id.to_string().to_uppercase());
    let (status, headers, _) = send(&fixture.router, request("PUT", &uri, "null")).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(headers[header::LOCATION], format!("/v1/objects/{id}"));
    let (status, _, body) = send(
        &fixture.router,
        request("GET", &format!("/v1/objects/{}", id.simple()), ""),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "null");
    for uri in ["/v1/objects/not-a-uuid", "/v1/objects/%FF"] {
        let (status, _, body) = send(&fixture.router, request("PUT", uri, "{}")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(error_code(&body), "invalid_uuid");
    }
    let (status, _, body) = send(
        &fixture.router,
        request("POST", "/v1/objects?guid=anything", "{}"),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error_code(&body), "unsupported_query");
    let (status, _, body) = send(
        &fixture.router,
        conditional(
            request("POST", "/v1/objects", "{}"),
            header::IF_NONE_MATCH,
            "*",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error_code(&body), "unsupported_precondition");
}

#[tokio::test]
async fn etag_preconditions_cover_reads_writes_delete_and_recreation() {
    let fixture = fixture(Config::default());
    let uri = format!("/v1/objects/{}", Uuid::new_v4());
    let (status, _, _) = send(
        &fixture.router,
        conditional(request("PUT", &uri, "{}"), header::IF_MATCH, "*"),
    )
    .await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
    let (status, headers, _) = send(
        &fixture.router,
        conditional(request("PUT", &uri, "123"), header::IF_NONE_MATCH, "*"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let original = tag(&headers);
    let weak = format!("W/{original}");
    for method in ["GET", "HEAD"] {
        let (status, headers, body) = send(
            &fixture.router,
            conditional(request(method, &uri, ""), header::IF_NONE_MATCH, &weak),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_MODIFIED);
        assert_eq!(tag(&headers), original);
        assert!(!headers.contains_key(header::CONTENT_LENGTH));
        assert_eq!(headers["x-nano-size-bytes"], "3");
        assert!(body.is_empty());
    }
    for (method, value) in [("PUT", "456"), ("DELETE", "")] {
        let (status, _, _) = send(
            &fixture.router,
            conditional(request(method, &uri, value), header::IF_NONE_MATCH, &weak),
        )
        .await;
        assert_eq!(status, StatusCode::PRECONDITION_FAILED);
    }
    let (status, _, _) = send(
        &fixture.router,
        conditional(request("GET", &uri, ""), header::IF_MATCH, &weak),
    )
    .await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
    let mut both = conditional(request("GET", &uri, ""), header::IF_MATCH, r#""wrong""#);
    both.headers_mut()
        .insert(header::IF_NONE_MATCH, "*".parse().unwrap());
    let (status, _, _) = send(&fixture.router, both).await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
    let list = format!(r#""other,tag", {original}, "#);
    let (status, _, body) = send(
        &fixture.router,
        conditional(request("GET", &uri, ""), header::IF_MATCH, &list),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "123");
    let (status, _, _) = send(
        &fixture.router,
        conditional(request("DELETE", &uri, ""), header::IF_MATCH, &original),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, _) = send(
        &fixture.router,
        conditional(request("GET", &uri, ""), header::IF_MATCH, "*"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = send(
        &fixture.router,
        conditional(request("DELETE", &uri, ""), header::IF_MATCH, "*"),
    )
    .await;
    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
    let (status, _, _) = send(
        &fixture.router,
        conditional(request("DELETE", &uri, ""), header::IF_NONE_MATCH, "*"),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, headers, _) = send(&fixture.router, request("PUT", &uri, "789")).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_ne!(tag(&headers), original);
    for method in ["PUT", "DELETE"] {
        let (status, _, _) = send(
            &fixture.router,
            conditional(request(method, &uri, "999"), header::IF_MATCH, &original),
        )
        .await;
        assert_eq!(status, StatusCode::PRECONDITION_FAILED);
    }
    let (_, _, body) = send(&fixture.router, request("GET", &uri, "")).await;
    assert_eq!(body, "789");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_compare_and_swap_has_exactly_one_winner() {
    let fixture = fixture(Config {
        max_in_flight: 8,
        queue_timeout: Duration::from_secs(5),
        ..Config::default()
    });
    let uri = format!("/v1/objects/{}", Uuid::new_v4());
    let (_, headers, _) = send(&fixture.router, request("PUT", &uri, "0")).await;
    let original = tag(&headers);
    let mut tasks = tokio::task::JoinSet::new();
    for index in 1..=32 {
        let router = fixture.router.clone();
        let uri = uri.clone();
        let original = original.clone();
        tasks.spawn(async move {
            let body = index.to_string();
            let (status, headers, _) = send(
                &router,
                conditional(
                    request("PUT", &uri, body.clone()),
                    header::IF_MATCH,
                    &original,
                ),
            )
            .await;
            (status, headers, body)
        });
    }
    let mut winner = None;
    let mut rejected = 0;
    while let Some(result) = tasks.join_next().await {
        let (status, headers, body) = result.unwrap();
        if status == StatusCode::NO_CONTENT {
            assert!(winner.is_none());
            assert_eq!(headers["x-nano-revision"], "2");
            winner = Some(body);
        } else {
            assert_eq!(status, StatusCode::PRECONDITION_FAILED);
            rejected += 1;
        }
    }
    assert_eq!(rejected, 31);
    let (_, _, body) = send(&fixture.router, request("GET", &uri, "")).await;
    assert_eq!(body, winner.unwrap());

    let uri = format!("/v1/objects/{}", Uuid::new_v4());
    for _ in 0..16 {
        let router = fixture.router.clone();
        let uri = uri.clone();
        tasks.spawn(async move {
            let (status, headers, _) = send(
                &router,
                conditional(request("PUT", &uri, "{}"), header::IF_NONE_MATCH, "*"),
            )
            .await;
            (status, headers, String::new())
        });
    }
    let mut created = 0;
    while let Some(result) = tasks.join_next().await {
        let (status, _, _) = result.unwrap();
        if status == StatusCode::CREATED {
            created += 1;
        } else {
            assert_eq!(status, StatusCode::PRECONDITION_FAILED);
        }
    }
    assert_eq!(created, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_readers_observe_matching_document_and_metadata_snapshots() {
    let fixture = fixture(Config {
        max_queued: 128,
        queue_timeout: Duration::from_secs(5),
        ..Config::default()
    });
    let uri = format!("/v1/objects/{}", Uuid::new_v4());
    let (_, headers, _) = send(&fixture.router, request("PUT", &uri, "0")).await;
    let mut versions = HashMap::from([(tag(&headers), Bytes::from_static(b"0"))]);
    let mut reads = Vec::new();
    let mut tasks = tokio::task::JoinSet::new();
    for index in 1..=24 {
        let router = fixture.router.clone();
        let uri = uri.clone();
        tasks.spawn(async move {
            let bytes = format!(r#"{{"value":"{}"}}"#, "x".repeat(index));
            let (status, headers, _) = send(&router, request("PUT", &uri, bytes.clone())).await;
            assert_eq!(status, StatusCode::NO_CONTENT);
            (true, headers, Bytes::from(bytes))
        });
    }
    for _ in 0..48 {
        let router = fixture.router.clone();
        let uri = uri.clone();
        tasks.spawn(async move {
            let (status, headers, bytes) = send(&router, request("GET", &uri, "")).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(headers[header::CONTENT_LENGTH], bytes.len().to_string());
            (false, headers, bytes)
        });
    }
    while let Some(result) = tasks.join_next().await {
        let (write, headers, bytes) = result.unwrap();
        if write {
            assert!(versions.insert(tag(&headers), bytes).is_none());
        } else {
            reads.push((tag(&headers), bytes));
        }
    }
    for (tag, bytes) in reads {
        assert_eq!(versions[&tag], bytes);
    }
}

#[tokio::test]
async fn size_depth_json_and_media_failures_are_explicit_and_do_not_mutate() {
    let fixture = fixture(Config {
        max_document_bytes: 16,
        max_depth: 2,
        ..Config::default()
    });
    let uri = format!("/v1/objects/{}", Uuid::new_v4());
    let exact = format!("{}1", " ".repeat(15));
    let (status, original, _) = send(&fixture.router, request("PUT", &uri, exact.clone())).await;
    assert_eq!(status, StatusCode::CREATED);
    for (input, expected) in [
        ("                 1", StatusCode::PAYLOAD_TOO_LARGE),
        (r#"{"a":1,"a":2}"#, StatusCode::BAD_REQUEST),
        ("[[[]]]", StatusCode::BAD_REQUEST),
        ("[1,]", StatusCode::BAD_REQUEST),
        ("", StatusCode::BAD_REQUEST),
    ] {
        let (status, _, body) = send(&fixture.router, request("PUT", &uri, input)).await;
        assert_eq!(status, expected, "{input}");
        assert!(matches!(
            error_code(&body).as_str(),
            "invalid_json" | "document_too_large"
        ));
    }
    for content_type in [
        None,
        Some("text/plain"),
        Some("application/json; charset=latin1"),
        Some("application/json; charset=utf-8; charset=latin1"),
    ] {
        let mut req = request("PUT", &uri, "0");
        req.headers_mut().remove(header::CONTENT_TYPE);
        if let Some(value) = content_type {
            req.headers_mut()
                .insert(header::CONTENT_TYPE, value.parse().unwrap());
        }
        let (status, _, body) = send(&fixture.router, req).await;
        assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(error_code(&body), "unsupported_media_type");
    }
    let mut req = request("PUT", &uri, "0");
    req.headers_mut()
        .insert(header::CONTENT_ENCODING, "gzip".parse().unwrap());
    assert_eq!(
        send(&fixture.router, req).await.0,
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
    let mut req = request("PUT", &uri, "0");
    req.headers_mut()
        .insert(header::CONTENT_RANGE, "bytes 0-0/1".parse().unwrap());
    assert_eq!(send(&fixture.router, req).await.0, StatusCode::BAD_REQUEST);
    let mut req = request("PUT", &uri, "");
    req.headers_mut()
        .insert(header::CONTENT_LENGTH, "17".parse().unwrap());
    assert_eq!(
        send(&fixture.router, req).await.0,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let (status, headers, body) = send(&fixture.router, request("GET", &uri, "")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(tag(&headers), tag(&original));
    assert_eq!(body, exact);
    let (status, _, _) = send(&fixture.router, request("PUT", &uri, "[[0]]")).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    for content_type in [
        "application/json; charset=UTF-8",
        "application/json; charset=\"utf-8\"",
    ] {
        let mut req = request("PUT", &uri, "[[0]]");
        req.headers_mut()
            .insert(header::CONTENT_TYPE, content_type.parse().unwrap());
        assert_eq!(send(&fixture.router, req).await.0, StatusCode::NO_CONTENT);
    }
}

#[tokio::test]
async fn authentication_routes_and_conditional_errors_have_json_contracts() {
    let fixture = fixture(Config::default());
    let uri = format!("/v1/objects/{}", Uuid::new_v4());
    for method in ["GET", "HEAD", "PUT", "POST", "DELETE"] {
        let mut req = request(method, &uri, "{}");
        req.headers_mut().remove(header::AUTHORIZATION);
        let (status, headers, body) = send(&fixture.router, req).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(
            headers[header::WWW_AUTHENTICATE],
            r#"Bearer realm="nano-db""#
        );
        if method != "HEAD" {
            assert_eq!(error_code(&body), "unauthorized");
        }
    }
    let mut req = request("GET", &uri, "");
    req.headers_mut().append(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer wrong"),
    );
    assert_eq!(send(&fixture.router, req).await.0, StatusCode::UNAUTHORIZED);
    let mut req = request("PUT", &uri, "{}");
    req.headers_mut().insert(
        header::AUTHORIZATION,
        format!("bEaReR   {TOKEN}").parse().unwrap(),
    );
    assert_eq!(send(&fixture.router, req).await.0, StatusCode::CREATED);
    let (status, headers, body) = send(&fixture.router, request("PATCH", &uri, "{}")).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert!(headers[header::ALLOW].to_str().unwrap().contains("PUT"));
    assert_eq!(error_code(&body), "method_not_allowed");
    let (status, _, body) = send(&fixture.router, request("GET", "/unknown", "")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(error_code(&body), "not_found");
    for value in [r#"W/"unterminated"#, "* , \"tag\""] {
        let (status, _, body) = send(
            &fixture.router,
            conditional(request("GET", &uri, ""), header::IF_MATCH, value),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(error_code(&body), "invalid_precondition");
    }
    let (status, _, body) = send(
        &fixture.router,
        conditional(
            request("GET", &uri, ""),
            header::IF_MODIFIED_SINCE,
            "Wed, 21 Oct 2015 07:28:00 GMT",
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error_code(&body), "unsupported_precondition");
}

#[tokio::test]
async fn response_lifetimes_and_waiting_requests_are_bounded() {
    let fixture = fixture(Config {
        max_in_flight: 1,
        max_queued: 1,
        queue_timeout: Duration::from_millis(100),
        ..Config::default()
    });
    let uri = format!("/v1/objects/{}", Uuid::new_v4());
    send(&fixture.router, request("PUT", &uri, "{}")).await;
    let held = fixture
        .router
        .clone()
        .oneshot(request("GET", &uri, ""))
        .await
        .unwrap();
    let router = fixture.router.clone();
    let queued_uri = uri.clone();
    let queued = tokio::spawn(async move { send(&router, request("GET", &queued_uri, "")).await });
    tokio::time::sleep(Duration::from_millis(20)).await;
    let (status, headers, body) = send(&fixture.router, request("GET", &uri, "")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(headers[header::RETRY_AFTER], "1");
    assert_eq!(error_code(&body), "overloaded");
    assert_eq!(queued.await.unwrap().0, StatusCode::SERVICE_UNAVAILABLE);
    drop(held);
    assert_eq!(
        send(&fixture.router, request("GET", &uri, "")).await.0,
        StatusCode::OK
    );
}
