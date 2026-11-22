//! Response compression as Go's `gzhttp.GzipHandler` (klauspost/compress 1.19.2) does it around every API and web
//! handler when `ServiceSettings.WebserverMode` is "gzip" (read at start, as Go wraps its handlers once):
//!
//! - `Vary: Accept-Encoding` on every answer of a wrapped handler, compressed or not;
//! - zstd or gzip by the `Accept-Encoding` q-values, zstd when they are equal; never for `HEAD`;
//! - only bodies of 1024 bytes or more (`DefaultMinSize`), without `Content-Encoding` or `Content-Range`, and not of
//!   an already compressed type (`DefaultContentTypeFilter`); `Content-Length` and `Accept-Ranges` are dropped;
//! - a websocket upgrade (101) passes through untouched (gzhttp hands the hijacked connection over as it is).
//!
//! Go's handlers outside the wrappers stay plain: the API's 404 for an unknown path, `root.html`, `robots.txt`,
//! `unsupported_browser.js`. Static files (`web.rs`) are compressed there, with a cache.

// Based on github.com/klauspost/compress gzhttp/compress.go (BSD-3-Clause) as used by Mattermost
// server/channels/api4/handlers.go and web/handlers.go, Copyright (c) 2015-present Mattermost, Inc. Licensed under
// AGPL-3.0.

use std::io::Write;

use axum::body::{Body, Bytes};
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;
use futures_util::StreamExt;

/// `DefaultMinSize`.
pub const MIN_SIZE: usize = 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Enc {
    None,
    Gzip,
    Zstd,
}

impl Enc {
    pub fn name(self) -> &'static str {
        match self {
            Enc::None => "",
            Enc::Gzip => "gzip",
            Enc::Zstd => "zstd",
        }
    }
}

/// `parseCoding`: the coding, lower case, and its q-value (1 by default, clamped to 0..1, 0 when unparsable).
fn parse_coding(s: &str) -> (String, f64) {
    if !s.contains(';') {
        return (s.trim().to_lowercase(), 1.0);
    }
    let mut coding = String::new();
    let mut q = 1.0;
    for (n, part) in s.split(';').enumerate() {
        let part = part.trim();
        if n == 0 {
            coding = part.to_lowercase();
        } else if part.len() >= 2 && part[..2].eq_ignore_ascii_case("q=") {
            q = part[2..].parse::<f64>().unwrap_or(0.0).clamp(0.0, 1.0);
        }
    }
    (coding, q)
}

/// `parseEncodingQValue`: the q-value of the first entry for `enc`, 0 without one.
fn q_value(header: &str, enc: &str) -> f64 {
    header.trim().split(',').map(parse_coding).find(|(c, _)| c == enc).map_or(0.0, |(_, q)| q)
}

/// `selectEncoding` with gzip and zstd enabled and zstd preferred.
pub fn select(method: &Method, headers: &HeaderMap) -> Enc {
    if method == Method::HEAD {
        return Enc::None;
    }
    // `Header.Get`: the first value only.
    let ae = headers.get(header::ACCEPT_ENCODING).and_then(|v| v.to_str().ok()).unwrap_or("");
    if ae.is_empty() {
        return Enc::None;
    }
    let (gz, zs) = (q_value(ae, "gzip"), q_value(ae, "zstd"));
    match (gz > 0.0, zs > 0.0) {
        (false, false) => Enc::None,
        (true, false) => Enc::Gzip,
        (false, true) => Enc::Zstd,
        _ if gz > zs => Enc::Gzip,
        _ => Enc::Zstd,
    }
}

/// `DefaultContentTypeFilter`: not audio, video, JPEG or an archive format.
pub fn content_type_ok(ct: &str) -> bool {
    let ct = ct.trim().to_lowercase();
    const CONTAINS: [&str; 8] = ["compress", "zip", "snappy", "lzma", "xz", "zstd", "brotli", "stuffit"];
    const PREFIX: [&str; 3] = ["video/", "audio/", "image/jp"];
    !(CONTAINS.iter().any(|s| ct.contains(s)) || PREFIX.iter().any(|p| ct.starts_with(p)))
}

/// The whole of `data` compressed (gzip at the default level, zstd at its fastest, as gzhttp's defaults).
pub fn encode(enc: Enc, data: &[u8]) -> Vec<u8> {
    let mut e = Encoder::new(enc);
    let mut out = e.write(data).unwrap_or_default();
    out.extend(e.finish().unwrap_or_default());
    out
}

enum Encoder {
    Gzip(flate2::write::GzEncoder<Vec<u8>>),
    Zstd(Box<zstd::stream::write::Encoder<'static, Vec<u8>>>),
}

impl Encoder {
    fn new(enc: Enc) -> Encoder {
        match enc {
            Enc::Zstd => match zstd::stream::write::Encoder::new(Vec::new(), 1) {
                Ok(z) => Encoder::Zstd(Box::new(z)),
                Err(_) => Encoder::Gzip(flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default())),
            },
            _ => Encoder::Gzip(flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default())),
        }
    }

    /// Feeds `b` in; what the encoder has produced so far.
    fn write(&mut self, b: &[u8]) -> std::io::Result<Vec<u8>> {
        match self {
            Encoder::Gzip(g) => {
                g.write_all(b)?;
                Ok(std::mem::take(g.get_mut()))
            }
            Encoder::Zstd(z) => {
                z.write_all(b)?;
                Ok(std::mem::take(z.get_mut()))
            }
        }
    }

    fn finish(self) -> std::io::Result<Vec<u8>> {
        match self {
            Encoder::Gzip(g) => g.finish(),
            Encoder::Zstd(z) => z.finish(),
        }
    }
}

