//! Minimal RTSP/1.0 message framing for the AirPlay control channel.
//!
//! AirPlay's control session is a sequence of RTSP-style request/response exchanges over
//! a single TCP connection (`POST /pair-setup`, `SETUP`, `RECORD`, `SET_PARAMETER`, …).
//! This module is the deterministic codec for those messages — building requests and
//! parsing responses — independent of the socket and the higher-level handshake, so it is
//! fully unit-testable without a device.

use std::net::SocketAddr;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use hc_core::{Error, Result};

/// A TCP control connection that exchanges RTSP messages with a receiver, owning the
/// `CSeq` counter and buffering partial reads until a full response is parsed.
pub struct RtspConnection {
    stream: TcpStream,
    buf: Vec<u8>,
    cseq: u32,
}

impl RtspConnection {
    /// Open a control connection to `addr` (the device's AirPlay control endpoint).
    pub async fn connect(addr: SocketAddr) -> Result<Self> {
        let stream = TcpStream::connect(addr)
            .await
            .map_err(|e| Error::DeviceUnreachable(format!("RTSP connect to {addr}: {e}")))?;
        Ok(Self::from_stream(stream))
    }

    /// Wrap an already-connected stream.
    #[must_use]
    pub fn from_stream(stream: TcpStream) -> Self {
        Self {
            stream,
            buf: Vec::new(),
            cseq: 0,
        }
    }

    /// The next `CSeq` value that [`request`](Self::request) will use.
    #[must_use]
    pub fn next_cseq(&self) -> u32 {
        self.cseq + 1
    }

    /// Send `req` (adding an incrementing `CSeq` header) and await the full response.
    ///
    /// The caller must not set `CSeq` itself; this method owns the sequence counter.
    pub async fn request(&mut self, req: RtspRequest) -> Result<RtspResponse> {
        self.cseq += 1;
        let req = req.header("CSeq", self.cseq.to_string());

        self.stream
            .write_all(&req.serialize())
            .await
            .map_err(|e| Error::Sink(format!("RTSP write: {e}")))?;
        self.stream
            .flush()
            .await
            .map_err(|e| Error::Sink(format!("RTSP flush: {e}")))?;

        let mut chunk = [0u8; 4096];
        loop {
            if let Some((resp, consumed)) = RtspResponse::parse(&self.buf)? {
                self.buf.drain(..consumed);
                return Ok(resp);
            }
            let n = self
                .stream
                .read(&mut chunk)
                .await
                .map_err(|e| Error::Sink(format!("RTSP read: {e}")))?;
            if n == 0 {
                return Err(Error::Protocol(
                    "connection closed before a complete RTSP response".into(),
                ));
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }
}

/// An RTSP request to send to the receiver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtspRequest {
    /// RTSP method (e.g. `POST`, `SETUP`, `RECORD`, `GET_PARAMETER`).
    pub method: String,
    /// Request URI (e.g. `/pair-setup`, `rtsp://host/stream`, or `*`).
    pub uri: String,
    /// Ordered headers; insertion order is preserved on the wire.
    pub headers: Vec<(String, String)>,
    /// Message body (may be empty).
    pub body: Vec<u8>,
}

impl RtspRequest {
    /// Create a request with no headers or body.
    #[must_use]
    pub fn new(method: impl Into<String>, uri: impl Into<String>) -> Self {
        Self {
            method: method.into(),
            uri: uri.into(),
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    /// Append a header (builder style).
    #[must_use]
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Attach a body, setting `Content-Type` and `Content-Length` (replacing any existing
    /// values of those headers).
    #[must_use]
    pub fn with_body(mut self, content_type: &str, body: Vec<u8>) -> Self {
        self.headers.retain(|(k, _)| {
            !k.eq_ignore_ascii_case("content-type") && !k.eq_ignore_ascii_case("content-length")
        });
        self.headers
            .push(("Content-Type".to_string(), content_type.to_string()));
        self.headers
            .push(("Content-Length".to_string(), body.len().to_string()));
        self.body = body;
        self
    }

    /// Serialize to bytes: request line, headers, blank line, body.
    #[must_use]
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(format!("{} {} RTSP/1.0\r\n", self.method, self.uri).as_bytes());
        for (k, v) in &self.headers {
            out.extend_from_slice(format!("{k}: {v}\r\n").as_bytes());
        }
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(&self.body);
        out
    }
}

/// A parsed RTSP response from the receiver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtspResponse {
    /// Numeric status code (e.g. 200).
    pub status: u16,
    /// Reason phrase (e.g. `OK`).
    pub reason: String,
    /// Response headers in order.
    pub headers: Vec<(String, String)>,
    /// Response body (length per `Content-Length`).
    pub body: Vec<u8>,
}

impl RtspResponse {
    /// Case-insensitive header lookup.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Whether the status code is 2xx.
    #[must_use]
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// Try to parse one complete response from the front of `buf`.
    ///
    /// Returns `Ok(None)` if `buf` does not yet contain a full message (caller should
    /// read more bytes and retry), `Ok(Some((response, consumed)))` on success where
    /// `consumed` is the number of bytes the message occupied, or `Err` if the bytes are
    /// not a valid RTSP message.
    pub fn parse(buf: &[u8]) -> Result<Option<(RtspResponse, usize)>> {
        let Some(head_end) = find_subsequence(buf, b"\r\n\r\n") else {
            return Ok(None); // headers not fully received yet
        };
        let head = std::str::from_utf8(&buf[..head_end])
            .map_err(|_| Error::Protocol("RTSP header is not valid UTF-8".into()))?;

        let mut lines = head.split("\r\n");
        let status_line = lines
            .next()
            .ok_or_else(|| Error::Protocol("empty RTSP response".into()))?;

        // "RTSP/1.0 200 OK"
        let mut parts = status_line.splitn(3, ' ');
        let version = parts.next().unwrap_or_default();
        if !version.starts_with("RTSP/") {
            return Err(Error::Protocol(format!(
                "invalid RTSP status line: {status_line:?}"
            )));
        }
        let status: u16 = parts
            .next()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| Error::Protocol(format!("invalid RTSP status code: {status_line:?}")))?;
        let reason = parts.next().unwrap_or_default().to_string();

