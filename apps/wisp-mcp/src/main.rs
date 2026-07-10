//! Stdio MCP server that bridges MCP hosts to the running Wisp desktop app.
//!
//! The desktop app exposes a tiny local HTTP IPC endpoint when `WISP_IPC=1`.
//! This process speaks MCP over stdio and fetches the current transcript from
//! that endpoint only when a tool is called.

use std::fmt::Write as _;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::net::TcpStream;

use serde_json::{Value, json};

const DEFAULT_IPC_ADDR: &str = "127.0.0.1:8765";
const MCP_PROTOCOL_VERSION: &str = "2025-03-26";
const TOOL_NAME: &str = "ask_current_conversation";

fn main() -> io::Result<()> {
    let config = IpcConfig::from_env();
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut writer = BufWriter::new(stdout.lock());

    while let Some((message, framing)) = read_stdio_message(&mut reader)? {
        if let Some(response) = handle_json_rpc(&message, &config) {
            write_stdio_message(&mut writer, &response, framing)?;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StdioFraming {
    ContentLength,
    JsonLine,
}

#[derive(Debug, Clone)]
struct IpcConfig {
    addr: String,
    token: Option<String>,
}

impl IpcConfig {
    fn from_env() -> Self {
        Self {
            addr: std::env::var("WISP_IPC_ADDR").unwrap_or_else(|_| DEFAULT_IPC_ADDR.to_owned()),
            token: std::env::var("WISP_IPC_TOKEN")
                .ok()
                .filter(|s| !s.is_empty()),
        }
    }
}

fn read_stdio_message(reader: &mut impl BufRead) -> io::Result<Option<(Value, StdioFraming)>> {
    let mut content_length = None;
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        let json_line = trimmed.trim_start();
        if content_length.is_none() && (json_line.starts_with('{') || json_line.starts_with('[')) {
            return serde_json::from_str(json_line)
                .map(|value| Some((value, StdioFraming::JsonLine)))
                .map_err(|err| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("invalid JSON-RPC line message: {err}"),
                    )
                });
        }
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            content_length = value.trim().parse::<usize>().ok();
        }
    }
    let Some(content_length) = content_length else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "missing Content-Length header",
        ));
    };
    let mut body = vec![0; content_length];
    reader.read_exact(&mut body)?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|err| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid JSON-RPC message: {err}"),
            )
        })
        .map(|message| message.map(|value| (value, StdioFraming::ContentLength)))
}

fn write_stdio_message(
    writer: &mut impl Write,
    value: &Value,
    framing: StdioFraming,
) -> io::Result<()> {
    let body = serde_json::to_vec(value).map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("failed to serialize JSON-RPC response: {err}"),
        )
    })?;
    if framing == StdioFraming::ContentLength {
        write!(writer, "Content-Length: {}\r\n\r\n", body.len())?;
    }
    writer.write_all(&body)?;
    if framing == StdioFraming::JsonLine {
        writer.write_all(b"\n")?;
    }
    writer.flush()
}

fn handle_json_rpc(
    value: &Value,
    config: &IpcConfig,
) -> Option<Value> {
    if let Some(items) = value.as_array() {
        let responses: Vec<Value> = items
            .iter()
            .filter_map(|item| handle_single_rpc(item, config))
            .collect();
        return (!responses.is_empty()).then_some(Value::Array(responses));
    }
    handle_single_rpc(value, config)
}

fn handle_single_rpc(
    value: &Value,
    config: &IpcConfig,
) -> Option<Value> {
    let id = value.get("id").cloned();
    let Some(method) = value.get("method").and_then(Value::as_str) else {
        let id = id.unwrap_or(Value::Null);
        return Some(rpc_error(&id, -32600, "Invalid Request"));
    };
    let id = id?;
    match method {
        "initialize" => Some(rpc_result(&id, &initialize_result())),
        "ping" => Some(rpc_result(&id, &json!({}))),
        "tools/list" => Some(rpc_result(&id, &tools_list_result())),
        "tools/call" => Some(handle_tools_call(&id, value, config)),
        _ => Some(rpc_error(&id, -32601, "Method not found")),
    }
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": MCP_PROTOCOL_VERSION,
        "capabilities": {
            "tools": {}
        },
        "serverInfo": {
            "name": "wisp-mcp",
            "version": env!("CARGO_PKG_VERSION")
        }
    })
}

fn tools_list_result() -> Value {
    json!({
        "tools": [
            {
                "name": TOOL_NAME,
                "description": "Use the currently visible Wisp transcript to answer questions like 'いまの話ってどういうこと?'. The tool returns transcript context for the host LLM to answer from.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "question": {
                            "type": "string",
                            "description": "Question to answer from the current Wisp transcript."
                        }
                    },
                    "required": ["question"]
                }
            }
        ]
    })
}

fn handle_tools_call(
    id: &Value,
    value: &Value,
    config: &IpcConfig,
) -> Value {
    let params = value.get("params").unwrap_or(&Value::Null);
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if name != TOOL_NAME {
        return rpc_error(id, -32602, "Unknown tool");
    }
    let question = params
        .get("arguments")
        .and_then(|arguments| arguments.get("question"))
        .and_then(Value::as_str)
        .unwrap_or("いまの話ってどういうこと?");

    match fetch_conversation(config) {
        Ok(snapshot) => {
            let context = tool_context(question, &snapshot);
            rpc_result(id, &tool_result(false, &context))
        },
        Err(err) => {
            let text = format!("Could not read the current Wisp transcript: {err}");
            rpc_result(id, &tool_result(true, &text))
        },
    }
}

