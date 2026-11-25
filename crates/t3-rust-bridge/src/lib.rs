//! Rust replacement for T3 Code's `acp-mcp-bridge` and `acp-mcp-call`.
//!
//! Behavioral parity target: `AcpMcpStdioBridge.ts` and `mcpResponsePayloads.ts`
//! in the t3code repository. JSON values are relayed/printed with ECMAScript
//! `JSON.stringify` semantics (insertion-ordered keys, shortest-round-trip
//! floats, U+2028/U+2029 left raw).

use std::sync::{Arc, Mutex as StdMutex};

use bytes::{Buf, Bytes, BytesMut};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper_util::client::legacy::{connect::HttpConnector, Client};
use hyper_util::rt::TokioExecutor;
use serde_json::{json, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{watch, Mutex};
use tokio::task::JoinSet;

const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

// Bound untrusted input at the process boundary. These limits are far above
// normal MCP traffic but prevent a peer from growing memory without bound.
const MAX_PAYLOAD_BYTES: usize = 64 * 1024 * 1024;
const MAX_IN_FLIGHT_REQUESTS: usize = 64;
const MAX_IN_FLIGHT_REQUEST_BYTES: usize = 2 * MAX_PAYLOAD_BYTES;
const MAX_SERVER_REQUEST_DEPTH: usize = 32;
const SSE_SEPARATOR_OVERLAP: usize = 3;

// ---------------------------------------------------------------------------
// JSON.stringify-compatible serialization
// ---------------------------------------------------------------------------

/// Serialize a `serde_json::Value` the way `JSON.stringify` would.
pub fn js_stringify(value: &Value) -> String {
    let mut out = String::new();
    write_js(value, &mut out);
    out
}

fn write_js(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                out.push_str(&i.to_string());
            } else if let Some(u) = n.as_u64() {
                out.push_str(&u.to_string());
            } else if let Some(f) = n.as_f64() {
                out.push_str(&js_float(f));
            }
        }
        Value::String(s) => write_js_string(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_js(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (i, (k, v)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_js_string(k, out);
                out.push(':');
                write_js(v, out);
            }
            out.push('}');
        }
    }
}

fn write_js_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{9}' => out.push_str("\\t"),
            '\u{a}' => out.push_str("\\n"),
            '\u{c}' => out.push_str("\\f"),
            '\u{d}' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            // JSON.stringify emits U+2028/U+2029 raw; do the same.
            c => out.push(c),
        }
    }
    out.push('"');
}

/// ECMAScript Number::toString (radix 10) for finite f64 values.
fn js_float(v: f64) -> String {
    if v == 0.0 || !v.is_finite() {
        // -0 stringifies as "0"; non-finite cannot appear in parsed JSON,
        // and JSON.stringify maps them to null anyway.
        return "0".to_string();
    }
    // Rust's LowerExp on f64 prints the shortest round-trip mantissa, e.g.
    // "1.5e-8", "1e21". Parse it into digits + decimal exponent.
    let repr = format!("{:e}", v.abs());
    let (mantissa, exp) = repr.split_once('e').expect("LowerExp output");
    let e: i32 = exp.parse().expect("LowerExp exponent");
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let k = digits.len() as i32; // number of significant digits
    let n = e + 1; // position of decimal point after first digit
    let mut out = String::new();
    if v < 0.0 {
        out.push('-');
    }
    if n >= k && n <= 21 {
        out.push_str(&digits);
        out.extend(std::iter::repeat_n('0', (n - k) as usize));
    } else if n > 0 && n <= 21 {
        out.push_str(&digits[..n as usize]);
        out.push('.');
        out.push_str(&digits[n as usize..]);
    } else if n > -6 && n <= 0 {
        out.push_str("0.");
        out.extend(std::iter::repeat_n('0', (-n) as usize));
        out.push_str(&digits);
    } else {
        out.push_str(&digits[..1]);
        if k > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        if n > 0 {
            out.push('+');
        } else {
            out.push('-');
        }
        out.push_str(&(n - 1).abs().to_string());
    }
    out
}

