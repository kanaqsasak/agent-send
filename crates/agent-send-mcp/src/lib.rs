//! Transport-thin MCP adapter for the daemon's authenticated local API.
//! The adapter owns no transfer or filesystem policy.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

pub const TOKEN_ENV: &str = "AGENT_SEND_AGENT_TOKEN";
pub const ADDR_ENV: &str = "AGENT_SEND_DAEMON_ADDR";
pub const DEFAULT_ADDR: &str = "127.0.0.1:41641";
const METHODS: [&str; 5] = [
    "peers.list",
    "folders.list",
    "transfers.send",
    "transfers.status",
    "transfers.cancel",
];

#[derive(Clone, PartialEq, Eq)]
pub struct AdapterConfig {
    pub daemon_addr: String,
    pub bearer_token: String,
}

#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    #[error("{TOKEN_ENV} is required")]
    MissingToken,
    #[error("{TOKEN_ENV} must not be empty")]
    EmptyToken,
    #[error("invalid JSON-RPC request: {0}")]
    Parse(String),
    #[error("unsupported MCP method: {0}")]
    UnsupportedMethod(String),
    #[error("daemon request failed: {0}")]
    Transport(#[from] io::Error),
    #[error("daemon returned HTTP status {status}: {code}")]
    Daemon { status: u16, code: String },
    #[error("daemon returned invalid JSON")]
    InvalidDaemonResponse,
}

impl std::fmt::Debug for AdapterConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AdapterConfig")
            .field("daemon_addr", &self.daemon_addr)
            .field("bearer_token", &"[REDACTED]")
            .finish()
    }
}

impl AdapterConfig {
    pub fn from_env() -> Result<Self, AdapterError> {
        let token = std::env::var(TOKEN_ENV).map_err(|_| AdapterError::MissingToken)?;
        if token.trim().is_empty() {
            return Err(AdapterError::EmptyToken);
        }
        Ok(Self {
            daemon_addr: std::env::var(ADDR_ENV).unwrap_or_else(|_| DEFAULT_ADDR.into()),
            bearer_token: token,
        })
    }
}

/// The only authorization material accepted by this process is configured at startup.
#[derive(Clone, PartialEq, Eq)]
pub struct TokenAuth(String);
impl std::fmt::Debug for TokenAuth {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("TokenAuth([REDACTED])")
    }
}

impl TokenAuth {
    pub fn new(token: impl Into<String>) -> Result<Self, AdapterError> {
        let token = token.into();
        if token.trim().is_empty() {
            return Err(AdapterError::EmptyToken);
        }
        Ok(Self(token))
    }
    pub fn verify(&self, supplied: &str) -> bool {
        constant_time_eq(self.0.as_bytes(), supplied.as_bytes())
    }
}

#[derive(Debug, Deserialize)]
struct RpcRequest {
    jsonrpc: String,
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}
#[derive(Debug, Serialize)]
struct RpcResponse {
    jsonrpc: &'static str,
    id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RpcError>,
}
#[derive(Debug, Serialize)]
struct RpcError {
    code: i32,
    message: String,
    data: Value,
}

pub trait DaemonClient {
    fn operation(&mut self, operation: &str, params: Value) -> Result<Value, AdapterError>;
}

pub struct TcpDaemonClient {
    addr: String,
    auth: TokenAuth,
}
impl TcpDaemonClient {
    pub fn new(config: AdapterConfig) -> Result<Self, AdapterError> {
        Ok(Self {
            addr: config.daemon_addr,
            auth: TokenAuth::new(config.bearer_token)?,
        })
    }
}
impl DaemonClient for TcpDaemonClient {
    fn operation(&mut self, operation: &str, params: Value) -> Result<Value, AdapterError> {
        let body = json!({"operation": operation, "params": params}).to_string();
        let mut stream = TcpStream::connect(
            self.addr
                .to_socket_addrs()?
                .next()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "daemon address"))?,
        )?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        write!(stream, "POST /v1/agent HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", self.auth.0, body.len(), body)?;
        stream.flush()?;
        let mut response = String::new();
        stream.read_to_string(&mut response)?;
        let (head, payload) = response
            .split_once("\r\n\r\n")
            .ok_or(AdapterError::InvalidDaemonResponse)?;
        let status = head
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|s| s.parse().ok())
            .ok_or(AdapterError::InvalidDaemonResponse)?;
        let value: Value =
            serde_json::from_str(payload).map_err(|_| AdapterError::InvalidDaemonResponse)?;
        if status / 100 != 2 {
            return Err(AdapterError::Daemon {
                status,
                code: value
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("daemon_error")
                    .into(),
            });
        }
        Ok(value.get("result").cloned().unwrap_or(value))
    }
}

#[derive(Debug, Serialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

