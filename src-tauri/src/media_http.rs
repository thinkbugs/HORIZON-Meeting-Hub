//! Serving library files over HTTP, the way a media engine asks for them.
//!
//! Two things here are not optional, and both were missing before this module:
//!
//! **Ranges.** `IMFMediaEngine` resolves an MP4 by reading its header, and a
//! header written at the end of the file (`moov` after `mdat`, which is what an
//! un-`faststart`ed recorder produces) means it seeks to the tail first. A server
//! that answers a `Range` request with the whole file and a `200` gets an
//! unplayable clip: the engine reads the wrong bytes, decides the container is
//! broken, and reports source-not-supported. Same story for seeking inside a film
//! and for resuming after a dropped connection. So `206` with `Content-Range` is
//! the load-bearing part of this file, not a nicety.
//!
//! **Streaming.** A two-hour recording is several gigabytes. Buffering a request
//! body or a response body in memory turns a normal cast into an out-of-memory
//! crash, which is why the upload path writes each chunk as it arrives.
//!
//! ## Who is allowed to ask
//!
//! Files are addressed by id, never by name: `/media/8f3c1a2b0c4d`. The id is
//! looked up in the library index and the stored name is re-checked before it is
//! joined to the folder, so there is no route that reads a path from the network.
//! Anything on the LAN can fetch what it can name, which is the trust model the
//! file transfer page already runs on; nothing here makes it wider.

use std::path::Path;

use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use crate::media_library::Library;

/// How much of a file is handed to the socket at a time. Big enough that a
/// gigabyte does not spend its life in syscall overhead, small enough that a
/// slow controller cannot make the hub hold much memory.
const CHUNK: u64 = 256 * 1024;

/// The request's byte range, resolved against the file's real length.
///
/// `end` is one past the last byte, not the last byte: HTTP writes the header
/// inclusively (`bytes 0-999/1000`), but inclusive ends cannot say "no bytes",
/// which is exactly what an empty file needs, and every length here is
/// `end - start` rather than `end - start + 1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    pub start: u64,
    pub end: u64,
}

impl ByteRange {
    pub fn whole(length: u64) -> Self {
        Self { start: 0, end: length }
    }

    pub fn len(&self) -> u64 {
        self.end - self.start
    }
}

/// Parse a `Range: bytes=` header.
///
/// `Ok(None)` means "send the whole file". That is the answer for a header with no
/// `bytes=` unit, for a list of ranges, and for one that cannot be parsed at all —
/// RFC 7233 says a server ignores a Range it does not understand, and a media
/// engine that sent something odd gets working playback rather than an error it
/// never asked for. `Err(())` is reserved for a well-formed range that this file
/// cannot satisfy, which is a `416` carrying the real length.
///
/// Only a single range is supported. Multi-range responses are what a download
/// manager uses; a media engine asks for one span at a time.
pub fn parse_range(header_value: Option<&str>, length: u64) -> Result<Option<ByteRange>, ()> {
    let Some(raw) = header_value else {
        return Ok(None);
    };
    let Some(spec) = raw.trim().strip_prefix("bytes=") else {
        return Ok(None);
    };
    if spec.contains(',') {
        return Ok(None);
    }
    if length == 0 {
        // Nothing to hand over, so no range can be satisfied.
        return Err(());
    }
    let Some((head, tail)) = spec.split_once('-') else {
        return Ok(None);
    };
    if head.is_empty() {
        // `bytes=-500`: the final 500 bytes. This is how a player fetches an MP4's
        // trailing header, so it has to land on the right span.
        let Ok(suffix) = tail.parse::<u64>() else {
            return Ok(None);
        };
        if suffix == 0 {
            return Err(());
        }
        return Ok(Some(ByteRange {
            start: length - suffix.min(length),
            end: length,
        }));
    }
    let Ok(start) = head.parse::<u64>() else {
        return Ok(None);
    };
    if start >= length {
        return Err(());
    }
    let end = if tail.is_empty() {
        length
    } else {
        let Ok(parsed) = tail.parse::<u64>() else {
            return Ok(None);
        };
        // A range that runs off the end is clamped, per RFC 7233; only a range
        // whose *start* is past the end is unsatisfiable.
        (parsed.saturating_add(1)).min(length)
    };
    if end <= start {
        return Err(());
    }
    Ok(Some(ByteRange { start, end }))
}

