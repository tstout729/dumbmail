//! One model turn through the Anthropic Messages API (`POST /v1/messages`, model [`MODEL`]),
//! streamed, over raw HTTPS like the other clients (no official Rust SDK exists).
//!
//! Requests go only to `https://api.anthropic.com` (a loopback stand-in set in code replaces it in
//! tests, see `translations::ai::api_base`). Thinking stays at the model's default with
//! `output_config.effort` set; no `temperature`, no prefill, `tool_choice` `auto` with `strict`
//! tools. The server-side fallback beta reroutes a declined request. The caller checks
//! [`Turn::stop`] before using [`Turn::content`]. Errors are reduced to a short class: nothing
//! from the request, the key or the response body is logged or stored.

use std::time::Duration;

use serde_json::{json, Map, Value};

pub const MODEL: &str = "claude-opus-5-5";
const API_VERSION: &str = "2026-10-21";
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
/// Most tokens one model turn may write (thinking included).
pub const MAX_TOKENS: u32 = 8192;
/// The largest request body.
pub const MAX_REQUEST_BYTES: usize = 900 * 1024;
const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(90);

/// How a turn ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// The model wants tools run.
    ToolUse,
    EndTurn,
    MaxTokens,
    /// The model (or the safety system, after any fallback) declined.
    Refusal,
    /// Anything else (pause_turn, a stop sequence, a new reason).
    Other(String),
}

/// A finished model turn: its content blocks as the API sent them (to echo back unchanged) and
/// the tokens it used.
#[derive(Debug, Clone)]
pub struct Turn {
    pub stop: Stop,
    pub content: Vec<Value>,
    pub tokens: i64,
}

/// What went wrong, short and safe to store, and the tokens the attempt used as far as the API
/// reported them (a stream that broke off still bills what it streamed).
#[derive(Debug, Clone)]
pub struct Failure {
    pub msg: &'static str,
    pub tokens: i64,
}

impl Failure {
    pub fn new(msg: &'static str) -> Self {
        Failure { msg, tokens: 0 }
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.msg)
    }
}

pub struct Request<'a> {
    /// A test's loopback stand-in; None: the Anthropic API.
    pub base_url: Option<&'a str>,
    pub api_key: &'a str,
    pub system: &'a str,
    pub tools: &'a [Value],
    pub messages: &'a [Value],
}

fn client() -> Result<reqwest::Client, Failure> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| Failure::new("couldn't start the model client"))
}

pub fn body(r: &Request<'_>) -> Value {
    json!({
        "model": MODEL,
        "max_tokens": MAX_TOKENS,
        "stream": true,
        "system": [{"type": "text", "text": r.system}],
        "tools": r.tools,
        "tool_choice": {"type": "auto"},
        "messages": r.messages,
        "output_config": {"effort": "low"},
        "fallbacks": "default",
        "cache_control": {"type": "ephemeral"},
    })
}

/// Run one model turn. `on_text` gets each piece of visible text as it streams (the caller throws
/// it away again when the turn doesn't end well).
pub async fn turn(r: &Request<'_>, mut on_text: impl FnMut(&str)) -> Result<Turn, Failure> {
    let body = serde_json::to_vec(&body(r)).map_err(|_| Failure::new("couldn't write the request"))?;
    if body.len() > MAX_REQUEST_BYTES {
        return Err(Failure::new("the conversation is too long"));
    }
    let base = crate::features::translations::ai::api_base(r.base_url);
    let url = format!("{}/v1/messages", base.trim_end_matches('/'));
    let work = async {
        let res = client()?
            .post(url)
            .header("x-api-key", r.api_key)
            .header("anthropic-version", API_VERSION)
            .header("anthropic-beta", FALLBACK_BETA)
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .map_err(|_| Failure::new("couldn't reach the model"))?;
        let status = res.status().as_u16();
        if status != 200 {
            return Err(Failure::new(match status {
                401 | 403 => "the model service refused the key",
                429 => "the model service is busy",
                529 => "the model service is overloaded",
                s if s >= 500 => "the model service had a problem",
                _ => "the model service refused the request",
            }));
        }
        read_stream(res, &mut on_text).await
    };
    tokio::time::timeout(TIMEOUT, work).await.map_err(|_| Failure::new("the model didn't answer in time"))?
}

/// Folds the server-sent events of one streamed message into its content blocks. Only a complete
/// stream counts: `message_stop` seen, every block that started also stopped, every tool input
/// complete JSON. Usage is the API's final counters, not a sum of snapshots.
#[derive(Default)]
pub struct Assembler {
    blocks: Vec<Value>,
    /// Partial JSON of a tool_use block's input, by block index.
    partial: std::collections::HashMap<usize, String>,
    open: std::collections::HashSet<usize>,
    bad_input: bool,
    stop: Option<String>,
    stopped: bool,
    /// Input tokens from `message_start` (input, cache writes and reads).
    input: i64,
    /// Output tokens: the latest cumulative count.
    output: i64,
    /// Every attempt's usage when the API lists them (server-side fallback).
    iterations: Option<i64>,
    error: bool,
}

