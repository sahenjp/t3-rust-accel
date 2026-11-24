use super::*;
use serde_json::json;
use std::io::Cursor;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tokio::time::timeout;

// ---------------------------------------------------------------------------
// Test HTTP server
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct RecordedRequest {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl RecordedRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

enum Part {
    Data(String),
    Wait(std::sync::Arc<Notify>),
    Sleep(u64),
    SetFlag(std::sync::Arc<AtomicBool>),
    /// Rendezvous point: stalls this stream until N peers also reach it.
    Sync(std::sync::Arc<tokio::sync::Barrier>),
}

enum Plan {
    Respond {
        status: u16,
        headers: Vec<(String, String)>,
        body: String,
    },
    /// Writes headers, then `parts`, then closes the socket (EOF-delimited).
    Stream {
        status: u16,
        headers: Vec<(String, String)>,
        parts: Vec<Part>,
    },
}

type Handler = Arc<dyn Fn(&RecordedRequest) -> Plan + Send + Sync>;

struct TestServer {
    endpoint: String,
    requests: Arc<StdMutex<Vec<RecordedRequest>>>,
}

async fn start_server(handler: Handler) -> TestServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let recorded = requests.clone();
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(handle_conn(
                socket,
                handler.clone(),
                recorded.clone(),
            ));
        }
    });
    TestServer { endpoint, requests }
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