// ---------------------------------------------------------------------------
// Shared session state
// ---------------------------------------------------------------------------

#[derive(Default)]
struct SharedState {
    session_id: StdMutex<Option<String>>,
    protocol_version: StdMutex<Option<String>>,
}

fn shared_lock<T>(mutex: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    // Session metadata is small and independent, so a poisoned lock is still
    // safe to inspect. Avoid turning an unrelated panic into a bridge-wide one.
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl SharedState {
    fn set_session_id(&self, id: &str) {
        *shared_lock(&self.session_id) = Some(id.to_string());
    }

    fn apply_headers(&self, mut builder: http::request::Builder) -> http::request::Builder {
        if let Some(sid) = shared_lock(&self.session_id).clone() {
            builder = builder.header("mcp-session-id", sid);
        }
        if let Some(pv) = shared_lock(&self.protocol_version).clone() {
            builder = builder.header("mcp-protocol-version", pv);
        }
        builder
    }
}

fn protocol_version_of(value: &Value) -> Option<&str> {
    value
        .get("result")?
        .get("protocolVersion")?
        .as_str()
        .filter(|s| !s.is_empty() && http::header::HeaderValue::from_str(s).is_ok())
}

fn update_protocol_version(state: &SharedState, value: &Value) {
    if let Some(pv) = protocol_version_of(value) {
        *shared_lock(&state.protocol_version) = Some(pv.to_string());
    }
}

// ---------------------------------------------------------------------------
// HTTP plumbing
// ---------------------------------------------------------------------------

type HttpClient = Client<HttpConnector, Full<Bytes>>;

fn http_client() -> HttpClient {
    Client::builder(TokioExecutor::new()).build(HttpConnector::new())
}

fn base_headers(builder: http::request::Builder, authorization: &str) -> http::request::Builder {
    builder
        .header("accept", "application/json, text/event-stream")
        .header("authorization", authorization)
}

async fn http_post(
    client: &HttpClient,
    endpoint: &str,
    state: &SharedState,
    authorization: &str,
    body: Bytes,
) -> Result<http::Response<Incoming>, String> {
    let builder = state.apply_headers(base_headers(
        http::Request::builder()
            .method("POST")
            .uri(endpoint)
            .header("content-type", "application/json"),
        authorization,
    ));
    let request = builder.body(Full::new(body)).map_err(|e| e.to_string())?;
    let response = client.request(request).await.map_err(|e| e.to_string())?;
    if let Some(sid) = response
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
    {
        state.set_session_id(sid);
    }
    Ok(response)
}

async fn http_delete(
    client: &HttpClient,
    endpoint: &str,
    state: &SharedState,
    authorization: &str,
) {
    let builder = state.apply_headers(base_headers(
        http::Request::builder().method("DELETE").uri(endpoint),
        authorization,
    ));
    if let Ok(request) = builder.body(Full::new(Bytes::new())) {
        if let Ok(response) = client.request(request).await {
            let _ = collect_body_limited(response.into_body()).await;
        }
    }
}

// ---------------------------------------------------------------------------
// Response payload reader (JSON body or incremental SSE)
// ---------------------------------------------------------------------------

fn response_too_large() -> String {
    format!(
        "T3 Code MCP response payload exceeded {} bytes.",
        MAX_PAYLOAD_BYTES
    )
}

async fn collect_body_limited(mut body: Incoming) -> Result<Bytes, String> {
    let mut bytes = BytesMut::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|e| e.to_string())?;
        if let Ok(data) = frame.into_data() {
            if bytes.len().saturating_add(data.len()) > MAX_PAYLOAD_BYTES {
                return Err(response_too_large());
            }
            bytes.extend_from_slice(&data);
        }
    }
    Ok(bytes.freeze())
}

/// Incremental SSE event splitter. It keeps the unscanned tail small, matching
/// the linear-time behavior of the TypeScript implementation even when a large
/// event arrives in many small network frames.
#[derive(Default)]
struct SseParser {
    buf: BytesMut,
    scan_pos: usize,
}