/// The content type a media engine and a browser both believe.
pub fn content_type(name: &str) -> &'static str {
    let extension = std::path::Path::new(name)
        .extension()
        .map(|ext| ext.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let extension = extension.as_str();
    match extension {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "heic" | "heif" => "image/heic",
        "avif" => "image/avif",
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "avi" => "video/x-msvideo",
        "wmv" => "video/x-ms-wmv",
        "flv" => "video/x-flv",
        "mpg" | "mpeg" => "video/mpeg",
        "ts" => "video/mp2t",
        "3gp" => "video/3gpp",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "wav" => "audio/wav",
        "aac" => "audio/aac",
        "flac" => "audio/flac",
        "ogg" | "oga" => "audio/ogg",
        "opus" => "audio/opus",
        "wma" => "audio/x-ms-wma",
        "mka" => "audio/x-matroska",
        _ => "application/octet-stream",
    }
}

/// `GET`/`HEAD` one library file, honouring a byte range.
///
/// A `HEAD` gets the same status and headers as the `GET` but no bytes: a media
/// engine uses it to learn the length before it starts asking for ranges.
pub async fn serve(library: &Library, method: Method, headers: &HeaderMap, id: &str) -> Response {
    let Some(item) = library.get(id) else {
        return text_response(StatusCode::NOT_FOUND, "片库里没有这一项");
    };
    if item.file.is_none() {
        // A remote item is not the hub's to proxy: the media engine fetches it
        // itself, which keeps a big file off the hub's uplink twice over.
        return text_response(StatusCode::NOT_FOUND, "这一项是远程地址，不在本机");
    }
    let Some(path) = library.path_for_id(id) else {
        return text_response(StatusCode::NOT_FOUND, "文件已不在片库目录");
    };
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let length = match tokio::fs::metadata(&path).await {
        Ok(metadata) => metadata.len(),
        Err(_) => return text_response(StatusCode::NOT_FOUND, "文件读不到"),
    };
    let requested = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok());
    let range = match parse_range(requested, length) {
        Ok(Some(range)) => range,
        Ok(None) => ByteRange::whole(length),
        Err(()) => {
            return Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header(header::CONTENT_RANGE, format!("bytes */{length}"))
                .header(header::ACCEPT_RANGES, "bytes")
                .body(Body::empty())
                .unwrap_or_else(|_| text_response(StatusCode::INTERNAL_SERVER_ERROR, "range"));
        }
    };

    // A range covering the whole file is answered as `200`, which is what RFC 7233
    // allows and what a controller expects when it probes with `bytes=0-`.
    let partial = requested.is_some() && !(range.start == 0 && range.end >= length);
    let status = if partial {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    let mut builder = Response::builder()
        .status(status)
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_str(content_type(&file_name))
                .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
        )
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, range.len())
        // A cast is played once, on one machine; caching it on every phone in the
        // room would only serve a stale frame after a re-upload.
        .header(header::CACHE_CONTROL, "no-store");
    if partial {
        builder = builder.header(
            header::CONTENT_RANGE,
            format!(
                "bytes {}-{}/{}",
                range.start,
                range.end - 1,
                length
            ),
        );
    }

    if method == Method::HEAD || length == 0 {
        return builder
            .body(Body::empty())
            .unwrap_or_else(|_| text_response(StatusCode::INTERNAL_SERVER_ERROR, "响应构造失败"));
    }
    let mut file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(_) => return text_response(StatusCode::NOT_FOUND, "文件打开失败"),
    };
    if file.seek(std::io::SeekFrom::Start(range.start)).await.is_err() {
        return text_response(StatusCode::INTERNAL_SERVER_ERROR, "跳转失败");
    }
    builder
        .body(body_of_file(file, range.len()))
        .unwrap_or_else(|_| text_response(StatusCode::INTERNAL_SERVER_ERROR, "响应构造失败"))
}

/// The next `length` bytes of an already-seeked file, read lazily so a request for
/// the tail of a file never touches the head.
fn body_of_file(file: tokio::fs::File, length: u64) -> Body {
    let stream = futures_util::stream::unfold(
        (file, length),
        move |(mut file, mut left)| async move {
            if left == 0 {
                return None;
            }
            let take = std::cmp::min(left, CHUNK) as usize;
            let mut buffer = vec![0u8; take];
            match file.read_exact(&mut buffer).await {
                Ok(_) => {
                    left -= take as u64;
                    Some((Ok(axum::body::Bytes::from(buffer)), (file, left)))
                }
                Err(error) => Some((Err(error), (file, 0))),
            }
        },
    );
    Body::from_stream(stream)
}

