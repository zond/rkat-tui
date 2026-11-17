//! A fake OpenAI-dialect endpoint shaped like a LiteLLM proxy, for the
//! tests that run the engine's real HTTP clients: `GET /v1/models`,
//! LiteLLM's `GET /model/info`, and streamed answers on `POST
//! /v1/responses` and `POST /v1/chat/completions` from a script. Every
//! request is recorded (method, path, headers, JSON body).

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// One model the endpoint lists, with its `/model/info` facts.
#[derive(Debug, Clone)]
pub struct FakeModel {
    pub id: &'static str,
    pub max_input_tokens: u64,
    pub max_output_tokens: u64,
    /// USD per token.
    pub input_cost: f64,
    pub output_cost: f64,
    pub supports_reasoning: bool,
}

/// One scripted answer.
#[derive(Clone)]
pub enum Reply {
    /// A text answer.
    Text(&'static str),
    /// Reasoning, then a text answer.
    Reasoning(&'static str, &'static str),
    /// One tool call.
    Tool { name: &'static str, args: Value },
    /// Text, then one tool call, as a proxy streams it: on the Responses
    /// API an assistant `message` item with its id (added, its text's
    /// delta, done) before the `function_call` item; on Chat Completions a
    /// `content` delta before the `tool_calls` one. `text` may be empty or
    /// only whitespace, as some models send before a call.
    TextAndTool {
        text: &'static str,
        name: &'static str,
        args: Value,
    },
    /// The answer this function makes of the request's body (where two
    /// actors call the model and their order is not known). Every model
    /// call takes it, and it is never used up.
    Dynamic(Arc<dyn Fn(&Value) -> Reply + Send + Sync>),
}

impl std::fmt::Debug for Reply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Text(text) => f.debug_tuple("Text").field(text).finish(),
            Self::Reasoning(thought, text) => f
                .debug_tuple("Reasoning")
                .field(thought)
                .field(text)
                .finish(),
            Self::Tool { name, args } => f
                .debug_struct("Tool")
                .field("name", name)
                .field("args", args)
                .finish(),
            Self::TextAndTool { text, name, args } => f
                .debug_struct("TextAndTool")
                .field("text", text)
                .field("name", name)
                .field("args", args)
                .finish(),
            Self::Dynamic(_) => f.write_str("Dynamic"),
        }
    }
}

/// A recorded request.
#[derive(Debug, Clone)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    /// Header names lower-cased.
    pub headers: Vec<(String, String)>,
    pub body: Value,
}

impl Recorded {
    /// The value of header `name` (lower-case).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    /// Whether this is a model call.
    pub fn is_turn(&self) -> bool {
        self.method == "POST"
            && (self.path.ends_with("/responses") || self.path.ends_with("/chat/completions"))
    }
}

struct Shared {
    models: Vec<FakeModel>,
    replies: Mutex<VecDeque<Reply>>,
    requests: Mutex<Vec<Recorded>>,
    /// Model calls answered so far: each answer's tool call gets its own id.
    calls: std::sync::atomic::AtomicU64,
}

/// The running endpoint (stopped when dropped).
pub struct FakeEndpoint {
    pub base_url: String,
    shared: Arc<Shared>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeEndpoint {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeEndpoint {
    /// Serve `models` and answer model calls with `replies` in order (then
    /// `"ok"`).
    pub async fn start(models: Vec<FakeModel>, replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let shared = Arc::new(Shared {
            models,
            replies: Mutex::new(replies.into()),
            requests: Mutex::new(Vec::new()),
            calls: std::sync::atomic::AtomicU64::new(0),
        });
        let serving = Arc::clone(&shared);
        let task = tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                let shared = Arc::clone(&serving);
                tokio::spawn(async move {
                    let _ = serve(socket, &shared).await;
                });
            }
        });
        Self {
            base_url: format!("http://{addr}"),
            shared,
            task,
        }
    }

    /// Every request so far.
    pub fn requests(&self) -> Vec<Recorded> {
        self.shared.requests.lock().unwrap().clone()
    }

    /// The model calls so far.
    pub fn turns(&self) -> Vec<Recorded> {
        self.requests()
            .into_iter()
            .filter(Recorded::is_turn)
            .collect()
    }
}