/// Paths whose Go handlers are not wrapped, or that compress on their own.
fn unwrapped(path: &str) -> bool {
    path.starts_with("/static/")
        || path == "/_documentation"
        || path == "/robots.txt"
        || path == "/unsupported_browser.js"
}

/// The middleware for the wrapped routes (`from_fn`), installed only in "gzip" mode.
pub async fn layer(req: Request, next: Next) -> Response {
    let enc = select(req.method(), req.headers());
    let skip = unwrapped(req.uri().path());
    let mut r = next.run(req).await;
    if skip || r.status() == StatusCode::SWITCHING_PROTOCOLS {
        return r;
    }
    r.headers_mut().append(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    if enc == Enc::None { r } else { compress(r, enc).await }
}

/// `GzipResponseWriter`: the body compressed once it reaches the minimum size, as it streams.
async fn compress(r: Response, enc: Enc) -> Response {
    let h = r.headers();
    if h.contains_key(header::CONTENT_ENCODING) || h.contains_key(header::CONTENT_RANGE) || !body_allowed(r.status()) {
        return r;
    }
    let ct = h.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
    if !ct.is_empty() && !content_type_ok(ct) {
        return r;
    }
    let declared =
        h.get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
    if declared != 0 && declared < MIN_SIZE {
        return r;
    }
    let (mut parts, body) = r.into_parts();
    if axum::body::HttpBody::is_end_stream(&body) {
        return Response::from_parts(parts, body);
    }
    let mut data = body.into_data_stream();
    let mut buf: Vec<u8> = Vec::new();
    let mut ended = false;
    let mut failed = None;
    while buf.len() < MIN_SIZE {
        match data.next().await {
            Some(Ok(c)) => buf.extend_from_slice(&c),
            Some(Err(e)) => {
                failed = Some(e);
                break;
            }
            None => {
                ended = true;
                break;
            }
        }
    }
    if let Some(e) = failed {
        // The handler's body broke off: what came before it, then the error.
        let s = futures_util::stream::iter([Ok(Bytes::from(buf)), Err(e)]);
        return Response::from_parts(parts, Body::from_stream(s));
    }
    if ended && buf.len() < MIN_SIZE && declared == 0 {
        return Response::from_parts(parts, Body::from(buf));
    }
    parts.headers.insert(header::CONTENT_ENCODING, HeaderValue::from_static(enc.name()));
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.remove(header::ACCEPT_RANGES);
    let mut e = Encoder::new(enc);
    let first = e.write(&buf);
    let state = (data, Some(e), Some(first));
    let s = futures_util::stream::unfold(state, |(mut data, mut e, mut first)| async move {
        if let Some(f) = first.take() {
            match f {
                Ok(b) if !b.is_empty() => return Some((Ok(Bytes::from(b)), (data, e, None))),
                Ok(_) => {}
                Err(err) => return Some((Err(axum::Error::new(err)), (data, None, None))),
            }
        }
        let enc = e.as_mut()?;
        loop {
            match data.next().await {
                Some(Ok(c)) => match enc.write(&c) {
                    Ok(b) if b.is_empty() => continue,
                    Ok(b) => return Some((Ok(Bytes::from(b)), (data, e, None))),
                    Err(err) => return Some((Err(axum::Error::new(err)), (data, None, None))),
                },
                Some(Err(err)) => return Some((Err(err), (data, None, None))),
                None => {
                    let out = e.take()?.finish().map(Bytes::from).map_err(axum::Error::new);
                    return Some((out, (data, None, None)));
                }
            }
        }
    });
    Response::from_parts(parts, Body::from_stream(s))
}

/// `bodyAllowedForStatus`.
fn body_allowed(s: StatusCode) -> bool {
    !(s.is_informational() || s == StatusCode::NO_CONTENT || s == StatusCode::NOT_MODIFIED)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(v: &str) -> HeaderMap {
        let mut m = HeaderMap::new();
        m.insert(header::ACCEPT_ENCODING, HeaderValue::from_str(v).unwrap());
        m
    }

    #[test]
    fn encodings_as_gzhttp() {
        assert_eq!(select(&Method::GET, &h("gzip, deflate, br, zstd")), Enc::Zstd);
        assert_eq!(select(&Method::GET, &h("gzip")), Enc::Gzip);
        assert_eq!(select(&Method::GET, &h("gzip;q=1, zstd;q=0.5")), Enc::Gzip);
        assert_eq!(select(&Method::GET, &h("zstd;q=0, gzip")), Enc::Gzip);
        assert_eq!(select(&Method::GET, &h("br, deflate")), Enc::None);
        assert_eq!(select(&Method::GET, &h("GZIP;Q=0.3")), Enc::Gzip);
        assert_eq!(select(&Method::GET, &h("gzip;q=x")), Enc::None);
        assert_eq!(select(&Method::HEAD, &h("gzip")), Enc::None);
        assert!(content_type_ok("application/json"));
        assert!(content_type_ok("image/png"));
        assert!(!content_type_ok("image/jpeg"));
        assert!(!content_type_ok("application/zip"));
        assert!(!content_type_ok("video/mp4"));
    }

    #[test]
    fn round_trip() {
        let data = "x".repeat(5000);
        let gz = encode(Enc::Gzip, data.as_bytes());
        let mut out = String::new();
        std::io::Read::read_to_string(&mut flate2::read::GzDecoder::new(&gz[..]), &mut out).unwrap();
        assert_eq!(out, data);
        let zs = encode(Enc::Zstd, data.as_bytes());
        assert_eq!(zstd::stream::decode_all(&zs[..]).unwrap(), data.as_bytes());
    }
}