async fn handle_conn(
    mut socket: tokio::net::TcpStream,
    handler: Handler,
    recorded: Arc<StdMutex<Vec<RecordedRequest>>>,
) {
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 16384];
    loop {
        let head_len = loop {
            if let Some(p) = find_header_end(&buf) {
                break p;
            }
            match socket.read(&mut tmp).await {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_len]).into_owned();
        let mut lines = head.split("\r\n");
        let request_line = lines.next().unwrap_or("");
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or("").to_string();
        let path = parts.next().unwrap_or("").to_string();
        let mut headers = Vec::new();
        let mut content_length = 0usize;
        for line in lines {
            if line.is_empty() {
                continue;
            }
            if let Some((k, v)) = line.split_once(':') {
                let (k, v) = (k.trim().to_string(), v.trim().to_string());
                if k.eq_ignore_ascii_case("content-length") {
                    content_length = v.parse().unwrap_or(0);
                }
                headers.push((k, v));
            }
        }
        while buf.len() < head_len + content_length {
            match socket.read(&mut tmp).await {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
            }
        }
        let body =
            String::from_utf8_lossy(&buf[head_len..head_len + content_length]).into_owned();
        buf.drain(..head_len + content_length);
        let request = RecordedRequest {
            method,
            path,
            headers,
            body,
        };
        recorded.lock().unwrap().push(request.clone());
        match handler(&request) {
            Plan::Respond {
                status,
                headers,
                body,
            } => {
                let mut out = format!("HTTP/1.1 {status} Reason\r\n");
                let has_len = headers
                    .iter()
                    .any(|(k, _)| k.eq_ignore_ascii_case("content-length"));
                for (k, v) in &headers {
                    out.push_str(&format!("{k}: {v}\r\n"));
                }
                if !has_len {
                    out.push_str(&format!("content-length: {}\r\n", body.len()));
                }
                out.push_str("\r\n");
                out.push_str(&body);
                if socket.write_all(out.as_bytes()).await.is_err() {
                    return;
                }
            }
            Plan::Stream {
                status,
                headers,
                parts,
            } => {
                let mut out = format!("HTTP/1.1 {status} Reason\r\n");
                for (k, v) in &headers {
                    out.push_str(&format!("{k}: {v}\r\n"));
                }
                out.push_str("connection: close\r\n\r\n");
                if socket.write_all(out.as_bytes()).await.is_err() {
                    return;
                }
                for part in parts {
                    match part {
                        Part::Data(data) => {
                            if socket.write_all(data.as_bytes()).await.is_err() {
                                return;
                            }
                        }
                        Part::Wait(notify) => {
                            notify.notified().await;
                        }
                        Part::Sleep(ms) => {
                            tokio::time::sleep(Duration::from_millis(ms)).await;
                        }
                        Part::SetFlag(flag) => {
                            flag.store(true, Ordering::SeqCst);
                        }
                        Part::Sync(barrier) => {
                            barrier.wait().await;
                        }
                    }
                }
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn stdout_of(out: &Mutex<Vec<u8>>) -> String {
    String::from_utf8(out.try_lock().expect("output still locked").clone()).unwrap()
}

fn out_lines(out: &Mutex<Vec<u8>>) -> Vec<String> {
    stdout_of(out)
        .lines()
        .map(|l| l.to_string())
        .collect()
}

fn bridge_input(lines: &[&str]) -> Cursor<Vec<u8>> {
    Cursor::new(lines.join("\n").into_bytes())
}

async fn run_bridge_with(input: Cursor<Vec<u8>>, endpoint: &str) -> Arc<Mutex<Vec<u8>>> {
    let out = Arc::new(Mutex::new(Vec::new()));
    timeout(
        Duration::from_secs(10),
        run_bridge(
            endpoint.to_string(),
            "Bearer test-token".to_string(),
            input,
            out.clone(),
        ),
    )
    .await
    .expect("run_bridge timed out");
    out
}

// ---------------------------------------------------------------------------
// js_stringify
// ---------------------------------------------------------------------------

#[test]
fn js_stringify_matches_json_stringify() {
    assert_eq!(js_stringify(&json!(1.0)), "1");
    assert_eq!(js_stringify(&json!(1.5)), "1.5");
    assert_eq!(js_stringify(&json!(0.1)), "0.1");
    assert_eq!(js_stringify(&json!(1e20)), "100000000000000000000");
    assert_eq!(js_stringify(&json!(1e21)), "1e+21");
    assert_eq!(js_stringify(&json!(0.000001)), "0.000001");
    assert_eq!(js_stringify(&json!(1e-7)), "1e-7");
    assert_eq!(js_stringify(&json!(-0.0)), "0");
    assert_eq!(js_stringify(&json!(-1.5e-8)), "-1.5e-8");
    // U+2028/U+2029 stay raw, matching JSON.stringify.
    let s = js_stringify(&json!("a\u{2028}b\u{2029}c"));
    assert!(s.contains('\u{2028}') && s.contains('\u{2029}'));
    assert_eq!(js_stringify(&json!("a\u{b}b")), "\"a\\u000bb\"");
    // Insertion order is preserved.
    let mut map = serde_json::Map::new();
    map.insert("z".to_string(), json!(1));
    map.insert("a".to_string(), json!(2));
    assert_eq!(js_stringify(&Value::Object(map)), "{\"z\":1,\"a\":2}");
}

// ---------------------------------------------------------------------------
// acp-mcp-bridge
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn bridge_forwards_and_writes_json_response() {
    let server = start_server(Arc::new(|req| {
        assert_eq!(req.method, "POST");
        assert_eq!(req.header("content-type"), Some("application/json"));
        assert_eq!(
            req.header("accept"),
            Some("application/json, text/event-stream")
        );
        assert_eq!(req.header("authorization"), Some("Bearer test-token"));
        Plan::Respond {
            status: 200,
            headers: vec![("content-type".into(), "application/json".into())],
            body: r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#.into(),
        }
    }))
    .await;

    let out = run_bridge_with(
        bridge_input(&[r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#]),
        &server.endpoint,
    )
    .await;

    assert_eq!(
        out_lines(&out),
        vec![r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#]
    );
    // The original line is forwarded verbatim.
    assert_eq!(
        server.requests.lock().unwrap()[0].body,
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#
    );
}

#[tokio::test(flavor = "current_thread")]
async fn bridge_fans_out_json_array_response() {
    let server = start_server(Arc::new(|_| Plan::Respond {
        status: 200,
        headers: vec![("content-type".into(), "application/json".into())],
        body: r#"[{"jsonrpc":"2.0","id":1,"result":1},{"jsonrpc":"2.0","method":"notifications/x"}]"#
            .into(),
    }))
    .await;

    let out = run_bridge_with(
        bridge_input(&[r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#]),
        &server.endpoint,
    )
    .await;

    assert_eq!(
        out_lines(&out),
        vec![
            r#"{"jsonrpc":"2.0","id":1,"result":1}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/x"}"#
        ]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn bridge_sse_stream_parsing() {
    let server = start_server(Arc::new(|_| Plan::Stream {
        status: 200,
        headers: vec![("content-type".into(), "text/event-stream".into())],
        parts: vec![
            // \n\n separator
            Part::Data("data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"a\":1}}\n\n".into()),
            // multi-line data joined with \n, \r\n\r\n separator
            Part::Data("data: {\"jsonrpc\":\"2.0\",\ndata: \"id\":2,\"result\":{}}\r\n\r\n".into()),
            // \r\r separator
            Part::Data("data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\r\r".into()),
            // ignored non-data fields
            Part::Data("event: message\nid: 5\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"n2\"}\n\n".into()),
        ],
    }))
    .await;

    let out = run_bridge_with(
        bridge_input(&[r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#]),
        &server.endpoint,
    )
    .await;

    assert_eq!(
        out_lines(&out),
        vec![
            r#"{"jsonrpc":"2.0","id":1,"result":{"a":1}}"#,
            r#"{"jsonrpc":"2.0","id":2,"result":{}}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/progress"}"#,
            r#"{"jsonrpc":"2.0","method":"n2"}"#,
        ]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn bridge_parse_error_and_ignored_lines() {
    let server = start_server(Arc::new(|_| Plan::Respond {
        status: 200,
        headers: vec![("content-type".into(), "application/json".into())],
        body: r#"{"jsonrpc":"2.0","id":1,"result":{}}"#.into(),
    }))
    .await;

    let out = run_bridge_with(
        bridge_input(&[
            "",              // blank
            "   ",           // whitespace
            "not json",      // parse error
            "123",           // non-object: ignored
            "\"str\"",       // non-object: ignored
            "null",          // ignored
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
        ]),
        &server.endpoint,
    )
    .await;

    let lines = out_lines(&out);
    assert_eq!(
        lines[0],
        r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Parse error"}}"#
    );
    assert_eq!(lines[1], r#"{"jsonrpc":"2.0","id":1,"result":{}}"#);
    assert_eq!(lines.len(), 2);
    // Only the valid object line reached the server.
    assert_eq!(server.requests.lock().unwrap().len(), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn bridge_http_error_with_and_without_id() {
    let server = start_server(Arc::new(|_| Plan::Respond {
        status: 500,
        headers: vec![],
        body: "oops".into(),
    }))
    .await;

    let out = run_bridge_with(
        bridge_input(&[
            r#"{"jsonrpc":"2.0","id":5,"method":"tools/call"}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/x"}"#,
        ]),
        &server.endpoint,
    )
    .await;

    assert_eq!(
        out_lines(&out),
        vec![r#"{"jsonrpc":"2.0","id":5,"error":{"code":-32603,"message":"T3 Code MCP endpoint responded with HTTP 500."}}"#]
    );
    assert_eq!(server.requests.lock().unwrap().len(), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn bridge_202_and_204_produce_no_output() {
    let server = start_server(Arc::new(|req| Plan::Respond {
        status: if req.body.contains("a") { 202 } else { 204 },
        headers: vec![],
        body: String::new(),
    }))
    .await;

    let out = run_bridge_with(
        bridge_input(&[
            r#"{"jsonrpc":"2.0","id":1,"method":"a"}"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"b"}"#,
        ]),
        &server.endpoint,
    )
    .await;

    assert!(out_lines(&out).is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn bridge_handshake_barrier_and_concurrency() {
    // The initialize SSE stream stays open for 150ms. A normal request
    // forwarded without waiting for the barrier would arrive while the flag
    // is still false. The two normal requests rendezvous on a Barrier(2), so
    // serialized forwarding deadlocks and the test times out.
    let init_done = Arc::new(AtomicBool::new(false));
    let saw_early = Arc::new(AtomicBool::new(false));
    let rendezvous = Arc::new(tokio::sync::Barrier::new(2));

    let server = {
        let init_done = init_done.clone();
        let saw_early = saw_early.clone();
        let rendezvous = rendezvous.clone();
        start_server(Arc::new(move |req| {
            if req.body.contains("\"initialize\"") {
                Plan::Stream {
                    status: 200,
                    headers: vec![
                        ("content-type".into(), "text/event-stream".into()),
                        ("mcp-session-id".into(), "sess-1".into()),
                    ],
                    parts: vec![
                        Part::Data(
                            "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"2025-06-18\",\"serverInfo\":{}}}\n\n".into(),
                        ),
                        Part::Sleep(150),
                        Part::SetFlag(init_done.clone()),
                    ],
                }
            } else {
                if !init_done.load(Ordering::SeqCst) {
                    saw_early.store(true, Ordering::SeqCst);
                }
                Plan::Stream {
                    status: 200,
                    headers: vec![("content-type".into(), "text/event-stream".into())],
                    parts: vec![
                        Part::Sync(rendezvous.clone()),
                        Part::Data(
                            "data: {\"jsonrpc\":\"2.0\",\"id\":99,\"result\":{\"ok\":true}}\n\n".into(),
                        ),
                    ],
                }
            }
        }))
        .await
    };

    let out = Arc::new(Mutex::new(Vec::new()));
    let input = bridge_input(&[
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call"}"#,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call"}"#,
    ]);
    timeout(
        Duration::from_secs(10),
        run_bridge(
            server.endpoint.clone(),
            "Bearer test-token".to_string(),
            input,
            out.clone(),
        ),
    )
    .await
    .expect("run_bridge timed out");

    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(requests[0].body.contains("initialize"));
    // No normal request arrived while the initialize stream was still open.
    assert!(!saw_early.load(Ordering::SeqCst));
    // Requests forwarded after the handshake carry the learned session id and
    // protocol version.
    assert_eq!(requests[1].header("mcp-session-id"), Some("sess-1"));
    assert_eq!(requests[2].header("mcp-session-id"), Some("sess-1"));
    assert_eq!(
        requests[1].header("mcp-protocol-version"),
        Some("2025-06-18")
    );
    drop(requests);
    assert_eq!(out_lines(&out).len(), 3);
}

#[tokio::test(flavor = "current_thread")]
async fn bridge_response_bypasses_barrier() {
    let release_init = Arc::new(Notify::new());
    let arrived_while_init_pending = Arc::new(AtomicBool::new(false));

    let release_clone = release_init.clone();
    let arrived_clone = arrived_while_init_pending.clone();
    let server = start_server(Arc::new(move |req| {
        let release_init = release_clone.clone();
        let arrived = arrived_clone.clone();
            if req.body.contains("\"initialize\"") {
                Plan::Stream {
                    status: 200,
                    headers: vec![("content-type".into(), "text/event-stream".into())],
                    parts: vec![
                        Part::Wait(release_init.clone()),
                        Part::Data(
                            "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"2025-06-18\"}}\n\n".into(),
                        ),
                    ],
                }
            } else {
                // Arrived while initialize still holds the barrier: only a
                // barrier-bypassing JSON-RPC response can get through.
                arrived.store(true, Ordering::SeqCst);
                Plan::Respond {
                    status: 200,
                    headers: vec![("content-type".into(), "application/json".into())],
                    body: r#"{"jsonrpc":"2.0","id":"srv-1","result":{}}"#.into(),
                }
            }
    }))
    .await;

    let out = Arc::new(Mutex::new(Vec::new()));
    let input = bridge_input(&[
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
        r#"{"jsonrpc":"2.0","id":"srv-1","result":{}}"#,
    ]);
    let bridge = tokio::spawn(run_bridge(
        server.endpoint.clone(),
        "Bearer test-token".to_string(),
        input,
        out.clone(),
    ));

    // Give the bridge a moment to forward both lines.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        arrived_while_init_pending.load(Ordering::SeqCst),
        "client response did not bypass the session barrier"
    );
    release_init.notify_waiters();
    timeout(Duration::from_secs(10), bridge).await.unwrap().unwrap();

    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].body.contains("srv-1"));
}

// ---------------------------------------------------------------------------
// acp-mcp-call
// ---------------------------------------------------------------------------

fn call_result_body() -> String {
    r#"{"jsonrpc":"2.0","id":"t3-cli-1","result":{"content":[{"type":"text","text":"{\"ok\":true}"},{"type":"text","text":"keep me"}],"structuredContent":{"ok":true}}}"#
        .to_string()
}

fn mcp_session_handler() -> (Handler, Arc<Notify>) {
    let tools_call_reached = Arc::new(Notify::new());
    let reached = tools_call_reached.clone();
    let handler: Handler = Arc::new(move |req| {
        if req.method == "DELETE" {
            return Plan::Respond {
                status: 200,
                headers: vec![],
                body: String::new(),
            };
        }
        if req.body.contains("\"initialize\"") {
            Plan::Respond {
                status: 200,
                headers: vec![
                    ("content-type".into(), "application/json".into()),
                    ("mcp-session-id".into(), "sess-42".into()),
                ],
                body: r#"{"jsonrpc":"2.0","id":"t3-cli-0","result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"t3","version":"1"}}}"#.into(),
            }
        } else if req.body.contains("notifications/initialized") {
            Plan::Respond {
                status: 202,
                headers: vec![],
                body: String::new(),
            }
        } else if req.body.contains("\"tools/call\"") {
            reached.notify_one();
            Plan::Respond {
                status: 200,
                headers: vec![("content-type".into(), "application/json".into())],
                body: call_result_body(),
            }
        } else {
            Plan::Respond {
                status: 500,
                headers: vec![],
                body: "unexpected".into(),
            }
        }
    });
    (handler, tools_call_reached)
}

#[tokio::test(flavor = "current_thread")]
async fn call_happy_path_dedup_and_delete() {
    let (handler, _) = mcp_session_handler();
    let server = start_server(handler).await;

    let result = timeout(
        Duration::from_secs(10),
        acp_mcp_call(
            &server.endpoint,
            "Bearer test-token",
            "some_tool",
            json!({"arg": 1}),
        ),
    )
    .await
    .expect("call timed out")
    .expect("call failed");

    let mut result = result.expect("missing result");
    dedup_structured_content(&mut result);
    let content = result["content"].as_array().unwrap();
    // The text part duplicating structuredContent was filtered out.
    assert_eq!(content.len(), 1);
    assert_eq!(content[0]["text"], "keep me");

    let requests = server.requests.lock().unwrap();
    let methods: Vec<(&str, &str)> = requests
        .iter()
        .map(|r| (r.method.as_str(), r.body.as_str()))
        .collect();
    assert_eq!(requests.len(), 4);
    assert!(methods[0].1.contains("initialize"));
    assert!(methods[1].1.contains("notifications/initialized"));
    assert!(methods[2].1.contains("tools/call"));
    assert_eq!(methods[3].0, "DELETE");
    // Session id + protocol version propagated after initialize.
    assert_eq!(requests[1].header("mcp-session-id"), Some("sess-42"));
    assert_eq!(requests[2].header("mcp-session-id"), Some("sess-42"));
    assert_eq!(
        requests[2].header("mcp-protocol-version"),
        Some("2025-06-18")
    );
    assert_eq!(requests[3].header("mcp-session-id"), Some("sess-42"));
    assert!(requests[3].header("content-type").is_none());
    // initialize is sent before a session id exists.
    assert!(requests[0].header("mcp-session-id").is_none());
    // Request id sequence uses t3-cli-{n}.
    assert!(requests[2].body.contains(r#""id":"t3-cli-1""#));
}

#[tokio::test(flavor = "current_thread")]
async fn call_replies_to_server_requests_midstream() {
    let ping_gate = Arc::new(Notify::new());
    let bogus_gate = Arc::new(Notify::new());
    let handler: Handler = {
        let ping_gate = ping_gate.clone();
        let bogus_gate = bogus_gate.clone();
        Arc::new(move |req| {
            if req.method == "DELETE" {
                return Plan::Respond {
                    status: 200,
                    headers: vec![],
                    body: String::new(),
                };
            }
            if req.body.contains("\"initialize\"") {
                // SSE stream: server asks ping, waits for the client's reply
                // POST before completing the initialize result.
                Plan::Stream {
                    status: 200,
                    headers: vec![
                        ("content-type".into(), "text/event-stream".into()),
                        ("mcp-session-id".into(), "sess-9".into()),
                    ],
                    parts: vec![
                        Part::Data(
                            "data: {\"jsonrpc\":\"2.0\",\"id\":\"srv-p1\",\"method\":\"ping\"}\n\n"
                                .into(),
                        ),
                        Part::Wait(ping_gate.clone()),
                        Part::Data(
                            "data: {\"jsonrpc\":\"2.0\",\"id\":\"t3-cli-0\",\"result\":{\"protocolVersion\":\"2025-06-18\"}}\n\n".into(),
                        ),
                    ],
                }
            } else if req.body.contains("notifications/initialized") {
                Plan::Respond {
                    status: 202,
                    headers: vec![],
                    body: String::new(),
                }
            } else if req.body.contains("\"tools/call\"") {
                Plan::Stream {
                    status: 200,
                    headers: vec![("content-type".into(), "text/event-stream".into())],
                    parts: vec![
                        Part::Data(
                            "data: {\"jsonrpc\":\"2.0\",\"id\":\"srv-x\",\"method\":\"bogus/thing\"}\n\n".into(),
                        ),
                        Part::Wait(bogus_gate.clone()),
                        Part::Data(
                            "data: {\"jsonrpc\":\"2.0\",\"id\":\"t3-cli-1\",\"result\":{\"done\":true}}\n\n".into(),
                        ),
                    ],
                }
            } else if req.body.contains("srv-p1") {
                assert_eq!(
                    req.body,
                    r#"{"jsonrpc":"2.0","id":"srv-p1","result":{}}"#
                );
                ping_gate.notify_one();
                Plan::Respond {
                    status: 202,
                    headers: vec![],
                    body: String::new(),
                }
            } else if req.body.contains("srv-x") {
                assert_eq!(
                    req.body,
                    r#"{"jsonrpc":"2.0","id":"srv-x","error":{"code":-32601,"message":"Method not found"}}"#
                );
                bogus_gate.notify_one();
                Plan::Respond {
                    status: 202,
                    headers: vec![],
                    body: String::new(),
                }
            } else {
                Plan::Respond {
                    status: 500,
                    headers: vec![],
                    body: "unexpected".into(),
                }
            }
        })
    };
    let server = start_server(handler).await;

    let result = timeout(
        Duration::from_secs(10),
        acp_mcp_call(&server.endpoint, "Bearer test-token", "tool", json!({})),
    )
    .await
    .expect("call timed out")
    .expect("call failed");
    assert_eq!(result, Some(json!({"done": true})));

    let requests = server.requests.lock().unwrap();
    // initialize, ping-reply, notification, tools/call, bogus-reply, DELETE.
    assert_eq!(requests.len(), 6);
    assert_eq!(requests[5].method, "DELETE");
}

#[tokio::test(flavor = "current_thread")]
async fn call_http_error_still_deletes_session() {
    // Server answers initialize with HTTP 500 but still issues a session id —
    // the session must be DELETED even though the call failed.
    let handler: Handler = Arc::new(|req| {
        if req.method == "DELETE" {
            Plan::Respond {
                status: 200,
                headers: vec![],
                body: String::new(),
            }
        } else {
            Plan::Respond {
                status: 500,
                headers: vec![("mcp-session-id".into(), "sess-doomed".into())],
                body: "nope".into(),
            }
        }
    });
    let server = start_server(handler).await;

    let err = timeout(
        Duration::from_secs(10),
        acp_mcp_call(&server.endpoint, "Bearer test-token", "tool", json!({})),
    )
    .await
    .expect("timed out")
    .expect_err("expected failure");
    assert_eq!(
        err.message,
        "T3 Code MCP endpoint responded with HTTP 500."
    );

    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].method, "DELETE");
    assert_eq!(requests[1].header("mcp-session-id"), Some("sess-doomed"));
}

#[tokio::test(flavor = "current_thread")]
async fn call_rpc_error_message() {
    let handler: Handler = Arc::new(|req| {
        if req.method == "DELETE" {
            Plan::Respond {
                status: 200,
                headers: vec![],
                body: String::new(),
            }
        } else if req.body.contains("\"initialize\"") {
            Plan::Respond {
                status: 200,
                headers: vec![
                    ("content-type".into(), "application/json".into()),
                    ("mcp-session-id".into(), "s".into()),
                ],
                body: r#"{"jsonrpc":"2.0","id":"t3-cli-0","result":{"protocolVersion":"2025-06-18"}}"#.into(),
            }
        } else if req.body.contains("notifications/initialized") {
            Plan::Respond {
                status: 202,
                headers: vec![],
                body: String::new(),
            }
        } else {
            Plan::Respond {
                status: 200,
                headers: vec![("content-type".into(), "application/json".into())],
                body: r#"{"jsonrpc":"2.0","id":"t3-cli-1","error":{"code":-32000,"message":"tool blew up"}}"#.into(),
            }
        }
    });
    let server = start_server(handler).await;

    let err = timeout(
        Duration::from_secs(10),
        acp_mcp_call(&server.endpoint, "Bearer test-token", "tool", json!({})),
    )
    .await
    .expect("timed out")
    .expect_err("expected rpc failure");
    assert_eq!(
        err.message,
        r#"T3 Code MCP tools/call failed: {"code":-32000,"message":"tool blew up"}"#
    );
}

// ---------------------------------------------------------------------------
// dedup_structured_content
// ---------------------------------------------------------------------------

#[test]
fn dedup_only_filters_duplicate_text_parts() {
    // No structuredContent: untouched.
    let mut v = json!({"content":[{"type":"text","text":"x"}]});
    dedup_structured_content(&mut v);
    assert_eq!(v["content"].as_array().unwrap().len(), 1);

    // structuredContent present but content not an array: untouched.
    let mut v = json!({"structuredContent":{"a":1},"content":null});
    dedup_structured_content(&mut v);
    assert!(v["content"].is_null());

    // null structuredContent encodes as "null".
    let mut v = json!({
        "structuredContent": null,
        "content": [
            {"type":"text","text":"null"},
            {"type":"text","text":"other"},
            {"type":"image","data":"x","text":"null"}
        ]
    });
    dedup_structured_content(&mut v);
    let content = v["content"].as_array().unwrap();
    assert_eq!(content.len(), 2);
    assert_eq!(content[0]["text"], "other");
    assert_eq!(content[1]["type"], "image");
}