impl Assembler {
    /// One event's `data` JSON. Returns visible text to show, if any.
    pub fn event(&mut self, data: &Value) -> Option<String> {
        match data["type"].as_str()? {
            "message_start" => {
                let u = &data["message"]["usage"];
                self.input = input_tokens(u);
                self.output = u["output_tokens"].as_i64().unwrap_or(0);
                None
            }
            "content_block_start" => {
                let i = data["index"].as_u64()? as usize;
                while self.blocks.len() <= i {
                    self.blocks.push(Value::Null);
                }
                self.blocks[i] = data["content_block"].clone();
                self.open.insert(i);
                data["content_block"]["text"].as_str().filter(|t| !t.is_empty() && data["content_block"]["type"] == "text").map(str::to_string)
            }
            "content_block_delta" => {
                let i = data["index"].as_u64()? as usize;
                let d = &data["delta"];
                let block = self.blocks.get_mut(i)?;
                match d["type"].as_str()? {
                    "text_delta" => {
                        let t = d["text"].as_str()?;
                        append(block, "text", t);
                        Some(t.to_string())
                    }
                    "thinking_delta" => {
                        append(block, "thinking", d["thinking"].as_str()?);
                        None
                    }
                    "signature_delta" => {
                        block["signature"] = d["signature"].clone();
                        None
                    }
                    "input_json_delta" => {
                        self.partial.entry(i).or_default().push_str(d["partial_json"].as_str()?);
                        None
                    }
                    _ => None,
                }
            }
            "content_block_stop" => {
                let i = data["index"].as_u64()? as usize;
                self.open.remove(&i);
                if let Some(raw) = self.partial.remove(&i) {
                    let input = if raw.trim().is_empty() { Ok(json!({})) } else { serde_json::from_str::<Value>(&raw) };
                    match input {
                        Ok(v) if v.is_object() => {
                            if let Some(b) = self.blocks.get_mut(i) {
                                b["input"] = v;
                            }
                        }
                        _ => self.bad_input = true,
                    }
                }
                None
            }
            "message_delta" => {
                if let Some(s) = data["delta"]["stop_reason"].as_str() {
                    self.stop = Some(s.to_string());
                }
                let u = &data["usage"];
                if let Some(o) = u["output_tokens"].as_i64() {
                    self.output = o;
                }
                if u["input_tokens"].is_i64() {
                    self.input = self.input.max(input_tokens(u));
                }
                if let Some(it) = u["iterations"].as_array() {
                    self.iterations = Some(it.iter().map(|x| input_tokens(x) + x["output_tokens"].as_i64().unwrap_or(0)).sum());
                }
                None
            }
            "message_stop" => {
                self.stopped = true;
                None
            }
            "error" => {
                self.error = true;
                None
            }
            _ => None,
        }
    }

    /// The tokens used so far, by the API's own counters.
    pub fn tokens(&self) -> i64 {
        self.iterations.unwrap_or(self.input + self.output)
    }

    pub fn finish(self) -> Result<Turn, Failure> {
        let tokens = self.tokens();
        let fail = |msg| Err(Failure { msg, tokens });
        if self.error {
            return fail("the model service stopped part way");
        }
        if !self.stopped || !self.open.is_empty() || !self.partial.is_empty() {
            return fail("the model's answer broke off");
        }
        if self.bad_input {
            return fail("the model sent a broken tool call");
        }
        let stop = match self.stop.as_deref() {
            Some("tool_use") => Stop::ToolUse,
            Some("end_turn") => Stop::EndTurn,
            Some("max_tokens") => Stop::MaxTokens,
            Some("refusal") => Stop::Refusal,
            Some(o) => Stop::Other(o.to_string()),
            None => return fail("the model's answer broke off"),
        };
        Ok(Turn { stop, content: self.blocks.into_iter().filter(|b| !b.is_null()).collect(), tokens })
    }
}

fn append(block: &mut Value, key: &str, more: &str) {
    let mut s = block[key].as_str().unwrap_or("").to_string();
    s.push_str(more);
    block[key] = Value::String(s);
}

fn input_tokens(u: &Value) -> i64 {
    ["input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens"].iter().map(|k| u[*k].as_i64().unwrap_or(0)).sum()
}

