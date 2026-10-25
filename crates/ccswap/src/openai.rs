//! Anthropic Messages on loopback, using the live Codex account's inference login.

mod credentials;
mod stream;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use ccswap_claude::openai::InferenceLogins;
use ccswap_codex::{CodexPaths, CodexStore};
use ccswap_core::{
    endpoints::Endpoints,
    error::{Error, Result},
    fsutil, http_client,
    paths::Paths,
};
use serde_json::{Value, json};

pub const PROXY_ADDRESS: &str = "127.0.0.1:18347";
pub const PROXY_BASE_URL: &str = "http://127.0.0.1:18347";
const MAX_HEADERS: usize = 64 * 1024;
const MAX_BODY: usize = 32 * 1024 * 1024;

pub fn model_map() -> Value {
    serde_json::from_str(include_str!("../../../openai-models.json"))
        .expect("embedded OpenAI model map")
}

/// Persist once so already-running Claude sessions reconnect after a menu bar restart.
pub fn proxy_token(paths: &Paths) -> Result<String> {
    let logins = InferenceLogins::new(paths);
    let _lock = logins.lock()?;
    let file = logins.directory().join("proxy-token");
    match std::fs::read_to_string(&file) {
        Ok(token) if token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit()) => {
            return Ok(token);
        }
        Ok(_) => return Err(Error::config("Invalid saved OpenAI proxy token")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(Error::config(format!(
                "Could not read OpenAI proxy token: {e}"
            )));
        }
    }
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|e| Error::config(format!("Could not generate OpenAI proxy token: {e}")))?;
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    fsutil::ensure_private_dir(&logins.directory()).map_err(|e| Error::config(e.to_string()))?;
    fsutil::atomic_write_private(&logins.directory(), &file, token.as_bytes())
        .map_err(|e| Error::config(e.to_string()))?;
    Ok(token)
}

/// The public HTTP seam. The caller binds the listener; menu bar owns its lifetime.
pub fn serve(
    listener: TcpListener,
    paths: Paths,
    codex_paths: CodexPaths,
    endpoints: Endpoints,
) -> Result<()> {
    if !listener
        .local_addr()
        .map_err(|e| Error::config(e.to_string()))?
        .ip()
        .is_loopback()
    {
        return Err(Error::config("OpenAI proxy must listen on loopback"));
    }
    let token = Arc::new(proxy_token(&paths)?);
    let config = Arc::new((paths, codex_paths, endpoints));
    for stream in listener.incoming() {
        let stream = stream.map_err(|e| Error::config(format!("OpenAI proxy listener: {e}")))?;
        let token = token.clone();
        let config = config.clone();
        std::thread::spawn(move || {
            let mut stream = stream;
            let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
            let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
            if let Err(error) = handle(&mut stream, &token, &config.0, &config.1, &config.2) {
                let _ = send_json(&mut stream, error.status, &error.json(), error.retry_after);
            }
        });
    }
    Ok(())
}

#[derive(Debug)]
struct Failure {
    status: u16,
    kind: &'static str,
    message: String,
    retry_after: Option<u64>,
}

impl Failure {
    fn new(status: u16, kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            kind,
            message: message.into(),
            retry_after: None,
        }
    }
    fn invalid(message: impl Into<String>) -> Self {
        Self::new(400, "invalid_request_error", message)
    }
    fn upstream(message: impl Into<String>) -> Self {
        Self::new(502, "api_error", message)
    }
    fn json(&self) -> Value {
        json!({"type":"error","error":{"type":self.kind,"message":self.message}})
    }
    fn from_openai(status: u16, error: &Value) -> Self {
        let code = error["code"].as_str();
        let (http_status, kind) = match (status, code) {
            (429, _) | (_, Some("subscription_sharing_usage_limit_exceeded")) => {
                (429, "rate_limit_error")
            }
            (503, _) | (_, Some("subscription_sharing_usage_unavailable")) => {
                (503, "overloaded_error")
            }
            (401, _) => (401, "authentication_error"),
            _ => (502, "api_error"),
        };
        let message = match code {
            Some(code) => format!("OpenAI Responses failed ({code})"),
            None => format!("OpenAI Responses returned HTTP {status}"),
        };
        Self::new(http_status, kind, message)
    }
}

