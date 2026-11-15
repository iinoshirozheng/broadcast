//! `serveStatic`: the built web app, with `index.html` for client-side routes.

use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use broadcast_domain::js::decode_uri_component;
use broadcast_domain::{ApiError, Result};

/// The file response, or `None` when nothing matches (the caller replies 404).
/// Axum derives `content-length` and drops the body for `HEAD`.
pub async fn serve(root: &Path, method: &Method, path: &str) -> Result<Option<Response>> {
    if method != Method::GET && method != Method::HEAD {
        return Ok(None);
    }
    let invalid = |message: &str| ApiError::new(400, "INVALID_PATH", message);
    let decoded =
        decode_uri_component(path).ok_or_else(|| invalid("URL path contains invalid encoding"))?;
    if decoded.contains('\0') {
        return Err(invalid("URL path is invalid").into());
    }
    let relative = if decoded == "/" {
        "index.html"
    } else {
        decoded.trim_start_matches('/')
    };
    let mut filename = resolve(root, relative)
        .ok_or_else(|| invalid("URL path is outside the static directory"))?;

    let mut found = is_file(&filename).await?;
    if !found && extname(relative).is_empty() {
        filename = root.join("index.html");
        found = is_file(&filename).await?;
    }
    if !found {
        return Ok(None);
    }

    let body = tokio::fs::read(&filename).await?;
    let cache_control = if filename
        .file_name()
        .is_some_and(|name| name == "index.html")
    {
        "no-cache"
    } else {
        "public, max-age=31536000, immutable"
    };
    let extension = filename
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default();
    let mut response = (StatusCode::OK, body).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(cache_control),
    );
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(content_type(&extension.to_lowercase())),
    );
    Ok(Some(response))
}

/// `path.resolve(root, relative)`, or `None` when the result leaves `root`.
fn resolve(root: &Path, relative: &str) -> Option<PathBuf> {
    let mut filename = root.to_path_buf();
    for component in Path::new(relative).components() {
        match component {
            Component::Normal(part) => filename.push(part),
            Component::ParentDir => {
                filename.pop();
            }
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    filename.starts_with(root).then_some(filename)
}

/// `stat(filename).isFile()`, where only a missing path counts as "not a file".
async fn is_file(filename: &Path) -> Result<bool> {
    match tokio::fs::metadata(filename).await {
        Ok(metadata) => Ok(metadata.is_file()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// Node's `path.posix.extname`.
fn extname(path: &str) -> &str {
    let base = path
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default();
    let name = base.trim_start_matches('.');
    match name.rfind('.') {
        Some(index) => &name[index..],
        None => "",
    }
}

fn content_type(extension: &str) -> &'static str {
    match extension {
        "css" => "text/css; charset=utf-8",
        "html" => "text/html; charset=utf-8",
        "ico" => "image/x-icon",
        "jpeg" | "jpg" => "image/jpeg",
        "js" => "text/javascript; charset=utf-8",
        "json" | "map" => "application/json; charset=utf-8",
        "png" => "image/png",
        "svg" => "image/svg+xml",
        "webp" => "image/webp",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}
