//! Tiny local HTTP server for offline pipeline integration tests.
//!
//! It speaks HTTP/1.1 only inside this test harness; production ystt uses
//! ALPN Auto (H2 preferred, H1 fallback). It supports byte ranges, sets cookies that
//! TikTok's page sets, and can fail individual gallery images.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub const VIDEO_SIZE: usize = 512 * 1024;

#[derive(Debug, Default)]
pub struct Counters {
    pub total: AtomicUsize,
    pub ranges: AtomicUsize,
    pub media: AtomicUsize,
    pub pages: AtomicUsize,
}

pub struct TestServer {
    pub origin: String,
    pub counters: Arc<Counters>,
}

pub async fn start() -> TestServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let origin = format!("http://{address}");
    let counters = Arc::new(Counters::default());

    let server_origin = origin.clone();
    let server_counters = counters.clone();
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                break;
            };
            let origin = server_origin.clone();
            let counters = server_counters.clone();
            tokio::spawn(async move {
                handle(socket, &origin, &counters).await;
            });
        }
    });

    TestServer { origin, counters }
}

async fn handle(mut socket: TcpStream, origin: &str, counters: &Counters) {
    let Some(request) = read_request(&mut socket).await else {
        return;
    };
    counters.total.fetch_add(1, Ordering::Relaxed);

    let (method, path) = parse_request_line(&request).unwrap_or(("GET", "/"));
    if method != "GET" {
        write_response(&mut socket, 405, &[], b"method not allowed").await;
        return;
    }
    let headers = parse_headers(&request);

    match path.split('?').next().unwrap_or(path) {
        "/@test/video/123" => {
            counters.pages.fetch_add(1, Ordering::Relaxed);
            let page = video_page(origin, "123");
            write_response(
                &mut socket,
                200,
                &[
                    ("Content-Type", "text/html; charset=utf-8"),
                    ("Set-Cookie", "ttwid=test; Path=/"),
                    ("Set-Cookie", "tt_chain_token=test; Path=/"),
                ],
                page.as_bytes(),
            )
            .await;
        }
        "/@test/video/124" => {
            counters.pages.fetch_add(1, Ordering::Relaxed);
            let page = video_page(origin, "124");
            write_response(
                &mut socket,
                200,
                &[
                    ("Content-Type", "text/html; charset=utf-8"),
                    ("Set-Cookie", "ttwid=test; Path=/"),
                    ("Set-Cookie", "tt_chain_token=test; Path=/"),
                ],
                page.as_bytes(),
            )
            .await;
        }
        "/embed/v2/123" => {
            counters.pages.fetch_add(1, Ordering::Relaxed);
            let page = embed_video_page(origin);
            write_response(
                &mut socket,
                200,
                &[("Content-Type", "text/html; charset=utf-8")],
                page.as_bytes(),
            )
            .await;
        }
        "/media/embed.mp4" => {
            counters.media.fetch_add(1, Ordering::Relaxed);
            serve_video(&mut socket, &headers, counters).await;
        }
        "/@test/video/456" => {
            counters.pages.fetch_add(1, Ordering::Relaxed);
            let page = gallery_page(origin);
            write_response(
                &mut socket,
                200,
                &[
                    ("Content-Type", "text/html; charset=utf-8"),
                    ("Set-Cookie", "ttwid=test; Path=/"),
                    ("Set-Cookie", "tt_chain_token=test; Path=/"),
                ],
                page.as_bytes(),
            )
            .await;
        }
        "/media/video.mp4" => {
            counters.media.fetch_add(1, Ordering::Relaxed);
            if !has_session_cookie(&headers) {
                write_response(&mut socket, 403, &[], b"forbidden").await;
                return;
            }
            serve_video(&mut socket, &headers, counters).await;
        }
        "/media/audio.mp3" => {
            counters.media.fetch_add(1, Ordering::Relaxed);
            if !has_session_cookie(&headers) {
                write_response(&mut socket, 403, &[], b"forbidden").await;
                return;
            }
            write_response(&mut socket, 200, &[("Content-Type", "audio/mpeg")], &mp3()).await;
        }
        "/media/p1.jpg" => {
            counters.media.fetch_add(1, Ordering::Relaxed);
            if !has_session_cookie(&headers) {
                write_response(&mut socket, 403, &[], b"forbidden").await;
                return;
            }
            write_response(
                &mut socket,
                200,
                &[("Content-Type", "image/jpeg")],
                &jpeg(1),
            )
            .await;
        }
        "/media/p2.jpg" => {
            counters.media.fetch_add(1, Ordering::Relaxed);
            if !has_session_cookie(&headers) {
                write_response(&mut socket, 403, &[], b"forbidden").await;
                return;
            }
            write_response(
                &mut socket,
                200,
                &[("Content-Type", "image/jpeg")],
                &jpeg(2),
            )
            .await;
        }
        "/media/bad.jpg" => {
            counters.media.fetch_add(1, Ordering::Relaxed);
            write_response(&mut socket, 500, &[], b"boom").await;
        }
        _ => write_response(&mut socket, 404, &[], b"not found").await,
    }
}