fn handle(
    stream: &mut TcpStream,
    token: &str,
    paths: &Paths,
    codex_paths: &CodexPaths,
    endpoints: &Endpoints,
) -> std::result::Result<(), Failure> {
    let (method, target, body) = read_request(stream, token)?;
    if method == "GET" && target == "/health" {
        return send_json(stream, 200, &json!({"status":"ok"}), None)
            .map_err(|_| Failure::upstream("Client disconnected"));
    }
    if method != "POST" || target.split('?').next() != Some("/v1/messages") {
        return Err(Failure::new(
            404,
            "not_found_error",
            "Only /v1/messages is supported",
        ));
    }
    let request: Value =
        serde_json::from_slice(&body).map_err(|_| Failure::invalid("Invalid JSON request"))?;
    let response_body = convert_request(&request)?;
    let model = response_body["model"].as_str().expect("resolved model");
    let store = CodexStore::new(codex_paths.clone());
    let account = store
        .list()
        .map_err(|_| Failure::upstream("Could not read Codex accounts"))?
        .accounts
        .into_iter()
        .find(|a| a.active)
        .ok_or_else(|| {
            Failure::invalid("No active managed Codex account; use ccswap codex switch <number>")
        })?;
    let logins = InferenceLogins::new(paths);
    let access = credentials::access(&logins, &account, endpoints, None)?;
    let upstream_request = response_body.to_string();
    let send = |access: &str| {
        http_client::agent(Duration::from_secs(600))
            .post(&endpoints.openai_responses_url)
            .header("Authorization", format!("Bearer {access}"))
            .header("Content-Type", "application/json")
            .send(upstream_request.as_bytes())
            .map_err(|_| Failure::upstream("Could not contact OpenAI Responses"))
    };
    let mut response = send(&access)?;
    if response.status().as_u16() == 401 {
        drop(response);
        let access = credentials::access(&logins, &account, endpoints, Some(&access))?;
        response = send(&access)?;
    }
    let status = response.status().as_u16();
    if status != 200 {
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|h| h.to_str().ok())
            .and_then(http_client::parse_retry_after)
            .filter(|n| n.is_finite())
            .map(|n| n.ceil() as u64);
        let body = response
            .body_mut()
            .read_to_string()
            .ok()
            .and_then(|body| serde_json::from_str::<Value>(&body).ok())
            .unwrap_or(Value::Null);
        let mut failure = Failure::from_openai(status, &body["error"]);
        failure.retry_after = retry_after;
        if status == 401 {
            failure.message.push_str(&format!(
                "; run ccswap claude openai login {}",
                account.number
            ));
        }
        return Err(failure);
    }
    let streaming = request["stream"] == true;
    if streaming {
        write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n").map_err(|_| Failure::upstream("Client disconnected"))?;
        if let Err(error) = stream::translate(
            BufReader::new(response.body_mut().as_reader()),
            stream,
            model,
            true,
        ) {
            let _ = stream::event(stream, &error.json());
        }
    } else {
        let message = stream::translate(
            BufReader::new(response.body_mut().as_reader()),
            stream,
            model,
            false,
        )?;
        send_json(stream, 200, &message, None)
            .map_err(|_| Failure::upstream("Client disconnected"))?;
    }
    Ok(())
}

fn read_request(
    stream: &TcpStream,
    token: &str,
) -> std::result::Result<(String, String, Vec<u8>), Failure> {
    let mut reader = BufReader::new(stream);
    let mut total = 0;
    let mut line = |reader: &mut BufReader<&TcpStream>| -> std::result::Result<String, Failure> {
        let mut bytes = Vec::new();
        let n = reader
            .take((MAX_HEADERS - total + 1) as u64)
            .read_until(b'\n', &mut bytes)
            .map_err(|_| Failure::invalid("Could not read HTTP headers"))?;
        total += n;
        if n == 0 || total > MAX_HEADERS || !bytes.ends_with(b"\r\n") {
            return Err(Failure::invalid("Invalid or oversized HTTP headers"));
        }
        String::from_utf8(bytes).map_err(|_| Failure::invalid("Invalid HTTP headers"))
    };
    let first = line(&mut reader)?;
    let parts: Vec<_> = first.split_whitespace().collect();
    if parts.len() != 3 || parts[2] != "HTTP/1.1" {
        return Err(Failure::invalid("Expected HTTP/1.1 request"));
    }
    let mut auth = None;
    let mut length = None;
    loop {
        let header = line(&mut reader)?;
        if header == "\r\n" {
            break;
        }
        let (name, value) = header
            .trim_end()
            .split_once(':')
            .ok_or_else(|| Failure::invalid("Invalid HTTP header"))?;
        let value = value.trim();
        if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(Failure::invalid("Transfer-Encoding is not supported"));
        }
        if name.eq_ignore_ascii_case("content-length") {
            if length.is_some() {
                return Err(Failure::invalid("Duplicate Content-Length"));
            }
            length = Some(
                value
                    .parse::<usize>()
                    .ok()
                    .filter(|n| *n <= MAX_BODY)
                    .ok_or_else(|| Failure::invalid("Invalid or oversized Content-Length"))?,
            );
        }
        if name.eq_ignore_ascii_case("authorization") || name.eq_ignore_ascii_case("x-api-key") {
            if auth.is_some() {
                return Err(Failure::invalid("Duplicate authentication headers"));
            }
            auth = Some(value.strip_prefix("Bearer ").unwrap_or(value).to_string());
        }
    }
    if auth.as_deref() != Some(token) {
        return Err(Failure::new(
            401,
            "authentication_error",
            "Invalid OpenAI proxy token",
        ));
    }
    let len = length.unwrap_or(0);
    if parts[0] == "POST" && length.is_none() {
        return Err(Failure::invalid("Content-Length is required"));
    }
    let mut body = vec![0; len];
    reader
        .read_exact(&mut body)
        .map_err(|_| Failure::invalid("Incomplete HTTP request body"))?;
    Ok((parts[0].into(), parts[1].into(), body))
}