/// Write one multipart field straight to disk. Returns the byte count, or the
/// first error and how much was written, so the caller can clean up.
pub async fn field_to_file(
    field: &mut axum::extract::multipart::Field<'_>,
    path: &Path,
    limit: u64,
) -> Result<u64, (String, u64)> {
    let mut file = tokio::fs::File::create(path)
        .await
        .map_err(|error| (format!("无法写入文件：{error}"), 0u64))?;
    let mut written = 0u64;
    while let Some(chunk) = field.chunk().await.map_err(|error| (format!("{error}"), written))? {
        written += chunk.len() as u64;
        if written > limit {
            let _ = file.shutdown().await;
            return Err((format!("文件超过 {limit} 字节上限"), written));
        }
        file.write_all(&chunk)
            .await
            .map_err(|error| (format!("写入失败：{error}"), written))?;
    }
    file.flush()
        .await
        .map_err(|error| (format!("写入未落盘：{error}"), written))?;
    Ok(written)
}

fn text_response(status: StatusCode, text: &str) -> Response {
    (status, text.to_string()).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_header_means_the_whole_file() {
        assert_eq!(parse_range(None, 1000).expect("ok"), None);
        assert_eq!(
            parse_range(Some("bytes=0-999"), 1000).expect("ok"),
            Some(ByteRange { start: 0, end: 1000 })
        );
    }

    #[test]
    fn an_open_ended_range_runs_to_the_end() {
        assert_eq!(
            parse_range(Some("bytes=500-"), 1000).expect("ok"),
            Some(ByteRange { start: 500, end: 1000 })
        );
    }

    #[test]
    fn a_suffix_range_counts_back_from_the_end() {
        // This is the request a media engine makes to read an MP4's trailing
        // header, so it has to land on the right bytes.
        assert_eq!(
            parse_range(Some("bytes=-500"), 1000).expect("ok"),
            Some(ByteRange { start: 500, end: 1000 })
        );
        assert_eq!(
            parse_range(Some("bytes=-10"), 1000).expect("ok"),
            Some(ByteRange { start: 990, end: 1000 })
        );
        assert_eq!(
            parse_range(Some("bytes=-1"), 1000).expect("ok"),
            Some(ByteRange { start: 999, end: 1000 }),
            "the last byte, which is what `bytes=-1` names"
        );
        assert_eq!(
            parse_range(Some("bytes=-5000"), 1000).expect("ok"),
            Some(ByteRange { start: 0, end: 1000 }),
            "asking for more than the file has clamps instead of failing"
        );
    }

    #[test]
    fn a_range_past_the_end_is_unsatisfiable() {
        assert!(parse_range(Some("bytes=1500-2000"), 1000).is_err());
        assert!(parse_range(Some("bytes=2000-"), 1000).is_err());
        assert!(parse_range(Some("bytes=1000-"), 1000).is_err());
    }

    #[test]
    fn a_range_of_no_bytes_is_unsatisfiable() {
        assert!(parse_range(Some("bytes=-0"), 1000).is_err());
        assert_eq!(
            parse_range(Some("bytes=0-0"), 1000).expect("ok"),
            Some(ByteRange { start: 0, end: 1 }),
            "`0-0` names the first byte, not nothing"
        );
    }

    #[test]
    fn a_reversed_range_is_unsatisfiable() {
        assert!(parse_range(Some("bytes=800-500"), 1000).is_err());
    }

    #[test]
    fn a_garbage_range_is_ignored_rather_than_rejected() {
        assert_eq!(parse_range(Some("bytes=abc"), 1000).expect("ok"), None);
        assert_eq!(parse_range(Some("items=0-10"), 1000).expect("ok"), None);
        assert_eq!(parse_range(Some("bytes=0-10,20-30"), 1000).expect("ok"), None);
        assert_eq!(parse_range(Some("bytes="), 1000).expect("ok"), None);
    }

    #[test]
    fn an_empty_file_cannot_satisfy_any_range() {
        assert!(parse_range(Some("bytes=0-10"), 0).is_err());
        assert_eq!(parse_range(None, 0).expect("ok"), None);
    }

    #[test]
    fn a_range_length_counts_the_bytes_in_it() {
        assert_eq!(ByteRange { start: 0, end: 1 }.len(), 1);
        assert_eq!(ByteRange { start: 9, end: 100 }.len(), 91);
        assert_eq!(ByteRange::whole(10).len(), 10);
        assert_eq!(
            ByteRange::whole(0).len(),
            0,
            "an empty file has a range, and it is zero bytes"
        );
    }

    #[test]
    fn content_types_cover_what_a_room_projects() {
        assert_eq!(content_type("海报.JPG"), "image/jpeg");
        assert_eq!(content_type("clip.mp4"), "video/mp4");
        assert_eq!(content_type("film.mkv"), "video/x-matroska");
        assert_eq!(content_type("song.flac"), "audio/flac");
        assert_eq!(content_type("voice.m4a"), "audio/mp4");
        assert_eq!(content_type("notes"), "application/octet-stream");
    }
}