impl SseParser {
    fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    fn buffered_len(&self) -> usize {
        self.buf.len()
    }

    fn next_event(&mut self) -> Result<Option<String>, String> {
        let start = self.scan_pos.min(self.buf.len());
        if let Some((end, consume)) = find_sse_separator(&self.buf[start..]) {
            let event_end = start + end;
            if event_end > MAX_PAYLOAD_BYTES {
                return Err(response_too_large());
            }
            let consume_end = start + consume;
            let event = String::from_utf8_lossy(&self.buf[..event_end]).into_owned();
            self.buf.advance(consume_end);
            self.scan_pos = 0;
            return Ok(Some(event));
        }
        // A separator is at most four bytes, so a separator completed by the
        // next frame must begin within the last three bytes already buffered.
        self.scan_pos = self.buf.len().saturating_sub(SSE_SEPARATOR_OVERLAP);
        Ok(None)
    }
}

struct PayloadReader {
    body: Option<Incoming>,
    sse: bool,
    parser: SseParser,
    done: bool,
}

impl PayloadReader {
    fn new(response: http::Response<Incoming>) -> Self {
        let status = response.status().as_u16();
        let empty = status == 202 || status == 204;
        let sse = response
            .headers()
            .get(http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|v| {
                v.split(';')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .eq_ignore_ascii_case("text/event-stream")
            })
            .unwrap_or(false);
        let body = if empty {
            None
        } else {
            Some(response.into_body())
        };
        Self {
            body,
            sse,
            parser: SseParser::default(),
            done: empty,
        }
    }

    /// Next parsed JSON payload. SSE yields one payload per event; a JSON body
    /// yields a single payload (which may itself be an array). A leftover
    /// partial SSE event at EOF is discarded, matching the TS implementation.
    async fn next_payload(&mut self) -> Result<Option<Value>, String> {
        if self.done {
            return Ok(None);
        }
        if !self.sse {
            let Some(body) = self.body.take() else {
                self.done = true;
                return Ok(None);
            };
            self.done = true;
            let bytes = collect_body_limited(body).await?;
            let text = String::from_utf8_lossy(&bytes);
            if text.trim().is_empty() {
                return Ok(None);
            }
            return serde_json::from_str(&text)
                .map(Some)
                .map_err(|e| e.to_string());
        }
        loop {
            while let Some(event) = self.parser.next_event()? {
                let data = sse_event_data(&event);
                if !data.is_empty() {
                    return serde_json::from_str(&data)
                        .map(Some)
                        .map_err(|e| e.to_string());
                }
            }
            if self.parser.buffered_len() > MAX_PAYLOAD_BYTES + SSE_SEPARATOR_OVERLAP {
                return Err(response_too_large());
            }
            let Some(body) = self.body.as_mut() else {
                self.done = true;
                return Ok(None);
            };
            match body.frame().await {
                None => {
                    self.done = true;
                    return Ok(None);
                }
                Some(Err(e)) => return Err(e.to_string()),
                Some(Ok(frame)) => {
                    if let Ok(data) = frame.into_data() {
                        self.parser.push(&data);
                    }
                }
            }
        }
    }
}

/// Length of a line break starting at `buf[i]`: `\r\n` -> 2, `\n`/`\r` -> 1.
/// The second tuple element flags ambiguity: a lone `\r` at the buffer end
/// could still become `\r\n` once more data arrives.
fn line_break_at(buf: &[u8], i: usize) -> Option<(usize, bool)> {
    match buf[i] {
        b'\n' => Some((1, false)),
        b'\r' => {
            if i + 1 >= buf.len() {
                Some((1, true))
            } else if buf[i + 1] == b'\n' {
                Some((2, false))
            } else {
                Some((1, false))
            }
        }
        _ => None,
    }
}

