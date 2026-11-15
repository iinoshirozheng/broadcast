//! How attachment content is served by both the local server and the cloud worker.

const INLINE_TYPES: &[&str] = &[
    "application/pdf",
    "image/avif",
    "image/gif",
    "image/jpeg",
    "image/png",
    "image/webp",
    "text/plain",
];

/// `encodeURIComponent` with `'()*` also escaped, as RFC 5987 `filename*` expects.
fn encode_filename(filename: &str) -> String {
    const UNRESERVED: &[u8] = b"-_.!~";
    let mut encoded = String::with_capacity(filename.len());
    for byte in filename.bytes() {
        if byte.is_ascii_alphanumeric() || UNRESERVED.contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// The `content-disposition` and `content-type` of a stored attachment. Only the safe inline
/// types open in the browser, and only when `open_inline` asks for it.
pub fn content_headers<'a>(
    filename: &str,
    content_type: &'a str,
    open_inline: bool,
) -> (String, &'a str) {
    let inline =
        open_inline && (INLINE_TYPES.contains(&content_type) || content_type.starts_with("video/"));
    let disposition = format!(
        "{}; filename*=UTF-8''{}",
        if inline { "inline" } else { "attachment" },
        encode_filename(filename)
    );
    (
        disposition,
        if inline {
            content_type
        } else {
            "application/octet-stream"
        },
    )
}
