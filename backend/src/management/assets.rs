//! 编译期 WebUI 资源、缓存策略与 SPA fallback。

use axum::body::Body;
use axum::http::header::ACCEPT;
#[cfg(feature = "webui-embed")]
use axum::http::header::{
    CACHE_CONTROL, CONTENT_LENGTH, CONTENT_SECURITY_POLICY, CONTENT_TYPE, ETAG, IF_NONE_MATCH,
    X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{Method, Request, Response, StatusCode};
#[cfg(feature = "webui-embed")]
use sha2::{Digest, Sha256};

#[cfg(feature = "webui-embed")]
use rust_embed::RustEmbed;

#[cfg(feature = "webui-embed")]
#[derive(RustEmbed)]
#[folder = "../frontend/dist/"]
#[exclude = "*.map"]
struct WebAssets;

pub(crate) fn ensure_available() -> Result<(), &'static str> {
    #[cfg(feature = "webui-embed")]
    {
        WebAssets::get("index.html")
            .map(|_| ())
            .ok_or("embedded WebUI index is missing")
    }
    #[cfg(not(feature = "webui-embed"))]
    {
        Ok(())
    }
}

pub(crate) async fn fallback(request: Request<Body>) -> Response<Body> {
    if !matches!(*request.method(), Method::GET | Method::HEAD) {
        return empty_response(StatusCode::METHOD_NOT_ALLOWED);
    }
    let path = request.uri().path();
    if invalid_path(path) {
        return empty_response(StatusCode::NOT_FOUND);
    }
    let key = path.trim_start_matches('/');
    if !key.is_empty()
        && let Some(response) = asset_response(key, &request)
    {
        return response;
    }
    let accepts_html = request
        .headers()
        .get(ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("text/html") || value.contains("*/*"));
    if (key.is_empty() || !key.rsplit('/').next().unwrap_or_default().contains('.')) && accepts_html
    {
        return asset_response("index.html", &request)
            .unwrap_or_else(|| empty_response(StatusCode::SERVICE_UNAVAILABLE));
    }
    empty_response(StatusCode::NOT_FOUND)
}

fn invalid_path(path: &str) -> bool {
    let lowercase = path.to_ascii_lowercase();
    path.contains("..")
        || path.contains('\\')
        || path.contains('\0')
        || lowercase.contains("%2e")
        || lowercase.contains("%2f")
        || lowercase.contains("%5c")
        || lowercase.contains("%00")
}

#[cfg(feature = "webui-embed")]
fn asset_response(key: &str, request: &Request<Body>) -> Option<Response<Body>> {
    let asset = WebAssets::get(key)?;
    let bytes = asset.data.as_ref();
    let etag = format!("\"{:x}\"", Sha256::digest(bytes));
    let not_modified = request
        .headers()
        .get(IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == etag);
    let status = if not_modified {
        StatusCode::NOT_MODIFIED
    } else {
        StatusCode::OK
    };
    let body = if not_modified || request.method() == Method::HEAD {
        Body::empty()
    } else {
        Body::from(bytes.to_vec())
    };
    let cache_control = if key == "index.html" {
        "no-cache"
    } else if key.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "public, max-age=3600"
    };
    let content_type = mime_guess::from_path(key).first_or_octet_stream();
    let mut response = Response::builder()
        .status(status)
        .header(CONTENT_TYPE, content_type.as_ref())
        .header(CONTENT_LENGTH, bytes.len())
        .header(CACHE_CONTROL, cache_control)
        .header(ETAG, etag)
        .header(X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(
            CONTENT_SECURITY_POLICY,
            "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; object-src 'none'; base-uri 'self'; frame-ancestors 'none'; form-action 'self'",
        )
        .body(body)
        .expect("static response headers are valid");
    if not_modified {
        response.headers_mut().remove(CONTENT_LENGTH);
    }
    Some(response)
}

#[cfg(not(feature = "webui-embed"))]
fn asset_response(_key: &str, _request: &Request<Body>) -> Option<Response<Body>> {
    None
}

fn empty_response(status: StatusCode) -> Response<Body> {
    Response::builder()
        .status(status)
        .body(Body::empty())
        .expect("empty response is valid")
}

#[cfg(all(test, feature = "webui-embed"))]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, ETAG, IF_NONE_MATCH};
    use axum::http::{Request, StatusCode};

    use super::fallback;

    #[tokio::test]
    async fn serves_embedded_index_for_spa_route() {
        let request = Request::builder()
            .uri("/dashboard")
            .header("accept", "text/html")
            .body(Body::empty())
            .unwrap();

        let response = fallback(request).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[CONTENT_TYPE], "text/html");
        assert_eq!(response.headers()[CACHE_CONTROL], "no-cache");
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        assert!(
            body.windows(b"<div id=\"root\"></div>".len())
                .any(|window| { window == b"<div id=\"root\"></div>" })
        );
    }

    #[tokio::test]
    async fn supports_head_and_conditional_requests_without_body() {
        let get_request = Request::builder()
            .uri("/index.html")
            .header("accept", "text/html")
            .body(Body::empty())
            .unwrap();
        let get_response = fallback(get_request).await;
        let etag = get_response.headers()[ETAG].clone();
        let get_body = to_bytes(get_response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        assert!(!get_body.is_empty());

        let head_request = Request::builder()
            .method("HEAD")
            .uri("/index.html")
            .header("accept", "text/html")
            .body(Body::empty())
            .unwrap();
        let head_response = fallback(head_request).await;
        assert_eq!(head_response.status(), StatusCode::OK);
        assert_eq!(head_response.headers()[CONTENT_TYPE], "text/html");
        assert_eq!(
            to_bytes(head_response.into_body(), 1024)
                .await
                .unwrap()
                .len(),
            0
        );

        let conditional_request = Request::builder()
            .uri("/index.html")
            .header(IF_NONE_MATCH, etag)
            .body(Body::empty())
            .unwrap();
        let conditional_response = fallback(conditional_request).await;
        assert_eq!(conditional_response.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(
            to_bytes(conditional_response.into_body(), 1024)
                .await
                .unwrap()
                .len(),
            0
        );
    }
}