        let mut headers = Vec::new();
        for line in lines {
            if line.is_empty() {
                continue;
            }
            if let Some((k, v)) = line.split_once(':') {
                headers.push((k.trim().to_string(), v.trim().to_string()));
            } else {
                return Err(Error::Protocol(format!("malformed RTSP header: {line:?}")));
            }
        }

        let content_len: usize = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, v)| v.parse().ok())
            .unwrap_or(0);

        let body_start = head_end + 4; // skip the CRLFCRLF
        let body_end = body_start + content_len;
        if buf.len() < body_end {
            return Ok(None); // body not fully received yet
        }

        Ok(Some((
            RtspResponse {
                status,
                reason,
                headers,
                body: buf[body_start..body_end].to_vec(),
            },
            body_end,
        )))
    }
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_request_with_headers() {
        let bytes = RtspRequest::new("GET_PARAMETER", "rtsp://10.0.0.1/stream")
            .header("CSeq", "3")
            .header("User-Agent", "HorizonCast/0.0")
            .serialize();
        let text = String::from_utf8(bytes).unwrap();
        assert_eq!(
            text,
            "GET_PARAMETER rtsp://10.0.0.1/stream RTSP/1.0\r\n\
             CSeq: 3\r\n\
             User-Agent: HorizonCast/0.0\r\n\
             \r\n"
        );
    }

    #[test]
    fn with_body_sets_content_headers_and_replaces_duplicates() {
        let req = RtspRequest::new("POST", "/pair-setup")
            .header("Content-Length", "999") // stale, must be replaced
            .with_body("application/octet-stream", vec![1, 2, 3, 4]);
        let text = String::from_utf8(req.serialize()).unwrap();
        assert!(text.contains("POST /pair-setup RTSP/1.0\r\n"));
        assert!(text.contains("Content-Type: application/octet-stream\r\n"));
        assert!(text.contains("Content-Length: 4\r\n"));
        // Only one Content-Length header (the stale 999 was removed).
        assert_eq!(text.matches("Content-Length:").count(), 1);
        assert!(text.ends_with("\r\n\r\n\u{1}\u{2}\u{3}\u{4}"));
    }

    #[test]
    fn parses_response_with_body() {
        let raw = b"RTSP/1.0 200 OK\r\nCSeq: 3\r\nContent-Length: 5\r\n\r\nhello";
        let (resp, consumed) = RtspResponse::parse(raw).unwrap().expect("complete message");
        assert_eq!(consumed, raw.len());
        assert_eq!(resp.status, 200);
        assert_eq!(resp.reason, "OK");
        assert!(resp.is_success());
        assert_eq!(resp.header("cseq"), Some("3")); // case-insensitive
        assert_eq!(resp.body, b"hello");
    }

    #[test]
    fn parses_response_without_body() {
        let raw = b"RTSP/1.0 200 OK\r\nCSeq: 1\r\n\r\n";
        let (resp, consumed) = RtspResponse::parse(raw).unwrap().unwrap();
        assert_eq!(consumed, raw.len());
        assert!(resp.body.is_empty());
    }

    #[test]
    fn returns_none_when_headers_incomplete() {
        let raw = b"RTSP/1.0 200 OK\r\nCSeq: 1\r\n"; // no terminating CRLFCRLF
        assert_eq!(RtspResponse::parse(raw).unwrap(), None);
    }

    #[test]
    fn returns_none_when_body_incomplete() {
        let raw = b"RTSP/1.0 200 OK\r\nContent-Length: 10\r\n\r\nshort";
        assert_eq!(RtspResponse::parse(raw).unwrap(), None);
    }

    #[test]
    fn reports_consumed_so_pipelined_messages_can_follow() {
        let mut raw = b"RTSP/1.0 200 OK\r\nContent-Length: 2\r\n\r\nAB".to_vec();
        let trailing = b"RTSP/1.0 100 Continue\r\n\r\n";
        raw.extend_from_slice(trailing);
        let (first, consumed) = RtspResponse::parse(&raw).unwrap().unwrap();
        assert_eq!(first.body, b"AB");
        // The next message starts exactly where the first ended.
        let (second, _) = RtspResponse::parse(&raw[consumed..]).unwrap().unwrap();
        assert_eq!(second.status, 100);
    }

    #[test]
    fn rejects_non_rtsp_status_line() {
        let raw = b"HTTP/1.1 200 OK\r\n\r\n";
        assert!(RtspResponse::parse(raw).is_err());
    }

    #[test]
    fn rejects_bad_status_code() {
        let raw = b"RTSP/1.0 NOTACODE OK\r\n\r\n";
        assert!(RtspResponse::parse(raw).is_err());
    }

    // ---- connection driver ----

    use std::net::Ipv4Addr;
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;

    /// Mock RTSP server: accepts one connection, reads the request up to the header
    /// terminator, replies with `response`, and reports the bytes it received.
    async fn spawn_mock_rtsp(response: &'static [u8]) -> (SocketAddr, oneshot::Receiver<Vec<u8>>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let mut got = Vec::new();
            let mut buf = [0u8; 1024];
            loop {
                let n = sock.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                got.extend_from_slice(&buf[..n]);
                if got.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let _ = sock.write_all(response).await;
            let _ = sock.flush().await;
            let _ = tx.send(got);
        });
        (addr, rx)
    }

    #[tokio::test]
    async fn connection_sends_request_with_cseq_and_parses_response() {
        let (addr, captured) =
            spawn_mock_rtsp(b"RTSP/1.0 200 OK\r\nCSeq: 1\r\nContent-Length: 2\r\n\r\nok").await;

        let mut conn = RtspConnection::connect(addr).await.unwrap();
        assert_eq!(conn.next_cseq(), 1);
        let resp = conn
            .request(RtspRequest::new("OPTIONS", "*"))
            .await
            .unwrap();

        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, b"ok");
        assert_eq!(conn.next_cseq(), 2, "CSeq must advance after a request");

        // The driver added a CSeq header to what it sent.
        let sent = String::from_utf8(captured.await.unwrap()).unwrap();
        assert!(sent.starts_with("OPTIONS * RTSP/1.0\r\n"));
        assert!(sent.contains("CSeq: 1\r\n"));
    }
}
