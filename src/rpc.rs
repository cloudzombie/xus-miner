//! Bounded HTTP/JSON-RPC transport for the documented direct-node contract.
use serde_json::{json, Value};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

const MAX_HEADER_BYTES: usize = 32 * 1024;
// Templates are hex-expanded on the wire. Preserve the existing body budget.
const MAX_BODY_BYTES: usize = super::MAX_BLOB_BYTES * 2 + 256 * 1024;

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

pub(super) fn remaining(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "node RPC deadline exceeded"))
}

// Socket timeouts alone are idle timeouts. Recompute the remaining budget
// before EVERY syscall, including those inside read_exact/write_all, so a
// trickling peer cannot extend the deadline by repeatedly making progress.
struct DeadlineStream {
    stream: TcpStream,
    deadline: Instant,
}

fn normalize_timeout<T>(result: io::Result<T>) -> io::Result<T> {
    result.map_err(|error| {
        if matches!(
            error.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        ) {
            io::Error::new(io::ErrorKind::TimedOut, "node RPC deadline exceeded")
        } else {
            error
        }
    })
}

impl Read for DeadlineStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.stream
            .set_read_timeout(Some(remaining(self.deadline)?))?;
        normalize_timeout(self.stream.read(buffer))
    }
}

impl Write for DeadlineStream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.stream
            .set_write_timeout(Some(remaining(self.deadline)?))?;
        normalize_timeout(self.stream.write(buffer))
    }

    fn flush(&mut self) -> io::Result<()> {
        remaining(self.deadline)?;
        self.stream.flush()
    }
}

pub(super) fn call(
    address: &str,
    method: &str,
    params: Value,
    timeout: Duration,
) -> io::Result<Value> {
    let deadline = Instant::now() + timeout;
    let stream = super::connect_with_timeout(address, remaining(deadline)?)?;
    let mut stream = DeadlineStream { stream, deadline };
    let body = json!({"jsonrpc":"2.0", "id":1, "method":method, "params":params}).to_string();
    write!(stream,
        "POST / HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len())?;
    stream.flush()?;
    let body = read_response(&mut BufReader::new(stream))?;
    remaining(deadline)?;
    decode_reply(&body, method)
}

fn header_line<R: BufRead>(reader: &mut R, used: &mut usize) -> io::Result<String> {
    let mut bytes = Vec::new();
    reader
        .take((MAX_HEADER_BYTES - *used + 1) as u64)
        .read_until(b'\n', &mut bytes)?;
    *used += bytes.len();
    if *used > MAX_HEADER_BYTES {
        return Err(invalid("node RPC HTTP headers exceed the safety limit"));
    }
    if !bytes.ends_with(b"\r\n") {
        return Err(invalid(
            "node returned incomplete or malformed HTTP headers",
        ));
    }
    bytes.truncate(bytes.len() - 2);
    String::from_utf8(bytes).map_err(|_| invalid("node returned non-UTF-8 HTTP headers"))
}

fn read_response<R: BufRead>(reader: &mut R) -> io::Result<Vec<u8>> {
    let mut used = 0;
    let status_line = header_line(reader, &mut used)?;
    let mut status = status_line.splitn(3, ' ');
    let version = status.next().unwrap_or_default();
    let code = status.next().unwrap_or_default();
    if !matches!(version, "HTTP/1.0" | "HTTP/1.1")
        || code.len() != 3
        || !code.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(invalid("node returned malformed HTTP status"));
    }
    if !code.starts_with('2') {
        return Err(invalid(format!("node RPC returned HTTP status {code}")));
    }
    let mut content_length = None;
    loop {
        let line = header_line(reader, &mut used)?;
        if line.is_empty() {
            break;
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| invalid("node returned malformed HTTP header"))?;
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
            || value
                .bytes()
                .any(|byte| byte.is_ascii_control() && byte != b'\t')
        {
            return Err(invalid("node returned malformed HTTP header"));
        }
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            if content_length.is_some()
                || value.is_empty()
                || !value.bytes().all(|b| b.is_ascii_digit())
            {
                return Err(invalid("node returned invalid or duplicate Content-Length"));
            }
            let length = value
                .parse::<usize>()
                .map_err(|_| invalid("node returned invalid Content-Length"))?;
            if length > MAX_BODY_BYTES {
                return Err(invalid("node RPC body exceeds the safety limit"));
            }
            content_length = Some(length);
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            // Chunked responses were never supported by this client. Refuse
            // explicitly rather than misinterpreting an ambiguous message.
            return Err(invalid("node RPC Transfer-Encoding is unsupported"));
        } else if name.eq_ignore_ascii_case("content-encoding")
            && !value.eq_ignore_ascii_case("identity")
        {
            return Err(invalid("node RPC Content-Encoding is unsupported"));
        }
    }
    if let Some(length) = content_length {
        let mut body = vec![0; length];
        reader.read_exact(&mut body)?;
        Ok(body)
    } else {
        // HTTP/1.x close-delimited replies remain compatible. The enclosing
        // DeadlineStream bounds their duration even if the peer never closes.
        let mut body = Vec::new();
        reader
            .take((MAX_BODY_BYTES + 1) as u64)
            .read_to_end(&mut body)?;
        if body.len() > MAX_BODY_BYTES {
            return Err(invalid("node RPC body exceeds the safety limit"));
        }
        Ok(body)
    }
}

