use std::{
    error::Error,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use axum::{
    Extension, Router,
    body::{Body, Bytes, to_bytes},
    extract::{Path, Request, State, rejection::PathRejection},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use http_body::{Frame, SizeHint};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;

use crate::{
    Config, Store,
    conditions::{Conditions, Decision},
    error::ApiError,
    json,
    store::{Document, Metadata, Mutation},
};

#[derive(Clone)]
struct AppState {
    store: Arc<Store>,
    auth_hash: Option<[u8; 32]>,
    admission: Arc<Admission>,
    max_document_bytes: usize,
    max_depth: usize,
    queue_timeout: Duration,
    body_timeout: Duration,
}

struct Admission {
    slots: Arc<Semaphore>,
    active: Arc<Semaphore>,
}

struct Lease {
    _active: OwnedSemaphorePermit,
    _slot: OwnedSemaphorePermit,
}

pub fn app(config: Config, store: Arc<Store>) -> anyhow::Result<Router> {
    config.validate()?;
    let state = AppState {
        store,
        auth_hash: config
            .auth_token
            .as_ref()
            .map(|token| Sha256::digest(token.as_bytes()).into()),
        admission: Arc::new(Admission {
            slots: Arc::new(Semaphore::new(config.max_in_flight + config.max_queued)),
            active: Arc::new(Semaphore::new(config.max_in_flight)),
        }),
        max_document_bytes: config.max_document_bytes,
        max_depth: config.max_depth,
        queue_timeout: config.queue_timeout,
        body_timeout: config.body_timeout,
    };
    Ok(Router::new()
        .route(
            "/v1/objects",
            post(create).fallback(collection_method_not_allowed),
        )
        .route(
            "/v1/objects/{guid}",
            get(fetch)
                .head(head)
                .put(replace)
                .delete(remove)
                .fallback(object_method_not_allowed),
        )
        .fallback(not_found)
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state))
}

async fn guard(State(state): State<AppState>, mut request: Request, next: Next) -> Response {
    if !authorized(&state, request.headers()) {
        return ApiError::unauthorized().into_response();
    }
    if request.uri().query().is_some() {
        return ApiError::bad_request(
            "unsupported_query",
            "query parameters are reserved; none are supported in v1",
        )
        .into_response();
    }
    let slot = match state.admission.slots.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => return ApiError::overloaded().into_response(),
    };
    let active = match tokio::time::timeout(
        state.queue_timeout,
        state.admission.active.clone().acquire_owned(),
    )
    .await
    {
        Ok(Ok(permit)) => permit,
        Ok(Err(error)) => return ApiError::internal(error.into()).into_response(),
        Err(_) => return ApiError::overloaded().into_response(),
    };
    let lease = Arc::new(Lease {
        _active: active,
        _slot: slot,
    });
    request.extensions_mut().insert(lease.clone());
    let response = next.run(request).await;
    let (mut parts, body) = response.into_parts();
    let omit_length = matches!(
        parts.status,
        StatusCode::NO_CONTENT | StatusCode::NOT_MODIFIED
    );
    if omit_length {
        parts.headers.remove(header::CONTENT_LENGTH);
        parts.headers.remove(header::TRANSFER_ENCODING);
    }
    parts.headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    Response::from_parts(
        parts,
        Body::new(LeasedBody {
            inner: body,
            _lease: lease,
            omit_length,
        }),
    )
}

fn authorized(state: &AppState, headers: &HeaderMap) -> bool {
    let Some(expected) = state.auth_hash else {
        return true;
    };
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let Some(value) = values.next() else {
        return false;
    };
    if values.next().is_some() {
        return false;
    }
    let Ok(value) = value.to_str() else {
        return false;
    };
    let Some((scheme, credential)) = value.split_once(' ') else {
        return false;
    };
    let credential = credential.trim_start_matches(' ');
    if !scheme.eq_ignore_ascii_case("bearer") || credential.len() > 1024 {
        return false;
    }
    let actual: [u8; 32] = Sha256::digest(credential.as_bytes()).into();
    bool::from(expected.ct_eq(&actual))
}