async fn read_stream(mut res: reqwest::Response, on_text: &mut impl FnMut(&str)) -> Result<Turn, Failure> {
    let mut asm = Assembler::default();
    let mut buf: Vec<u8> = Vec::new();
    let mut total = 0usize;
    loop {
        let chunk = match res.chunk().await {
            Ok(Some(c)) => c,
            Ok(None) => break,
            Err(_) => return Err(Failure { msg: "the model's answer broke off", tokens: asm.tokens() }),
        };
        total += chunk.len();
        if total > MAX_RESPONSE_BYTES {
            return Err(Failure { msg: "the model's answer was too long", tokens: asm.tokens() });
        }
        buf.extend_from_slice(&chunk);
        while let Some(end) = find_event_end(&buf) {
            let raw: Vec<u8> = buf.drain(..end.1).collect();
            let text = String::from_utf8_lossy(&raw[..end.0]).into_owned();
            if let Some(data) = sse_data(&text) {
                if let Ok(v) = serde_json::from_str::<Value>(&data) {
                    if let Some(t) = asm.event(&v) {
                        on_text(&t);
                    }
                }
            }
        }
    }
    asm.finish()
}

/// Where the first complete event in `buf` ends: (end of its text, end including the blank line).
fn find_event_end(buf: &[u8]) -> Option<(usize, usize)> {
    let a = buf.windows(2).position(|w| w == b"\n\n").map(|i| (i, i + 2));
    let b = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| (i, i + 4));
    match (a, b) {
        (Some(x), Some(y)) => Some(if x.0 <= y.0 { x } else { y }),
        (x, y) => x.or(y),
    }
}