/// Serve the requests of one connection (keep-alive).
async fn serve(mut socket: TcpStream, shared: &Shared) -> std::io::Result<()> {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let head_end = loop {
            if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break at;
            }
            let mut chunk = [0u8; 8192];
            let n = socket.read(&mut chunk).await?;
            if n == 0 {
                return Ok(());
            }
            buf.extend_from_slice(&chunk[..n]);
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
        let mut lines = head.split("\r\n");
        let request_line = lines.next().unwrap_or_default();
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_string();
        let path = parts
            .next()
            .unwrap_or_default()
            .split('?')
            .next()
            .unwrap_or_default()
            .to_string();
        let headers: Vec<(String, String)> = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_string()))
            .collect();
        let length: usize = headers
            .iter()
            .find(|(n, _)| n == "content-length")
            .and_then(|(_, v)| v.parse().ok())
            .unwrap_or(0);
        let body_start = head_end + 4;
        while buf.len() < body_start + length {
            let mut chunk = [0u8; 8192];
            let n = socket.read(&mut chunk).await?;
            if n == 0 {
                return Ok(());
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        let body: Value =
            serde_json::from_slice(&buf[body_start..body_start + length]).unwrap_or(Value::Null);
        buf.drain(..body_start + length);
        let recorded = Recorded {
            method: method.clone(),
            path: path.clone(),
            headers,
            body,
        };
        let (content_type, payload) = respond(&recorded, shared);
        shared.requests.lock().unwrap().push(recorded);
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\n\r\n",
            payload.len()
        );
        socket.write_all(response.as_bytes()).await?;
        socket.write_all(payload.as_bytes()).await?;
        socket.flush().await?;
    }
}

fn respond(request: &Recorded, shared: &Shared) -> (&'static str, String) {
    let json_body = |value: Value| ("application/json", value.to_string());
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/v1/models" | "/models") => json_body(json!({
            "object": "list",
            "data": shared.models.iter().map(|m| json!({
                "id": m.id, "object": "model", "created": 0, "owned_by": "litellm"
            })).collect::<Vec<_>>(),
        })),
        ("GET", "/model/info" | "/v1/model/info") => json_body(json!({
            "data": shared.models.iter().map(|m| json!({
                "model_name": m.id,
                "model_info": {
                    "max_input_tokens": m.max_input_tokens,
                    "max_output_tokens": m.max_output_tokens,
                    "input_cost_per_token": m.input_cost,
                    "output_cost_per_token": m.output_cost,
                    "supports_reasoning": m.supports_reasoning,
                    "supports_vision": false,
                    "supports_function_calling": true,
                },
            })).collect::<Vec<_>>(),
        })),
        ("POST", path) if path.ends_with("/responses") => {
            let reply = next_reply(shared, &request.body);
            (
                "text/event-stream",
                responses_sse(&reply, call_number(shared)),
            )
        }
        ("POST", path) if path.ends_with("/chat/completions") => {
            let reply = next_reply(shared, &request.body);
            ("text/event-stream", chat_sse(&reply, call_number(shared)))
        }
        _ => json_body(json!({})),
    }
}

fn next_reply(shared: &Shared, body: &Value) -> Reply {
    let mut replies = shared.replies.lock().unwrap();
    match replies.front() {
        Some(Reply::Dynamic(make)) => make(body),
        _ => replies.pop_front().unwrap_or(Reply::Text("ok")),
    }
}

/// This model call's number, which its ids carry (`resp_<n>`, `call_<n>`).
fn call_number(shared: &Shared) -> u64 {
    shared
        .calls
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        + 1
}

fn sse(events: &[Value]) -> String {
    let mut out = String::new();
    for event in events {
        out.push_str("data: ");
        out.push_str(&event.to_string());
        out.push_str("\n\n");
    }
    out
}

const USAGE_IN: u64 = 1_000;
const USAGE_OUT: u64 = 100;