fn fetch_conversation(config: &IpcConfig) -> Result<Value, String> {
    let mut stream = TcpStream::connect(&config.addr)
        .map_err(|err| format!("failed to connect to {}: {err}", config.addr))?;
    let mut request = format!(
        "GET /conversation HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
        config.addr
    );
    if let Some(token) = &config.token {
        let _ = write!(request, "Authorization: Bearer {token}\r\n");
    }
    request.push_str("\r\n");
    stream
        .write_all(request.as_bytes())
        .map_err(|err| format!("failed to send IPC request: {err}"))?;
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|err| format!("failed to read IPC response: {err}"))?;
    let (headers, body) = response
        .split_once("\r\n\r\n")
        .ok_or_else(|| "malformed IPC HTTP response".to_owned())?;
    let status = headers.lines().next().unwrap_or_default();
    if !status.contains(" 200 ") {
        return Err(format!("IPC server returned {status}"));
    }
    serde_json::from_str(body).map_err(|err| format!("invalid IPC JSON response: {err}"))
}

fn tool_result(
    is_error: bool,
    text: &str,
) -> Value {
    json!({
        "content": [
            {
                "type": "text",
                "text": text
            }
        ],
        "isError": is_error
    })
}

fn tool_context(
    question: &str,
    snapshot: &Value,
) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "User question: {question}");
    if let Some(view) = snapshot.get("view").and_then(Value::as_str) {
        let _ = writeln!(out, "Wisp view: {view}");
    }
    if let Some(state) = snapshot.get("state").and_then(Value::as_str) {
        let _ = writeln!(out, "Recording state: {state}");
    }
    if let Some(session_id) = snapshot.get("session_id").and_then(Value::as_i64) {
        let _ = writeln!(out, "Session id: {session_id}");
    }
    if let Some(title) = snapshot.get("title").and_then(Value::as_str) {
        let _ = writeln!(out, "Session title: {title}");
    }
    if let Some(error) = snapshot.get("last_error").and_then(Value::as_str) {
        let _ = writeln!(out, "Last recording error: {error}");
    }
    out.push_str("\nVisible transcript:\n");
    let mut wrote_segment = false;
    let segments = snapshot
        .get("segments")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    for segment in segments {
        let text = segment
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim();
        if text.is_empty() {
            continue;
        }
        wrote_segment = true;
        let source = segment
            .get("source")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let start = segment
            .get("start_seconds")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let end = segment
            .get("end_seconds")
            .and_then(Value::as_f64)
            .unwrap_or(start);
        let finality = if segment
            .get("is_final")
            .and_then(Value::as_bool)
            .unwrap_or(true)
        {
            ""
        } else {
            " partial"
        };
        let _ = writeln!(out, "[{source} {start:.1}-{end:.1}s{finality}] {text}");
    }
    if !wrote_segment {
        out.push_str(
            "No transcript segments are currently visible. Start or open a Wisp session, then ask again.\n",
        );
    }
    out.push_str(
        "\nInstruction for the host LLM: answer the user question using the Wisp transcript above. If the transcript is insufficient, say what is missing.",
    );
    out
}

fn rpc_result(
    id: &Value,
    result: &Value,
) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result
    })
}

fn rpc_error(
    id: &Value,
    code: i64,
    message: &str,
) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message
        }
    })
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use serde_json::json;

    use super::{
        IpcConfig, StdioFraming, handle_json_rpc, read_stdio_message, tool_context,
        write_stdio_message,
    };

    #[test]
    fn tools_list_exposes_current_conversation_tool() {
        let config = IpcConfig {
            addr: "127.0.0.1:8765".into(),
            token: None,
        };
        let response = handle_json_rpc(
            &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
            &config,
        )
        .expect("response");
        assert_eq!(
            response["result"]["tools"][0]["name"],
            "ask_current_conversation"
        );
    }

    #[test]
    fn reads_json_line_framed_messages() {
        let mut input =
            Cursor::new(b"{\"jsonrpc\":\"2.0\",\"id\":0,\"method\":\"initialize\"}\n".to_vec());
        let (message, framing) = read_stdio_message(&mut input)
            .expect("read")
            .expect("message");
        assert_eq!(framing, StdioFraming::JsonLine);
        assert_eq!(message["method"], "initialize");
    }

    #[test]
    fn writes_json_line_framed_messages() {
        let mut output = Vec::new();
        write_stdio_message(
            &mut output,
            &json!({"jsonrpc": "2.0", "id": 1, "result": {}}),
            StdioFraming::JsonLine,
        )
        .expect("write");
        let output = String::from_utf8(output).expect("utf8");
        assert!(!output.starts_with("Content-Length"));
        assert!(output.ends_with('\n'));
    }

    #[test]
    fn context_includes_transcript_segments() {
        let context = tool_context(
            "いまの話ってどういうこと?",
            &json!({
                "view": "live_session",
                "state": "recording",
                "session_id": 7,
                "title": "demo",
                "segments": [
                    {
                        "source": "mic",
                        "text": "今日はロードマップの話をしています。",
                        "start_seconds": 0.0,
                        "end_seconds": 2.0,
                        "is_final": true
                    }
                ]
            }),
        );
        assert!(context.contains("ロードマップ"));
    }
}