/// Locate the first SSE event separator (two consecutive line breaks).
/// Returns `(event_end, consume_end)`; ambiguous trailing `\r` waits for data.
fn find_sse_separator(buf: &[u8]) -> Option<(usize, usize)> {
    let mut i = 0;
    while i < buf.len() {
        let Some((len1, ambiguous)) = line_break_at(buf, i) else {
            i += 1;
            continue;
        };
        if ambiguous {
            return None;
        }
        let j = i + len1;
        if j >= buf.len() {
            return None;
        }
        if let Some((len2, ambiguous2)) = line_break_at(buf, j) {
            if ambiguous2 {
                return None;
            }
            return Some((i, j + len2));
        }
        i += 1;
    }
    None
}

/// Concatenate the `data:` fields of one raw SSE event block, matching
/// `sseEventData` in mcpResponsePayloads.ts.
fn sse_event_data(event: &str) -> String {
    let mut out = String::new();
    for line in event.split(['\r', '\n']) {
        if let Some(rest) = line.strip_prefix("data:") {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(rest.trim_start());
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Output helpers
// ---------------------------------------------------------------------------

fn error_envelope(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}

async fn write_value<W: AsyncWrite + Send + Unpin>(out: &Mutex<W>, value: &Value) {
    let mut line = js_stringify(value);
    line.push('\n');
    let mut guard = out.lock().await;
    let _ = guard.write_all(line.as_bytes()).await;
    let _ = guard.flush().await;
}

// ---------------------------------------------------------------------------
// acp-mcp-bridge
// ---------------------------------------------------------------------------

async fn forward<W: AsyncWrite + Send + Unpin>(
    client: &HttpClient,
    endpoint: &str,
    authorization: &str,
    state: &SharedState,
    out: &Mutex<W>,
    line: String,
    id: Option<&Value>,
) -> Result<(), String> {
    let response = http_post(client, endpoint, state, authorization, Bytes::from(line)).await?;
    if !response.status().is_success() {
        let status = response.status().as_u16();
        if let Some(id) = id {
            write_value(
                out,
                &error_envelope(
                    id.clone(),
                    -32603,
                    &format!("T3 Code MCP endpoint responded with HTTP {status}."),
                ),
            )
            .await;
        }
        let _ = collect_body_limited(response.into_body()).await;
        return Ok(());
    }
    let mut reader = PayloadReader::new(response);
    while let Some(payload) = reader.next_payload().await? {
        let entries: &[Value] = match &payload {
            Value::Array(items) => items.as_slice(),
            other => std::slice::from_ref(other),
        };
        for entry in entries {
            update_protocol_version(state, entry);
            write_value(out, entry).await;
        }
    }
    Ok(())
}

async fn run_forward<W: AsyncWrite + Send + Unpin>(
    client: Arc<HttpClient>,
    endpoint: String,
    authorization: String,
    state: Arc<SharedState>,
    out: Arc<Mutex<W>>,
    line: String,
    id: Option<Value>,
) {
    if let Err(message) = forward(
        &client,
        &endpoint,
        &authorization,
        &state,
        &out,
        line,
        id.as_ref(),
    )
    .await
    {
        if let Some(id) = id {
            write_value(
                &out,
                &error_envelope(
                    id,
                    -32603,
                    &format!("T3 Code MCP bridge request failed: {message}"),
                ),
            )
            .await;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BoundedLine {
    Eof,
    Line,
    TooLong,
}

/// Read one `\n`-terminated input line while bounding memory use. Bytes beyond
/// the limit are consumed and discarded so the next line is still framed
/// correctly. The newline itself is retained for compatibility with the
/// existing CRLF handling.
async fn read_line_bounded<R: AsyncBufRead + Unpin>(
    input: &mut R,
    raw: &mut Vec<u8>,
    max_content_bytes: usize,
) -> std::io::Result<BoundedLine> {
    raw.clear();
    let mut too_long = false;
    loop {
        let buf = input.fill_buf().await?;
        if buf.is_empty() {
            return Ok(if too_long {
                BoundedLine::TooLong
            } else if raw.is_empty() {
                BoundedLine::Eof
            } else {
                BoundedLine::Line
            });
        }
        let newline_pos = buf.iter().position(|b| *b == b'\n');
        let content_end = newline_pos.unwrap_or(buf.len());
        let consume_len = newline_pos.map(|pos| pos + 1).unwrap_or(buf.len());
        if !too_long {
            let room = max_content_bytes.saturating_sub(raw.len());
            let copy_len = content_end.min(room);
            raw.extend_from_slice(&buf[..copy_len]);
            if content_end > room {
                too_long = true;
            }
        }
        if newline_pos.is_some() && !too_long {
            raw.push(b'\n');
        }
        input.consume(consume_len);
        if newline_pos.is_some() {
            return Ok(if too_long {
                BoundedLine::TooLong
            } else {
                BoundedLine::Line
            });
        }
    }
}

async fn wait_done(mut done: watch::Receiver<bool>) {
    let _ = done.wait_for(|d| *d).await;
}

/// stdio-to-HTTP JSON-RPC relay. Mirrors `runAcpMcpStdioBridge`:
/// stdin is split on `\n` only, `initialize`/`notifications/initialized`
/// serialize through a session barrier, JSON-RPC responses bypass it, and all
/// other messages run concurrently after any pending barrier completes.
pub async fn run_bridge<R, W>(
    endpoint: String,
    authorization: String,
    mut input: R,
    output: Arc<Mutex<W>>,
) where
    R: AsyncBufRead + Unpin + Send + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let client = Arc::new(http_client());
    let state = Arc::new(SharedState::default());
    let mut requests = JoinSet::new();
    let mut replies = JoinSet::new();
    let request_bytes = Arc::new(tokio::sync::Semaphore::new(MAX_IN_FLIGHT_REQUEST_BYTES));
    let reply_bytes = Arc::new(tokio::sync::Semaphore::new(MAX_PAYLOAD_BYTES));
    let mut pending_barrier: Option<watch::Receiver<bool>> = None;

    let mut raw = Vec::new();
    loop {
        match read_line_bounded(&mut input, &mut raw, MAX_PAYLOAD_BYTES).await {
            Ok(BoundedLine::Eof) | Err(_) => break,
            Ok(BoundedLine::TooLong) => {
                write_value(
                    &output,
                    &error_envelope(
                        Value::Null,
                        -32603,
                        &format!(
                            "T3 Code MCP bridge input message exceeded {} bytes.",
                            MAX_PAYLOAD_BYTES
                        ),
                    ),
                )
                .await;
                continue;
            }
            Ok(BoundedLine::Line) => {}
        }
        let mut end = raw.len();
        if end > 0 && raw[end - 1] == b'\n' {
            end -= 1;
            if end > 0 && raw[end - 1] == b'\r' {
                end -= 1;
            }
        }
        let line = String::from_utf8_lossy(&raw[..end]).into_owned();
        if line.trim().is_empty() {
            continue;
        }
        let parsed: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(_) => {
                write_value(
                    &output,
                    &json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Parse error"}}),
                )
                .await;
                continue;
            }
        };
        // JS `typeof value === "object"` is true for objects AND arrays.
        if !(parsed.is_object() || parsed.is_array()) {
            continue;
        }
        let object = parsed.as_object();
        let has_id = object.map(|o| o.contains_key("id")).unwrap_or(false);
        let id = object.and_then(|o| o.get("id")).cloned();
        let is_response = has_id
            && object
                .map(|o| o.contains_key("result") || o.contains_key("error"))
                .unwrap_or(false);
        let method = object
            .and_then(|o| o.get("method"))
            .and_then(|m| m.as_str());
        let is_session_handshake =
            method == Some("initialize") || method == Some("notifications/initialized");

        while requests.try_join_next().is_some() {}
        while replies.try_join_next().is_some() {}
        // Give replies their own slots so a full request queue cannot prevent
        // a server-initiated request from receiving its reply.
        let (tasks, bytes) = if is_response {
            (&mut replies, &reply_bytes)
        } else {
            (&mut requests, &request_bytes)
        };
        let slots_available = tasks.len() < MAX_IN_FLIGHT_REQUESTS;
        let permit = slots_available
            .then(|| bytes.clone().try_acquire_many_owned(line.len() as u32).ok())
            .flatten();
        let Some(permit) = permit else {
            if let Some(id) = id.filter(|_| !is_response) {
                write_value(
                    &output,
                    &error_envelope(
                        id,
                        -32603,
                        "T3 Code MCP bridge has too many in-flight requests.",
                    ),
                )
                .await;
            }
            continue;
        };

        if is_response {
            let client = client.clone();
            let endpoint = endpoint.clone();
            let authorization = authorization.clone();
            let state = state.clone();
            let output = output.clone();
            tasks.spawn(async move {
                let _permit = permit;
                run_forward(client, endpoint, authorization, state, output, line, None).await;
            });
        } else if is_session_handshake {
            let prior = pending_barrier.take();
            let (tx, rx) = watch::channel(false);
            pending_barrier = Some(rx.clone());
            tasks.spawn({
                let client = client.clone();
                let endpoint = endpoint.clone();
                let authorization = authorization.clone();
                let state = state.clone();
                let output = output.clone();
                async move {
                    let _permit = permit;
                    if let Some(prior) = prior {
                        wait_done(prior).await;
                    }
                    run_forward(client, endpoint, authorization, state, output, line, id).await;
                    let _ = tx.send(true);
                }
            });
        } else {
            let barrier = pending_barrier.clone();
            tasks.spawn({
                let client = client.clone();
                let endpoint = endpoint.clone();
                let authorization = authorization.clone();
                let state = state.clone();
                let output = output.clone();
                async move {
                    let _permit = permit;
                    if let Some(barrier) = barrier {
                        wait_done(barrier).await;
                    }
                    run_forward(client, endpoint, authorization, state, output, line, id).await;
                }
            });
        }
    }
    while requests.join_next().await.is_some() {}
    while replies.join_next().await.is_some() {}
}

// ---------------------------------------------------------------------------
// acp-mcp-call (one-shot tool session)
// ---------------------------------------------------------------------------

/// Error carrying the same message text as `AcpMcpBridgeError`.
#[derive(Debug)]
pub struct BridgeError {
    pub message: String,
}

impl BridgeError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for BridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for BridgeError {}

fn server_request_reply(payload: &Value) -> Option<Value> {
    let object = payload.as_object()?;
    if !object.contains_key("id") {
        return None;
    }
    let method = object.get("method")?.as_str()?;
    let id = object.get("id").cloned().unwrap_or(Value::Null);
    Some(if method == "ping" {
        json!({"jsonrpc":"2.0","id":id,"result":{}})
    } else {
        json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Method not found"}})
    })
}

async fn session_post(
    client: &HttpClient,
    endpoint: &str,
    state: &SharedState,
    authorization: &str,
    message: Value,
) -> Result<http::Response<Incoming>, BridgeError> {
    let response = http_post(
        client,
        endpoint,
        state,
        authorization,
        Bytes::from(js_stringify(&message)),
    )
    .await
    .map_err(BridgeError::new)?;
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let _ = collect_body_limited(response.into_body()).await;
        return Err(BridgeError::new(format!(
            "T3 Code MCP endpoint responded with HTTP {status}."
        )));
    }
    Ok(response)
}

async fn session_send(
    client: &HttpClient,
    endpoint: &str,
    state: &SharedState,
    authorization: &str,
    message: Value,
) -> Result<Vec<Value>, BridgeError> {
    let response = session_post(client, endpoint, state, authorization, message).await?;
    let mut readers = vec![PayloadReader::new(response)];
    let mut payloads = Vec::new();
    while !readers.is_empty() {
        let next = {
            let reader = readers.last_mut().expect("readers is not empty");
            reader.next_payload().await.map_err(BridgeError::new)?
        };
        let Some(payload) = next else {
            readers.pop();
            continue;
        };
        update_protocol_version(state, &payload);
        if let Some(reply) = server_request_reply(&payload) {
            if readers.len() >= MAX_SERVER_REQUEST_DEPTH {
                return Err(BridgeError::new(format!(
                    "T3 Code MCP server request nesting exceeded {MAX_SERVER_REQUEST_DEPTH} levels."
                )));
            }
            let nested = session_post(client, endpoint, state, authorization, reply).await?;
            readers.push(PayloadReader::new(nested));
        } else if readers.len() == 1 {
            // Responses to server-request replies are fire-and-forget in the
            // upstream implementation; their non-request payloads are dropped.
            payloads.push(payload);
        }
    }
    Ok(payloads)
}

async fn session_request(
    client: &HttpClient,
    endpoint: &str,
    state: &SharedState,
    authorization: &str,
    next_id: &mut u64,
    method: &str,
    params: Value,
) -> Result<Value, BridgeError> {
    let id = format!("t3-cli-{}", *next_id);
    *next_id += 1;
    let responses = session_send(
        client,
        endpoint,
        state,
        authorization,
        json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
    )
    .await?;
    let expected = Value::String(id);
    let envelope = responses
        .iter()
        .find(|entry| entry.get("id") == Some(&expected));
    match envelope {
        Some(env) => {
            let object = env.as_object().expect("envelope with id is an object");
            if let Some(error) = object.get("error") {
                Err(BridgeError::new(format!(
                    "T3 Code MCP {method} failed: {}",
                    js_stringify(error)
                )))
            } else {
                Ok(env.clone())
            }
        }
        None => Err(BridgeError::new(format!("T3 Code MCP {method} failed."))),
    }
}

/// Run one MCP tool session: initialize -> notifications/initialized ->
/// tools/call, then DELETE the session whenever a session id was issued.
/// Returns the `tools/call` envelope's `result` member, or `None` when the
/// server omitted it (prints as `undefined` upstream).
pub async fn acp_mcp_call(
    endpoint: &str,
    authorization: &str,
    tool: &str,
    arguments: Value,
) -> Result<Option<Value>, BridgeError> {
    let client = http_client();
    let state = SharedState::default();
    let mut next_id = 0u64;

    let outcome = async {
        session_request(
            &client,
            endpoint,
            &state,
            authorization,
            &mut next_id,
            "initialize",
            json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "t3-code-acp-cli", "version": "0.0.0"},
            }),
        )
        .await?;
        session_send(
            &client,
            endpoint,
            &state,
            authorization,
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        )
        .await?;
        let envelope = session_request(
            &client,
            endpoint,
            &state,
            authorization,
            &mut next_id,
            "tools/call",
            json!({"name": tool, "arguments": arguments}),
        )
        .await?;
        Ok::<Option<Value>, BridgeError>(envelope.get("result").cloned())
    }
    .await;

    if shared_lock(&state.session_id).is_some() {
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            http_delete(&client, endpoint, &state, authorization),
        )
        .await;
    }
    outcome
}

/// Drop `content` text parts that merely duplicate `structuredContent`,
/// matching the filter in `runAcpMcpCliFastPath`.
pub fn dedup_structured_content(result: &mut Value) {
    let Some(object) = result.as_object_mut() else {
        return;
    };
    let has_encoded = object.contains_key("structuredContent")
        && object.get("content").map(|c| c.is_array()).unwrap_or(false);
    if !has_encoded {
        return;
    }
    let encoded = js_stringify(&object["structuredContent"]);
    if let Some(Value::Array(content)) = object.get_mut("content") {
        content.retain(|part| {
            !(part.get("type").and_then(|t| t.as_str()) == Some("text")
                && part
                    .get("text")
                    .and_then(|t| t.as_str())
                    .map(|t| t == encoded)
                    .unwrap_or(false))
        });
    }
}

#[cfg(test)]
mod tests;
