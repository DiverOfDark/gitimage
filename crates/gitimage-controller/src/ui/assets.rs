//! The web UI, embedded at compile time by `build.rs`.

use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};

include!(concat!(env!("OUT_DIR"), "/ui_assets.rs"));

fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or_default() {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn find(path: &str) -> Option<&'static [u8]> {
    ASSETS.iter().find(|(p, _)| *p == path).map(|(_, b)| *b)
}

/// Serves an embedded file; any other path gets `index.html` (client-side routing).
pub async fn serve(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if path.starts_with("api/") || path.starts_with("auth/") {
        return StatusCode::NOT_FOUND.into_response();
    }
    let (name, body) = match find(path) {
        Some(b) if !path.is_empty() => (path, b),
        // A missing hashed asset must not turn into HTML.
        _ if path.starts_with("assets/") => return StatusCode::NOT_FOUND.into_response(),
        _ => ("index.html", find("index.html").unwrap_or_default()),
    };
    let cache = if name.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static(content_type(name)),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static(cache)),
        ],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_is_always_embedded() {
        assert!(find("index.html").is_some());
        assert_eq!(
            content_type("assets/app.js"),
            "text/javascript; charset=utf-8"
        );
    }
}