async fn serve_video(
    socket: &mut TcpStream,
    headers: &HashMap<String, String>,
    counters: &Counters,
) {
    let body = video_body();
    let range = headers
        .get("range")
        .and_then(|value| parse_range(value, body.len()));

    match range {
        Some((start, end)) => {
            counters.ranges.fetch_add(1, Ordering::Relaxed);
            let slice = &body[start..=end];
            let content_range = format!("bytes {start}-{end}/{}", body.len());
            write_response(
                socket,
                206,
                &[
                    ("Content-Type", "video/mp4"),
                    ("Content-Range", &content_range),
                    ("Accept-Ranges", "bytes"),
                    ("ETag", "\"video-v1\""),
                ],
                slice,
            )
            .await;
        }
        None => {
            write_response(
                socket,
                200,
                &[
                    ("Content-Type", "video/mp4"),
                    ("Accept-Ranges", "bytes"),
                    ("ETag", "\"video-v1\""),
                ],
                &body,
            )
            .await;
        }
    }
}

fn video_body() -> Vec<u8> {
    let mut body = vec![0u8; VIDEO_SIZE];
    body[4..8].copy_from_slice(b"ftyp");
    for (index, byte) in body.iter_mut().enumerate().skip(8) {
        *byte = (index % 251) as u8;
    }
    body
}

fn jpeg(seed: u8) -> Vec<u8> {
    let mut body = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F'];
    body.extend(std::iter::repeat_n(seed, 2048));
    body.extend_from_slice(&[0xFF, 0xD9]);
    body
}

fn mp3() -> Vec<u8> {
    let mut body = b"ID3\x04\x00\x00\x00\x00\x00\x00".to_vec();
    body.extend(std::iter::repeat_n(0x55, 2048));
    body
}

fn video_page(origin: &str, id: &str) -> String {
    let media = format!("{origin}/media/video.mp4");
    let audio = format!("{origin}/media/audio.mp3");
    format!(
        r#"<!doctype html><html><body>
<script id="__UNIVERSAL_DATA_FOR_REHYDRATION__" type="application/json">
{{
  "__DEFAULT_SCOPE__": {{
    "webapp.video-detail": {{
      "statusMsg": "",
      "itemInfo": {{
        "itemStruct": {{
          "id": "{id}",
          "desc": "integration video",
          "author": {{ "uniqueId": "test", "nickname": "Test" }},
          "music": {{ "playUrl": "{audio}", "title": "test track" }},
          "video": {{
            "playAddr": "{media}",
            "width": 720,
            "height": 1280,
            "duration": 10,
            "bitrateInfo": [
              {{
                "Bitrate": 900000,
                "CodecType": "h264",
                "Width": 720,
                "Height": 1280,
                "PlayAddr": {{ "UrlList": ["{media}"] }}
              }}
            ]
          }},
          "isContentClassified": false
        }}
      }}
    }}
  }}
}}
</script>
</body></html>"#
    )
}