fn decode_reply(body: &[u8], method: &str) -> io::Result<Value> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|error| invalid(format!("node returned invalid JSON: {error}")))?;
    if value.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
        || value.get("id").and_then(Value::as_u64) != Some(1)
    {
        return Err(invalid(
            "node RPC reply has invalid JSON-RPC version or mismatched id",
        ));
    }
    if !value.get("error").is_none_or(Value::is_null) {
        return Err(io::Error::other(format!(
            "node RPC {method} failed: {}",
            value["error"]
        )));
    }
    value
        .get("result")
        .cloned()
        .ok_or_else(|| invalid("node RPC reply has no result"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn parse(response: &[u8]) -> io::Result<Vec<u8>> {
        read_response(&mut Cursor::new(response))
    }

    #[test]
    fn accepts_length_and_close_delimited_http_without_consuming_the_next_message() {
        for version in ["HTTP/1.0", "HTTP/1.1"] {
            assert_eq!(
                parse(format!("{version} 200 OK\r\n\r\nbody").as_bytes()).unwrap(),
                b"body"
            );
            let response = format!("{version} 200 OK\r\ncOnTeNt-LeNgTh: 4\r\n\r\nbodyNEXT");
            let mut reader = Cursor::new(response.as_bytes());
            assert_eq!(read_response(&mut reader).unwrap(), b"body");
            assert_eq!(&response.as_bytes()[reader.position() as usize..], b"NEXT");
        }
    }

    #[test]
    fn rejects_malformed_status_headers_and_ambiguous_framing() {
        for response in [
            "garbage 200 OK\r\n\r\n",
            "HTTP/1.1 20 OK\r\n\r\n",
            "HTTP/1.1 2000 OK\r\n\r\n",
            "HTTP/1.1 503 Unavailable\r\n\r\n",
            "HTTP/1.1 302 Redirect\r\n\r\n",
            "HTTP/1.1 200 OK\n\n",
            "HTTP/1.1 200 OK\r\nMissing-Colon\r\n\r\n",
            "HTTP/1.1 200 OK\r\n Content-Length: 0\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length: +1\r\n\r\nx",
            "HTTP/1.1 200 OK\r\nContent-Length: -1\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length: 1, 1\r\n\r\nx",
            "HTTP/1.1 200 OK\r\nContent-Length: \r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length: 0\r\ncontent-length: 0\r\n\r\n",
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nTransfer-Encoding: chunked\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\n\r\n",
            "HTTP/1.1 200 OK\r\nX-Test: bad\0value\r\n\r\n",
        ] {
            assert_eq!(
                parse(response.as_bytes()).unwrap_err().kind(),
                io::ErrorKind::InvalidData,
                "{response:?}"
            );
        }
        assert_eq!(
            parse(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nabc")
                .unwrap_err()
                .kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn header_limit_is_enforced_before_reading_an_unbounded_body() {
        let prefix = "HTTP/1.1 200 OK\r\nX-Pad: ";
        let suffix = "\r\nContent-Length: 0\r\n\r\n";
        let response = format!(
            "{prefix}{}{suffix}",
            "x".repeat(MAX_HEADER_BYTES - prefix.len() - suffix.len())
        );
        assert!(parse(response.as_bytes()).unwrap().is_empty());

        // No newline: the reader must stop after the header budget, rather
        // than draining up to the much larger body budget.
        let response = format!("{prefix}{}", "x".repeat(MAX_HEADER_BYTES * 2));
        let mut reader = Cursor::new(response.as_bytes());
        let error = read_response(&mut reader).unwrap_err();
        assert!(error.to_string().contains("headers exceed"));
        assert_eq!(reader.position(), (MAX_HEADER_BYTES + 1) as u64);
    }

    #[test]
    fn body_limit_covers_both_framing_modes_and_rejects_large_lengths_early() {
        for length_delimited in [false, true] {
            let header = if length_delimited {
                format!("HTTP/1.1 200 OK\r\nContent-Length: {MAX_BODY_BYTES}\r\n\r\n")
            } else {
                "HTTP/1.1 200 OK\r\n\r\n".to_owned()
            };
            let mut response = header.into_bytes();
            response.resize(response.len() + MAX_BODY_BYTES, b'x');
            assert_eq!(parse(&response).unwrap().len(), MAX_BODY_BYTES);
        }
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY_BYTES + 1
        );
        assert!(parse(header.as_bytes())
            .unwrap_err()
            .to_string()
            .contains("safety limit"));
        let mut response = b"HTTP/1.1 200 OK\r\n\r\n".to_vec();
        response.resize(response.len() + MAX_BODY_BYTES + 1, b'x');
        assert!(parse(&response)
            .unwrap_err()
            .to_string()
            .contains("safety limit"));
    }

    #[test]
    fn json_rpc_envelope_requires_the_requested_id_version_and_result() {
        for reply in [
            json!({"jsonrpc":"2.0", "id":2, "result":42}),
            json!({"jsonrpc":"2.0", "id":"1", "result":42}),
            json!({"jsonrpc":"2.0", "id":null, "result":42}),
            json!({"jsonrpc":"1.0", "id":1, "result":42}),
            json!({"id":1, "result":42}),
            json!({"jsonrpc":"2.0", "id":1}),
            json!([{"jsonrpc":"2.0", "id":1, "result":42}]),
        ] {
            assert_eq!(
                decode_reply(reply.to_string().as_bytes(), "sov_health")
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidData,
                "{reply}"
            );
        }
        for result in [json!(42), Value::Null, json!({"height":42})] {
            let reply = json!({"jsonrpc":"2.0", "id":1, "result":result, "error":null});
            assert_eq!(
                decode_reply(reply.to_string().as_bytes(), "sov_health").unwrap(),
                result
            );
        }
        let error = json!({"jsonrpc":"2.0", "id":1, "error":{"code":-32601,"message":"method unavailable"}});
        let error = decode_reply(error.to_string().as_bytes(), "sov_health").unwrap_err();
        assert!(error.to_string().contains("sov_health failed"));
        assert!(error.to_string().contains("method unavailable"));
    }
}