fn convert_request(request: &Value) -> std::result::Result<Value, Failure> {
    let name = request["model"]
        .as_str()
        .ok_or_else(|| Failure::invalid("A model is required"))?;
    let map = model_map();
    let family = if name.starts_with("claude-") {
        name.split('-')
            .find(|part| map.get(*part).is_some())
            .unwrap_or(name)
    } else {
        name
    };
    let model = map
        .get(family)
        .and_then(Value::as_str)
        .or_else(|| {
            map.as_object()?
                .values()
                .find_map(|value| (value.as_str() == Some(name)).then_some(name))
        })
        .ok_or_else(|| Failure::invalid(format!("Model {name} is not in openai-models.json")))?;
    let messages = request["messages"]
        .as_array()
        .ok_or_else(|| Failure::invalid("messages must be an array"))?;
    let mut input = Vec::new();
    for message in messages {
        let role = message["role"]
            .as_str()
            .filter(|r| matches!(*r, "user" | "assistant"))
            .ok_or_else(|| Failure::invalid("Unsupported message role"))?;
        let content = &message["content"];
        if content.is_string() {
            input.push(json!({"role":role,"content":text(content)?}));
            continue;
        }
        let blocks = content
            .as_array()
            .ok_or_else(|| Failure::invalid("Message content must be text or blocks"))?;
        let mut pending = Vec::new();
        for block in blocks {
            match block["type"].as_str() {
                Some("text") => pending.push(required_string(block, "text")?),
                Some("thinking" | "redacted_thinking") => {}
                Some("tool_use" | "tool_result") => {
                    if !pending.is_empty() {
                        input.push(json!({"role":role,"content":pending.join("\n")}));
                        pending.clear();
                    }
                    if block["type"] == "tool_use" && role == "assistant" {
                        if !block["input"].is_object() {
                            return Err(Failure::invalid("Tool input must be an object"));
                        }
                        input.push(json!({"type":"function_call","call_id":required_string(block,"id")?,"name":required_string(block,"name")?,"arguments":block["input"].to_string()}));
                    } else if block["type"] == "tool_result" && role == "user" {
                        let output = match block.get("content") {
                            Some(content) => text(content)?,
                            None => String::new(),
                        };
                        input.push(json!({"type":"function_call_output","call_id":required_string(block,"tool_use_id")?,"output":output}));
                    } else {
                        return Err(Failure::invalid("Tool block has an invalid message role"));
                    }
                }
                _ => return Err(Failure::invalid("Unsupported message content block")),
            }
        }
        if !pending.is_empty() {
            input.push(json!({"role":role,"content":pending.join("\n")}));
        }
    }
    let mut out = json!({"model":model,"input":input,"store":false,"stream":true});
    if let Some(system) = request.get("system") {
        out["instructions"] = text(system)?.into();
    }
    if let Some(tools) = request.get("tools") {
        let tools = tools
            .as_array()
            .ok_or_else(|| Failure::invalid("tools must be an array"))?;
        let mut functions = Vec::new();
        for tool in tools {
            if !tool["input_schema"].is_object() {
                return Err(Failure::invalid("Tool input_schema must be an object"));
            }
            let mut function = json!({"type":"function","name":required_string(tool,"name")?,"parameters":tool["input_schema"],"strict":false});
            if tool.get("description").is_some() {
                function["description"] = required_string(tool, "description")?.into();
            }
            functions.push(function);
        }
        out["tools"] = functions.into();
    }
    Ok(out)
}

fn required_string<'a>(value: &'a Value, key: &str) -> std::result::Result<&'a str, Failure> {
    value[key]
        .as_str()
        .ok_or_else(|| Failure::invalid(format!("Missing or invalid {key}")))
}

fn text(content: &Value) -> std::result::Result<String, Failure> {
    if let Some(text) = content.as_str() {
        return Ok(text.into());
    }
    let blocks = content
        .as_array()
        .ok_or_else(|| Failure::invalid("Message content must be text or blocks"))?;
    let mut text = Vec::new();
    for block in blocks {
        match block["type"].as_str() {
            Some("text") => text.push(
                block["text"]
                    .as_str()
                    .ok_or_else(|| Failure::invalid("Invalid text block"))?,
            ),
            Some("thinking" | "redacted_thinking") => {}
            _ => return Err(Failure::invalid("Unsupported message content block")),
        }
    }
    Ok(text.join("\n"))
}

fn send_json(
    stream: &mut TcpStream,
    status: u16,
    body: &Value,
    retry_after: Option<u64>,
) -> std::io::Result<()> {
    let body = body.to_string();
    write!(
        stream,
        "HTTP/1.1 {status} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    )?;
    if let Some(seconds) = retry_after {
        write!(stream, "Retry-After: {seconds}\r\n")?;
    }
    write!(stream, "\r\n{body}")
}
