use axum::{
    Json,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Serialize;

#[derive(Debug)]
pub(crate) struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
    header: Option<(header::HeaderName, HeaderValue)>,
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: ErrorDetail<'a>,
}

#[derive(Serialize)]
struct ErrorDetail<'a> {
    code: &'a str,
    message: &'a str,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            header: None,
        }
    }

    pub fn bad_request(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code, message)
    }

    pub fn precondition_failed() -> Self {
        Self::new(
            StatusCode::PRECONDITION_FAILED,
            "precondition_failed",
            "the ETag precondition did not match",
        )
    }

    pub fn overloaded() -> Self {
        let mut error = Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "overloaded",
            "request capacity or queue deadline exceeded",
        );
        error.header = Some((header::RETRY_AFTER, HeaderValue::from_static("1")));
        error
    }

    pub fn unauthorized() -> Self {
        let mut error = Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "a valid bearer token is required",
        );
        error.header = Some((
            header::WWW_AUTHENTICATE,
            HeaderValue::from_static(r#"Bearer realm="nano-db""#),
        ));
        error
    }

    pub fn method_not_allowed(allow: &'static str) -> Self {
        let mut error = Self::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            "method is not supported for this resource",
        );
        error.header = Some((header::ALLOW, HeaderValue::from_static(allow)));
        error
    }

    pub fn internal(error: anyhow::Error) -> Self {
        tracing::error!(error = ?error, "storage or blocking operation failed");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "storage_error",
            "the operation could not be completed",
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = ErrorBody {
            error: ErrorDetail {
                code: self.code,
                message: &self.message,
            },
        };
        let mut response = (self.status, Json(body)).into_response();
        if let Some((name, value)) = self.header {
            response.headers_mut().insert(name, value);
        }
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        response.headers_mut().insert(
            "x-content-type-options",
            HeaderValue::from_static("nosniff"),
        );
        response
    }
}