pub fn tool_definitions() -> Vec<ToolDefinition> {
    let empty = json!({"type":"object","properties":{},"additionalProperties":false});
    vec![
        ("peers.list", "List trusted peers permitted by the token", empty.clone()),
        ("folders.list", "List named folders permitted by the token", empty),
        ("transfers.send", "Submit a transfer using named folder IDs and relative source paths", json!({"type":"object","required":["peer_id","source_folder_id","source_paths","destination_folder_id","idempotency_key"],"properties":{"peer_id":{"type":"string"},"source_folder_id":{"type":"string"},"source_paths":{"type":"array","items":{"type":"string"}},"destination_folder_id":{"type":"string"},"idempotency_key":{"type":"string"}},"additionalProperties":false})),
        ("transfers.status", "Get the status of a transfer", json!({"type":"object","required":["transfer_id"],"properties":{"transfer_id":{"type":"string"}},"additionalProperties":false})),
        ("transfers.cancel", "Cancel a transfer", json!({"type":"object","required":["transfer_id"],"properties":{"transfer_id":{"type":"string"}},"additionalProperties":false})),
    ].into_iter().map(|(name, description, input_schema)| ToolDefinition { name: name.into(), description: description.into(), input_schema }).collect()
}

pub fn parse_request(line: &str) -> Result<(Option<Value>, String, Value), AdapterError> {
    let request: RpcRequest =
        serde_json::from_str(line).map_err(|e| AdapterError::Parse(e.to_string()))?;
    if request.jsonrpc != "2.0" || request.method.trim().is_empty() {
        return Err(AdapterError::Parse(
            "JSON-RPC 2.0 method is required".into(),
        ));
    }
    Ok((request.id, request.method, request.params))
}

pub fn handle_line<C: DaemonClient>(line: &str, client: &mut C) -> Value {
    let parsed = parse_request(line);
    let (id, method, params) = match parsed {
        Ok(v) => v,
        Err(e) => return rpc_error(None, -32600, e.to_string(), "invalid_request"),
    };
    let result = match method.as_str() {
        "initialize" => Ok(
            json!({"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"agent-send-mcp","version":"0.1.0"}}),
        ),
        "tools/list" => Ok(json!({"tools": tool_definitions()})),
        "tools/call" => call_tool(params, client),
        _ => Err(AdapterError::UnsupportedMethod(method)),
    };
    match result {
        Ok(value) => serde_json::to_value(RpcResponse {
            jsonrpc: "2.0",
            id,
            result: Some(value),
            error: None,
        })
        .unwrap(),
        Err(e) => rpc_error(id, -32602, e.to_string(), error_code(&e)),
    }
}

fn call_tool<C: DaemonClient>(params: Value, client: &mut C) -> Result<Value, AdapterError> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| AdapterError::Parse("tools/call requires name".into()))?;
    if !METHODS.contains(&name) {
        return Err(AdapterError::UnsupportedMethod(name.into()));
    }
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let value = client.operation(name, args)?;
    Ok(
        json!({"content":[{"type":"text","text":serde_json::to_string(&value).unwrap()}],"structuredContent":value,"isError":false}),
    )
}
fn error_code(error: &AdapterError) -> &'static str {
    match error {
        AdapterError::UnsupportedMethod(_) => "unsupported_method",
        AdapterError::Daemon { .. } => "daemon_error",
        _ => "invalid_request",
    }
}
fn rpc_error(id: Option<Value>, code: i32, message: String, kind: &str) -> Value {
    serde_json::to_value(RpcResponse {
        jsonrpc: "2.0",
        id,
        result: None,
        error: Some(RpcError {
            code,
            message,
            data: json!({"code":kind}),
        }),
    })
    .unwrap()
}
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

pub fn run_stdio<C: DaemonClient>(
    mut input: impl io::BufRead,
    mut output: impl Write,
    client: &mut C,
) -> io::Result<()> {
    let mut line = String::new();
    while input.read_line(&mut line)? > 0 {
        let response = handle_line(line.trim_end(), client);
        serde_json::to_writer(&mut output, &response)?;
        output.write_all(b"\n")?;
        output.flush()?;
        line.clear();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Mock(Vec<String>);
    impl DaemonClient for Mock {
        fn operation(&mut self, op: &str, _: Value) -> Result<Value, AdapterError> {
            self.0.push(op.into());
            Ok(json!({"ok":true}))
        }
    }
    #[test]
    fn parser_and_allowlist() {
        assert!(parse_request(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#).is_ok());
        let mut m = Mock(Vec::new());
        let out = handle_line(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"files.read"}}"#,
            &mut m,
        );
        assert_eq!(out["error"]["data"]["code"], "unsupported_method");
        assert!(m.0.is_empty());
    }
    #[test]
    fn authorization_is_scoped_and_constant_time() {
        let auth = TokenAuth::new("secret").unwrap();
        assert!(!format!("{auth:?}").contains("secret"));
        assert!(auth.verify("secret"));
        assert!(!auth.verify("other"));
        assert!(TokenAuth::new("").is_err());
    }
    #[test]
    fn schemas_only_expose_safe_tools() {
        let names: Vec<_> = tool_definitions().into_iter().map(|t| t.name).collect();
        assert_eq!(names, METHODS);
        let send = tool_definitions()
            .into_iter()
            .find(|t| t.name == "transfers.send")
            .unwrap();
        assert!(send.input_schema["properties"]
            .get("destination_path")
            .is_none());
    }
    #[test]
    fn offline_mock_client() {
        let mut m = Mock(Vec::new());
        let out = handle_line(
            r#"{"jsonrpc":"2.0","id":"x","method":"tools/call","params":{"name":"peers.list"}}"#,
            &mut m,
        );
        assert_eq!(out["result"]["isError"], false);
        assert_eq!(m.0, vec!["peers.list"]);
    }
}