/// The `data:` lines of one event, joined.
fn sse_data(event: &str) -> Option<String> {
    let lines: Vec<&str> = event.lines().filter_map(|l| l.strip_prefix("data:")).map(|l| l.strip_prefix(' ').unwrap_or(l)).collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// The assistant content to send back on the next request. After a mid-answer fallback, blocks
/// the declined model wrote before the last `fallback` block are left out, except text and the
/// fallback blocks themselves, which stay where they were.
pub fn echo(content: &[Value]) -> Vec<Value> {
    let Some(boundary) = content.iter().rposition(|b| b["type"] == "fallback") else { return content.to_vec() };
    content
        .iter()
        .enumerate()
        .filter(|(i, b)| *i >= boundary || matches!(b["type"].as_str(), Some("text" | "fallback")))
        .map(|(_, b)| b.clone())
        .collect()
}

/// The tool calls to run: those after the last `fallback` block (earlier ones were the declined
/// model's and are dropped from the echo).
pub fn tool_calls(content: &[Value]) -> Vec<(String, String, Value)> {
    let start = content.iter().rposition(|b| b["type"] == "fallback").map(|i| i + 1).unwrap_or(0);
    content[start..]
        .iter()
        .filter(|b| b["type"] == "tool_use")
        .filter_map(|b| Some((b["id"].as_str()?.to_string(), b["name"].as_str()?.to_string(), b["input"].clone())))
        .collect()
}

/// The visible text of a turn's content.
pub fn text_of(content: &[Value]) -> String {
    content.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("")
}

/// A `strict` tool definition (all properties required, nothing else allowed).
pub fn tool_def(name: &str, description: &str, properties: Value) -> Value {
    let required: Vec<String> = properties.as_object().map(|o| o.keys().cloned().collect()).unwrap_or_default();
    json!({
        "name": name,
        "description": description,
        "strict": true,
        "input_schema": {"type": "object", "properties": properties, "required": required, "additionalProperties": false},
    })
}

/// A tool result block (content as JSON text).
pub fn tool_result(id: &str, content: &Value, is_error: bool) -> Value {
    let mut m = Map::new();
    m.insert("type".into(), json!("tool_result"));
    m.insert("tool_use_id".into(), json!(id));
    m.insert("content".into(), json!(content.to_string()));
    if is_error {
        m.insert("is_error".into(), json!(true));
    }
    Value::Object(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(events: &[Value]) -> (Turn, String) {
        let mut a = Assembler::default();
        let mut shown = String::new();
        for e in events {
            if let Some(t) = a.event(e) {
                shown.push_str(&t);
            }
        }
        (a.finish().unwrap(), shown)
    }

    #[test]
    fn assembles_text_thinking_and_tool_calls() {
        let (t, shown) = feed(&[
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 100, "output_tokens": 1}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "sig"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "Let me "}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "check."}}),
            json!({"type": "content_block_stop", "index": 1}),
            json!({"type": "content_block_start", "index": 2, "content_block": {"type": "tool_use", "id": "toolu_1", "name": "search_products", "input": {}}}),
            json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "{\"query\":"}}),
            json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": " \"jacket\"}"}}),
            json!({"type": "content_block_stop", "index": 2}),
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 20}}),
            json!({"type": "message_stop"}),
        ]);
        assert_eq!(shown, "Let me check.");
        assert_eq!(t.stop, Stop::ToolUse);
        assert_eq!(t.content[0]["signature"], "sig");
        assert_eq!(t.content[2]["input"]["query"], "jacket");
        assert_eq!(t.tokens, 120, "input from message_start, output from the final count");
        assert_eq!(tool_calls(&t.content), vec![("toolu_1".to_string(), "search_products".to_string(), json!({"query": "jacket"}))]);
    }

    #[test]
    fn a_stream_without_a_stop_reason_is_an_error() {
        let mut a = Assembler::default();
        a.event(&json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": "Hi"}}));
        assert!(a.finish().is_err());
    }

    #[test]
    fn only_a_complete_stream_counts() {
        let start = json!({"type": "message_start", "message": {"usage": {"input_tokens": 10, "output_tokens": 1}}});
        let text = |i: u64| json!({"type": "content_block_start", "index": i, "content_block": {"type": "text", "text": "Hi"}});
        let stop_block = |i: u64| json!({"type": "content_block_stop", "index": i});
        let delta = json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 7}});
        let done = json!({"type": "message_stop"});
        let run = |evs: Vec<Value>| {
            let mut a = Assembler::default();
            for e in &evs {
                a.event(e);
            }
            a.finish()
        };
        assert!(run(vec![start.clone(), text(0), stop_block(0), delta.clone(), done.clone()]).is_ok());
        let e = run(vec![start.clone(), text(0), stop_block(0), delta.clone()]).unwrap_err();
        assert_eq!(e.tokens, 17, "a broken stream still reports what it used");
        assert!(run(vec![start.clone(), text(0), delta.clone(), done.clone()]).is_err(), "a block that never stopped");
        let tool = vec![
            start.clone(),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "t", "name": "search_products", "input": {}}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"query\": \"jack"}}),
            stop_block(0),
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 7}}),
            done.clone(),
        ];
        assert!(run(tool).is_err(), "a tool input that isn't complete JSON");
        // Server-side fallback: every attempt's usage counts.
        let fb = vec![start, text(0), stop_block(0), json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 7, "iterations": [{"input_tokens": 10, "output_tokens": 3}, {"input_tokens": 12, "output_tokens": 7}]}}), done];
        assert_eq!(run(fb).unwrap().tokens, 32);
    }

    #[test]
    fn echo_drops_the_declined_models_blocks_before_a_fallback() {
        let content = vec![
            json!({"type": "thinking", "thinking": "", "signature": "a"}),
            json!({"type": "text", "text": "Part"}),
            json!({"type": "tool_use", "id": "t0", "name": "x", "input": {}}),
            json!({"type": "fallback", "from": {"model": "m1"}, "to": {"model": "m2"}}),
            json!({"type": "thinking", "thinking": "", "signature": "b"}),
            json!({"type": "tool_use", "id": "t1", "name": "y", "input": {}}),
        ];
        let e = echo(&content);
        let kinds: Vec<&str> = e.iter().map(|b| b["type"].as_str().unwrap()).collect();
        assert_eq!(kinds, vec!["text", "fallback", "thinking", "tool_use"]);
        assert_eq!(tool_calls(&content).len(), 1);
        assert_eq!(tool_calls(&content)[0].0, "t1");
    }

    #[test]
    fn sse_events_split_on_blank_lines() {
        let buf = b"event: ping\ndata: {\"type\":\"ping\"}\n\nevent: x\r\ndata: 1\r\n\r\n";
        let (end, next) = find_event_end(buf).unwrap();
        assert_eq!(sse_data(std::str::from_utf8(&buf[..end]).unwrap()).unwrap(), "{\"type\":\"ping\"}");
        let rest = &buf[next..];
        let (end2, _) = find_event_end(rest).unwrap();
        assert_eq!(sse_data(std::str::from_utf8(&rest[..end2]).unwrap()).unwrap(), "1");
    }

    #[test]
    fn requests_use_the_current_api_shape() {
        let b = body(&Request { base_url: None, api_key: "k", system: "s", tools: &[], messages: &[] });
        assert_eq!(b["model"], "claude-opus-5-5");
        assert_eq!(b["tool_choice"]["type"], "auto");
        assert_eq!(b["output_config"]["effort"], "low");
        assert!(b.get("temperature").is_none() && b.get("thinking").is_none());
        let t = tool_def("search_products", "d", json!({"query": {"type": "string"}}));
        assert_eq!(t["strict"], true);
        assert_eq!(t["input_schema"]["additionalProperties"], false);
        assert_eq!(t["input_schema"]["required"][0], "query");
    }
}