fn responses_sse(reply: &Reply, n: u64) -> String {
    let response = format!("resp_{n}");
    let message = format!("msg_{n}");
    let mut events = vec![json!({"type": "response.created", "response": {"id": response}})];
    let text = |events: &mut Vec<Value>, text: &str| {
        events.push(json!({"type": "response.output_text.delta", "delta": text}));
    };
    match reply {
        Reply::Text(answer) => text(&mut events, answer),
        // Resolved by `next_reply`; one that makes another is "ok".
        Reply::Dynamic(_) => text(&mut events, "ok"),
        Reply::Reasoning(thought, answer) => {
            events.push(json!({"type": "response.reasoning_summary_text.delta", "delta": thought}));
            text(&mut events, answer);
        }
        Reply::Tool { name, args } => function_call_item(&mut events, 0, n, name, args),
        Reply::TextAndTool { text, name, args } => {
            events.push(json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type": "message", "id": message, "role": "assistant", "status": "in_progress", "content": []},
            }));
            events.push(json!({
                "type": "response.output_text.delta",
                "item_id": message,
                "output_index": 0,
                "content_index": 0,
                "delta": text,
            }));
            events.push(json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {
                    "type": "message", "id": message, "role": "assistant", "status": "completed",
                    "content": [{"type": "output_text", "text": text, "annotations": []}],
                },
            }));
            function_call_item(&mut events, 1, n, name, args);
        }
    }
    events.push(json!({
        "type": "response.completed",
        "response": {
            "id": response,
            "status": "completed",
            "usage": {"input_tokens": USAGE_IN, "output_tokens": USAGE_OUT},
        },
    }));
    sse(&events)
}

/// A finished Responses `function_call` output item at `index`.
fn function_call_item(events: &mut Vec<Value>, index: u64, n: u64, name: &str, args: &Value) {
    events.push(json!({
        "type": "response.output_item.done",
        "output_index": index,
        "item": {
            "type": "function_call",
            "id": format!("fc_{n}"),
            "call_id": format!("call_{n}"),
            "name": name,
            "arguments": args.to_string(),
            "status": "completed",
        },
    }));
}

fn chat_sse(reply: &Reply, n: u64) -> String {
    let call = format!("call_{n}");
    let chunk = |delta: Value, finish: Option<&str>| {
        json!({
            "id": "chat_1",
            "object": "chat.completion.chunk",
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
        })
    };
    let mut events = Vec::new();
    let finish = match reply {
        Reply::Text(answer) => {
            events.push(chunk(json!({"role": "assistant", "content": answer}), None));
            "stop"
        }
        // Resolved by `next_reply`; one that makes another is "ok".
        Reply::Dynamic(_) => {
            events.push(chunk(json!({"role": "assistant", "content": "ok"}), None));
            "stop"
        }
        Reply::Reasoning(thought, answer) => {
            events.push(chunk(
                json!({"role": "assistant", "reasoning_content": thought}),
                None,
            ));
            events.push(chunk(json!({"content": answer}), None));
            "stop"
        }
        Reply::TextAndTool { text, name, args } => {
            events.push(chunk(json!({"role": "assistant", "content": text}), None));
            events.push(chunk(
                json!({"tool_calls": [{
                    "index": 0, "id": call, "type": "function",
                    "function": {"name": name, "arguments": args.to_string()},
                }]}),
                None,
            ));
            "tool_calls"
        }
        Reply::Tool { name, args } => {
            events.push(chunk(
                json!({"role": "assistant", "tool_calls": [{
                    "index": 0, "id": call, "type": "function",
                    "function": {"name": name, "arguments": args.to_string()},
                }]}),
                None,
            ));
            "tool_calls"
        }
    };
    events.push(chunk(json!({}), Some(finish)));
    events.push(json!({
        "id": "chat_1",
        "object": "chat.completion.chunk",
        "choices": [],
        "usage": {"prompt_tokens": USAGE_IN, "completion_tokens": USAGE_OUT, "total_tokens": USAGE_IN + USAGE_OUT},
    }));
    let mut out = sse(&events);
    out.push_str("data: [DONE]\n\n");
    out
}