struct LeasedBody {
    inner: Body,
    _lease: Arc<Lease>,
    omit_length: bool,
}

impl http_body::Body for LeasedBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        Pin::new(&mut self.inner).poll_frame(context)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        // Preserve length omission on bodyless statuses instead of Axum synthesizing zero.
        if self.omit_length {
            SizeHint::default()
        } else {
            self.inner.size_hint()
        }
    }
}

async fn blocking<T: Send + 'static>(
    lease: Arc<Lease>,
    work: impl FnOnce() -> Result<T, ApiError> + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(move || {
        // Cancellation must not free capacity while the non-cancellable transaction still runs.
        let _lease = lease;
        work()
    })
    .await
    .map_err(|error| ApiError::internal(error.into()))?
}

async fn create(
    State(state): State<AppState>,
    Extension(lease): Extension<Arc<Lease>>,
    request: Request,
) -> Result<Response, ApiError> {
    let conditions = Conditions::parse(request.headers())?;
    if !conditions.is_empty() {
        return Err(ApiError::bad_request(
            "unsupported_precondition",
            "POST targets the collection; use conditional PUT for caller-assigned IDs",
        ));
    }
    let bytes = read_document(&state, request).await?;
    let store = state.store.clone();
    let max_depth = state.max_depth;
    let (id, mutation) = blocking(lease, move || {
        validate_document(&bytes, max_depth)?;
        for _ in 0..4 {
            let id = Uuid::now_v7();
            let mutation = store
                .put(id, &bytes, &Conditions::create_only())
                .map_err(ApiError::internal)?;
            if !matches!(mutation, Mutation::PreconditionFailed) {
                return Ok((id, mutation));
            }
        }
        Err(ApiError::internal(anyhow::anyhow!(
            "generated UUID collisions exhausted"
        )))
    })
    .await?;
    written_response(&state.store, id, mutation)
}

async fn replace(
    State(state): State<AppState>,
    Extension(lease): Extension<Arc<Lease>>,
    path: Result<Path<String>, PathRejection>,
    request: Request,
) -> Result<Response, ApiError> {
    let id = parse_id(path)?;
    let conditions = Conditions::parse(request.headers())?;
    let bytes = read_document(&state, request).await?;
    let store = state.store.clone();
    let max_depth = state.max_depth;
    let mutation = blocking(lease, move || {
        validate_document(&bytes, max_depth)?;
        store
            .put(id, &bytes, &conditions)
            .map_err(ApiError::internal)
    })
    .await?;
    written_response(&state.store, id, mutation)
}

async fn fetch(
    State(state): State<AppState>,
    Extension(lease): Extension<Arc<Lease>>,
    path: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    retrieve(state, lease, path, headers, false).await
}

async fn head(
    State(state): State<AppState>,
    Extension(lease): Extension<Arc<Lease>>,
    path: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    retrieve(state, lease, path, headers, true).await
}

async fn retrieve(
    state: AppState,
    lease: Arc<Lease>,
    path: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
    head: bool,
) -> Result<Response, ApiError> {
    let id = parse_id(path)?;
    let conditions = Conditions::parse(&headers)?;
    let store = state.store.clone();
    let result = blocking(lease, move || {
        store
            .read(id, !head, &conditions)
            .map_err(ApiError::internal)?
            .ok_or_else(|| {
                ApiError::new(StatusCode::NOT_FOUND, "not_found", "object does not exist")
            })
    })
    .await?;
    representation_response(&state.store, result.0, head, result.1)
}

