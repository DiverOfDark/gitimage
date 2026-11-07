//! OCI JSON error responses.

use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

use crate::OciError;

/// An error reply: status, OCI error code, message and any extra headers.
#[derive(Debug)]
pub(super) struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
    headers: Vec<(HeaderName, HeaderValue)>,
}

impl ApiError {
    pub(super) fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            headers: Vec::new(),
        }
    }

    /// Adds a response header (values built by this crate are always valid).
    pub(super) fn with_header(mut self, name: HeaderName, value: &str) -> Self {
        if let Ok(v) = HeaderValue::from_str(value) {
            self.headers.push((name, v));
        }
        self
    }

    pub(super) fn range_invalid(message: impl Into<String>) -> Self {
        Self::new(StatusCode::RANGE_NOT_SATISFIABLE, "RANGE_INVALID", message)
    }

    pub(super) fn size_invalid(message: impl Into<String>) -> Self {
        Self::new(StatusCode::PAYLOAD_TOO_LARGE, "SIZE_INVALID", message)
    }
}

impl From<OciError> for ApiError {
    fn from(e: OciError) -> Self {
        let message = e.to_string();
        let (status, code) = match &e {
            OciError::BlobUnknown => (StatusCode::NOT_FOUND, "BLOB_UNKNOWN"),
            OciError::BlobUploadUnknown => (StatusCode::NOT_FOUND, "BLOB_UPLOAD_UNKNOWN"),
            OciError::DigestInvalid => (StatusCode::BAD_REQUEST, "DIGEST_INVALID"),
            OciError::ManifestUnknown => (StatusCode::NOT_FOUND, "MANIFEST_UNKNOWN"),
            OciError::ManifestInvalid(_) => (StatusCode::BAD_REQUEST, "MANIFEST_INVALID"),
            OciError::NameUnknown => (StatusCode::NOT_FOUND, "NAME_UNKNOWN"),
            OciError::TagInvalid => (StatusCode::BAD_REQUEST, "TAG_INVALID"),
            OciError::Unauthorized => (StatusCode::UNAUTHORIZED, "UNAUTHORIZED"),
            OciError::Denied => (StatusCode::FORBIDDEN, "DENIED"),
            OciError::Unsupported => (StatusCode::METHOD_NOT_ALLOWED, "UNSUPPORTED"),
            OciError::Backend(_) => {
                tracing::warn!(error = %message, "storage backend error");
                (StatusCode::INTERNAL_SERVER_ERROR, "UNKNOWN")
            }
        };
        let err = Self::new(status, code, message);
        if matches!(e, OciError::Unauthorized) {
            err.with_header(header::WWW_AUTHENTICATE, r#"Basic realm="gitimage""#)
        } else {
            err
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = serde_json::json!({
            "errors": [{ "code": self.code, "message": self.message }]
        });
        let mut resp = (self.status, axum::Json(body)).into_response();
        resp.headers_mut().extend(self.headers);
        resp
    }
}
