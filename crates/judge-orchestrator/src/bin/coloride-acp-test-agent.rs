//! Deterministic stdio fixture for LOT-014 integration tests; no provider or credential support.
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};

fn emit(value: Value) {
    let mut output = io::stdout().lock();
    writeln!(output, "{value}").unwrap();
    output.flush().unwrap();
}
fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();
    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();
    let mut cwd = String::new();
    for (id, result) in [
        (0, json!({"protocolVersion":1,"agentCapabilities":{}})),
        (1, json!({"sessionId":"fake"})),
    ] {
        let Some(Ok(line)) = lines.next() else {
            return;
        };
        if id == 1 {
            let request: Value = serde_json::from_str(&line).unwrap_or_default();
            cwd = request["params"]["cwd"].as_str().unwrap_or_default().into();
        }
        emit(json!({"jsonrpc":"2.0","id":id,"result":result}));
    }
    if lines.next().is_none() {
        return;
    }
    let request = |id, method, mut params: Value| {
        params["sessionId"] = json!("fake");
        emit(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}));
    };
    match mode.as_str() {
        "write" => request(
            10,
            "fs/write_text_file",
            json!({"path":"src/file.rs","content":"new 🦀\n"}),
        ),
        "read" => request(10, "fs/read_text_file", json!({"path":"src/file.rs"})),
        // What real agents send: absolute paths under the cwd of `session/new`.
        "absolute" => {
            request(
                10,
                "fs/read_text_file",
                json!({"path":format!("{cwd}/src/file.rs")}),
            );
            emit(
                json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"fake","update":{"sessionUpdate":"tool_call","toolCallId":"t","title":"read","locations":[{"path":format!("{cwd}/src/file.rs"),"line":3}]}}}),
            );
        }
        "terminal-verbose" => request(
            10,
            "terminal/create",
            json!({"command":"/bin/sh","args":["-c","i=0; while [ $i -lt 4000 ]; do printf '\\033[31mline %s\\n' $i; i=$((i+1)); done"],"cwd":"."}),
        ),
        "paths" => {
            for (i, path) in [
                "../escape",
                "/etc/passwd",
                "C:/Windows/file",
                "src/../escape",
                "src/SECRETS/key",
                "src/sEcReTs/key",
                "src\\file.rs",
            ]
            .iter()
            .enumerate()
            {
                request(10 + i, "fs/read_text_file", json!({"path":path}));
            }
        }
        "unknown" => request(10, "execute_arbitrary_code", json!({})),
        "oversized" => emit(
            json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"fake","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"x".repeat(1_100_000)}}}}),
        ),
        "malformed" => {
            println!("{{broken}}");
            return;
        }
        "flood" => {
            for i in 0..34 {
                request(
                    10 + i,
                    "session/request_permission",
                    json!({"toolCall":{"toolCallId":"tool"},"options":[]}),
                );
            }
        }
        "terminal-complete" => request(
            10,
            "terminal/create",
            json!({"command":"/bin/sh","args":["-c","printf terminal-output"],"cwd":"."}),
        ),
        "terminal" => request(
            10,
            "terminal/create",
            json!({"command":"/bin/sh","args":["-c","sleep 60"],"cwd":"."}),
        ),
        "direct-read" => {
            let read = std::fs::read_to_string("src/file.rs").is_ok();
            emit(
                json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"fake","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":format!("direct-read={read}")}}}}),
            );
        }
        "direct-write" => {
            let wrote = std::fs::write("src/file.rs", "bypass").is_ok();
            emit(
                json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"fake","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":format!("direct-write={wrote}")}}}}),
            );
        }
        "environment" => {
            emit(
                json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"fake","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":std::env::vars().map(|(k, _)|k).collect::<Vec<_>>().join(",")}}}}),
            );
        }
        _ => {}
    }
    for line in lines {
        let Ok(line) = line else {
            break;
        };
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            break;
        };
        if value["method"] == "session/cancel" {
            break;
        }
        emit(
            json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"fake","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":value.to_string()}}}}),
        );
        if mode == "terminal-verbose" && value["id"] == 10 {
            request(
                11,
                "terminal/output",
                json!({"terminalId":value["result"]["terminalId"]}),
            );
        }
        if mode == "terminal-complete" {
            let id = value["id"].as_u64().unwrap_or(0);
            let terminal = "10";
            match id {
                10 => request(
                    11,
                    "terminal/output",
                    json!({"terminalId":value["result"]["terminalId"]}),
                ),
                11 => request(12, "terminal/wait_for_exit", json!({"terminalId":terminal})),
                12 => request(13, "terminal/kill", json!({"terminalId":terminal})),
                13 => request(14, "terminal/release", json!({"terminalId":terminal})),
                14 => request(15, "terminal/output", json!({"terminalId":terminal})),
                _ => {}
            }
        }
    }
}