async fn remove(
    State(state): State<AppState>,
    Extension(lease): Extension<Arc<Lease>>,
    path: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let id = parse_id(path)?;
    let conditions = Conditions::parse(&headers)?;
    let store = state.store.clone();
    let mutation = blocking(lease, move || {
        store.delete(id, &conditions).map_err(ApiError::internal)
    })
    .await?;
    match mutation {
        Mutation::Deleted => Ok(StatusCode::NO_CONTENT.into_response()),
        Mutation::PreconditionFailed => Err(ApiError::precondition_failed()),
        Mutation::Written { .. } => Err(ApiError::internal(anyhow::anyhow!(
            "unexpected delete result"
        ))),
    }
}

fn parse_id(path: Result<Path<String>, PathRejection>) -> Result<Uuid, ApiError> {
    let Path(raw) =
        path.map_err(|_| ApiError::bad_request("invalid_uuid", "object ID must be a valid UUID"))?;
    Uuid::parse_str(&raw)
        .map_err(|_| ApiError::bad_request("invalid_uuid", "object ID must be a valid UUID"))
}

async fn read_document(state: &AppState, request: Request) -> Result<Bytes, ApiError> {
    validate_media_type(request.headers())?;
    if request.headers().contains_key(header::CONTENT_RANGE) {
        return Err(ApiError::bad_request(
            "unsupported_partial_write",
            "PUT and POST accept complete JSON values only",
        ));
    }
    if let Some(length) = request.headers().get(header::CONTENT_LENGTH) {
        let length = length
            .to_str()
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or_else(|| ApiError::bad_request("invalid_body", "invalid Content-Length"))?;
        if length > state.max_document_bytes as u64 {
            return Err(too_large());
        }
    }
    match tokio::time::timeout(
        state.body_timeout,
        to_bytes(request.into_body(), state.max_document_bytes),
    )
    .await
    {
        Ok(Ok(bytes)) => Ok(bytes),
        Ok(Err(error))
            if error
                .source()
                .is_some_and(|source| source.is::<http_body_util::LengthLimitError>()) =>
        {
            Err(too_large())
        }
        Ok(Err(error)) => {
            tracing::warn!(error = %error, "request body could not be read");
            Err(ApiError::bad_request(
                "invalid_body",
                "request body could not be read",
            ))
        }
        Err(_) => Err(ApiError::new(
            StatusCode::REQUEST_TIMEOUT,
            "request_timeout",
            "request body deadline exceeded",
        )),
    }
}

fn validate_media_type(headers: &HeaderMap) -> Result<(), ApiError> {
    let unsupported = || {
        ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            "use application/json with UTF-8 and no content encoding",
        )
    };
    let mut values = headers.get_all(header::CONTENT_TYPE).iter();
    let value = values.next().ok_or_else(unsupported)?;
    if values.next().is_some() {
        return Err(unsupported());
    }
    let media_type = value
        .to_str()
        .ok()
        .and_then(|value| value.parse::<mime::Mime>().ok())
        .ok_or_else(unsupported)?;
    if media_type.type_() != mime::APPLICATION
        || media_type.subtype() != mime::JSON
        || media_type.params().any(|(name, value)| {
            name == mime::CHARSET && !value.as_str().eq_ignore_ascii_case("utf-8")
        })
    {
        return Err(unsupported());
    }
    let mut encodings = headers.get_all(header::CONTENT_ENCODING).iter();
    if let Some(encoding) = encodings.next()
        && (encodings.next().is_some() || !encoding.as_bytes().eq_ignore_ascii_case(b"identity"))
    {
        return Err(unsupported());
    }
    Ok(())
}

fn validate_document(bytes: &[u8], max_depth: usize) -> Result<(), ApiError> {
    json::validate(bytes, max_depth).map_err(|error| {
        ApiError::bad_request(
            "invalid_json",
            format!("{} at byte {}", error.message, error.offset),
        )
    })
}

fn too_large() -> ApiError {
    ApiError::new(
        StatusCode::PAYLOAD_TOO_LARGE,
        "document_too_large",
        "document exceeds the configured byte limit",
    )
}