fn embed_video_page(origin: &str) -> String {
    let media = format!("{origin}/media/embed.mp4");
    format!(
        r#"<!doctype html><html><body>
<script id="__FRONTITY_CONNECT_STATE__" type="application/json">
{{
  "source": {{
    "data": {{
      "/embed/v2/123": {{
        "videoData": {{
          "itemInfos": {{
            "id": "123",
            "text": "integration embed",
            "authorId": "test",
            "video": {{
              "urls": ["{media}"],
              "videoMeta": {{ "width": 720, "height": 1280, "duration": 10 }}
            }}
          }}
        }}
      }}
    }}
  }}
}}
</script>
</body></html>"#
    )
}

fn gallery_page(origin: &str) -> String {
    let first = format!("{origin}/media/p1.jpg");
    let second = format!("{origin}/media/p2.jpg");
    let bad = format!("{origin}/media/bad.jpg");
    format!(
        r#"<!doctype html><html><body>
<script id="__UNIVERSAL_DATA_FOR_REHYDRATION__" type="application/json">
{{
  "__DEFAULT_SCOPE__": {{
    "webapp.video-detail": {{
      "statusMsg": "",
      "itemInfo": {{
        "itemStruct": {{
          "id": "456",
          "desc": "integration gallery",
          "author": {{ "uniqueId": "test" }},
          "imagePost": {{
            "images": [
              {{ "imageURL": {{ "urlList": ["{first}"] }}, "imageWidth": 640, "imageHeight": 640 }},
              {{ "imageURL": {{ "urlList": ["{second}"] }}, "imageWidth": 640, "imageHeight": 640 }},
              {{ "imageURL": {{ "urlList": ["{bad}"] }}, "imageWidth": 640, "imageHeight": 640 }}
            ]
          }},
          "isContentClassified": false
        }}
      }}
    }}
  }}
}}
</script>
</body></html>"#
    )
}

async fn read_request(socket: &mut TcpStream) -> Option<String> {
    let mut buffer = Vec::with_capacity(8192);
    let mut chunk = [0u8; 4096];
    loop {
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.windows(4).any(|window| window == b"\r\n\r\n") || buffer.len() > 64 * 1024 {
            break;
        }
    }
    String::from_utf8(buffer).ok()
}

fn parse_request_line(request: &str) -> Option<(&str, &str)> {
    let line = request.lines().next()?;
    let mut parts = line.split_whitespace();
    Some((parts.next()?, parts.next()?))
}

fn parse_headers(request: &str) -> HashMap<String, String> {
    let mut headers = HashMap::new();
    for line in request.lines().skip(1) {
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    headers
}

fn has_session_cookie(headers: &HashMap<String, String>) -> bool {
    headers
        .get("cookie")
        .is_some_and(|cookie| cookie.contains("tt_chain_token=test"))
}

fn parse_range(value: &str, length: usize) -> Option<(usize, usize)> {
    let value = value.trim().strip_prefix("bytes=")?;
    let (start, end) = value.split_once('-')?;
    let start = start.parse::<usize>().ok()?;
    if start >= length {
        return None;
    }
    let end = if end.is_empty() {
        length - 1
    } else {
        end.parse::<usize>().ok()?.min(length - 1)
    };
    (start <= end).then_some((start, end))
}

async fn write_response(
    socket: &mut TcpStream,
    status: u16,
    headers: &[(&str, &str)],
    body: &[u8],
) {
    let reason = match status {
        200 => "OK",
        206 => "Partial Content",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        _ => "Unknown",
    };
    let mut response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in headers {
        response.push_str(name);
        response.push_str(": ");
        response.push_str(value);
        response.push_str("\r\n");
    }
    response.push_str("\r\n");
    let _ = socket.write_all(response.as_bytes()).await;
    let _ = socket.write_all(body).await;
    let _ = socket.shutdown().await;
}