fn representation_response(
    store: &Store,
    document: Document,
    head: bool,
    decision: Decision,
) -> Result<Response, ApiError> {
    if decision == Decision::Failed {
        return Err(ApiError::precondition_failed());
    }
    let status = if decision == Decision::NotModified {
        StatusCode::NOT_MODIFIED
    } else {
        StatusCode::OK
    };
    let body = if head || status == StatusCode::NOT_MODIFIED {
        Body::empty()
    } else {
        Body::from(
            document
                .bytes
                .ok_or_else(|| ApiError::internal(anyhow::anyhow!("missing document bytes")))?,
        )
    };
    let mut response = Response::builder()
        .status(status)
        .header(header::CACHE_CONTROL, "no-cache")
        .body(body)
        .map_err(|error| ApiError::internal(error.into()))?;
    if status == StatusCode::OK {
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        response.headers_mut().insert(
            header::CONTENT_LENGTH,
            HeaderValue::from(document.metadata.size),
        );
    }
    metadata_headers(&mut response, store, document.metadata)?;
    Ok(response)
}

fn written_response(store: &Store, id: Uuid, mutation: Mutation) -> Result<Response, ApiError> {
    match mutation {
        Mutation::Written { created, metadata } => {
            let mut response = Response::builder()
                .status(if created {
                    StatusCode::CREATED
                } else {
                    StatusCode::NO_CONTENT
                })
                .header(header::CACHE_CONTROL, "no-store")
                .body(Body::empty())
                .map_err(|error| ApiError::internal(error.into()))?;
            metadata_headers(&mut response, store, metadata)?;
            if created {
                response.headers_mut().insert(
                    header::LOCATION,
                    HeaderValue::from_str(&format!("/v1/objects/{id}"))
                        .map_err(|error| ApiError::internal(error.into()))?,
                );
            }
            Ok(response)
        }
        Mutation::PreconditionFailed => Err(ApiError::precondition_failed()),
        Mutation::Deleted => Err(ApiError::internal(anyhow::anyhow!(
            "unexpected write result"
        ))),
    }
}

fn metadata_headers(
    response: &mut Response,
    store: &Store,
    metadata: Metadata,
) -> Result<(), ApiError> {
    for (name, value) in [
        (header::ETAG, store.etag(metadata)),
        (
            header::HeaderName::from_static("x-nano-size-bytes"),
            metadata.size.to_string(),
        ),
        (
            header::HeaderName::from_static("x-nano-revision"),
            metadata.revision.to_string(),
        ),
        (
            header::HeaderName::from_static("x-nano-created-at-ms"),
            metadata.created_at_ms.to_string(),
        ),
        (
            header::HeaderName::from_static("x-nano-updated-at-ms"),
            metadata.updated_at_ms.to_string(),
        ),
    ] {
        response.headers_mut().insert(
            name,
            HeaderValue::from_str(&value).map_err(|error| ApiError::internal(error.into()))?,
        );
    }
    Ok(())
}

async fn collection_method_not_allowed() -> ApiError {
    ApiError::method_not_allowed("POST")
}

async fn object_method_not_allowed() -> ApiError {
    ApiError::method_not_allowed("GET, HEAD, PUT, DELETE")
}

async fn not_found() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "not_found",
        "resource does not exist",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn canceled_requests_keep_blocking_work_inside_admission_bounds() {
        let active = Arc::new(Semaphore::new(1));
        let slots = Arc::new(Semaphore::new(1));
        let lease = Arc::new(Lease {
            _active: active.clone().acquire_owned().await.unwrap(),
            _slot: slots.clone().acquire_owned().await.unwrap(),
        });
        let (started, waiting) = tokio::sync::oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let task = tokio::spawn(blocking(lease, move || {
            started.send(()).unwrap();
            blocked.recv().unwrap();
            Ok(())
        }));
        waiting.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(active.available_permits(), 0);
        assert_eq!(slots.available_permits(), 0);
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while active.available_permits() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(slots.available_permits(), 1);
    }
}
