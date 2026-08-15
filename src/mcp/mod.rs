//! A minimal MCP (Model Context Protocol) server over stdio, so AI assistants
//! can drive runtime-orbit in plain language.
//!
//! MCP's stdio transport is newline-delimited JSON-RPC 2.0. We keep stdout
//! exclusively for protocol messages and route each tool call to the CLI
//! as a subprocess (capturing its output) — that way the human-facing `println!`
//! output of the commands never corrupts the JSON channel.

use anyhow::Result;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const PROTOCOL_VERSION: &str = "2024-11-05";

const INSTRUCTIONS: &str = "\
runtime-orbit lets a machine that is low on RAM (the *borrower*) use another machine's \
container runtime (the *donor*) over SSH, forwarding published container ports back to \
the borrower's localhost so it feels local.

Typical flow on a borrower:
- `status` / `dashboard` — is it linked and up? what's forwarded? both machines' RAM, \
CPU, traffic, and the containers the donor is carrying. `dashboard` returns JSON.
- `up` / `down` — start/stop routing docker to the donor.
- `doctor` — diagnose SSH, the donor's runtime, the forwarded socket, the docker context.
- `engines` — which container runtimes exist on each machine.
- `add_forward` / `remove_forward` / `list_forwards` — manage TCP port tunnels.
- `limits_show` / `limits_set` — RAM budgets: how much to borrow, and how much local RAM \
to use before new work is routed to the donor.
- `route_list` / `route_add` / `route_explain` — the routing table deciding local vs donor \
per workload.

On a donor: `donor_status` and `donor_doctor`.

First-time setup is interactive (it may ask for a password or show a pairing code), so \
for that tell the user to run `runtime-orbit setup --ip <donor-ip>` in their terminal \
rather than trying to proxy the prompts. `setup_hint` returns the exact command.";

/// Serve MCP over stdio until stdin closes.
pub async fn serve() -> Result<()> {
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    let mut stdout = tokio::io::stdout();
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            break; // EOF
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);

        // Notifications (no id) get no response.
        let Some(id) = id else {
            continue;
        };

        let response = match method {
            "initialize" => ok(id, initialize_result()),
            "tools/list" => ok(id, json!({ "tools": tool_specs() })),
            "tools/call" => match call_tool(&params).await {
                Ok(text) => ok(id, tool_text(&text, false)),
                Err(e) => ok(id, tool_text(&format!("{e:#}"), true)),
            },
            "ping" => ok(id, json!({})),
            _ => err(id, -32601, "method not found"),
        };

        let mut buf = serde_json::to_string(&response)?;
        buf.push('\n');
        stdout.write_all(buf.as_bytes()).await?;
        stdout.flush().await?;
    }
    Ok(())
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "runtime-orbit", "version": env!("CARGO_PKG_VERSION") },
        "instructions": INSTRUCTIONS,
    })
}

fn ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn err(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn tool_text(text: &str, is_error: bool) -> Value {
    json!({
        "content": [ { "type": "text", "text": text } ],
        "isError": is_error,
    })
}

fn no_args() -> Value {
    json!({ "type": "object", "properties": {}, "additionalProperties": false })
}

fn tool_specs() -> Value {
    json!([
        { "name": "status", "description": "Show the link, connection state, forwarded ports, and the donor's CPU/RAM/image counts.", "inputSchema": no_args() },
        { "name": "dashboard", "description": "Full machine-readable snapshot as JSON: both machines (OS, cores, RAM used/available, IP, load), connection and routing state, the donor's containers with CPU/memory/network, forwarded ports, and the configured budgets.", "inputSchema": no_args() },
        { "name": "up", "description": "Route Docker to the linked donor and start forwarding published ports. Detached.", "inputSchema": no_args() },
        { "name": "down", "description": "Stop forwarding, close the SSH connection, and put Docker back on this machine's engine.", "inputSchema": no_args() },
        { "name": "doctor", "description": "Diagnose SSH, the donor's runtime, the forwarded socket, and the docker context. Returns fixes.", "inputSchema": no_args() },
        { "name": "engines", "description": "List the container runtimes present on this machine and on the donor, and which socket is in use.", "inputSchema": no_args() },
        { "name": "list_forwards", "description": "List the ports currently forwarded from the donor to localhost.", "inputSchema": no_args() },
        { "name": "donor_status", "description": "Run on a donor: what it is lending right now, and to whom.", "inputSchema": no_args() },
        { "name": "donor_doctor", "description": "Run on a donor: whether it can lend its runtime, and what to fix.", "inputSchema": no_args() },
        { "name": "limits_show", "description": "Show the RAM/CPU budgets and where the next container would run.", "inputSchema": no_args() },
        { "name": "route_list", "description": "Show the routing table (which workloads run local vs on the donor).", "inputSchema": no_args() },
        {
            "name": "limits_set",
            "description": "Change the budgets. Values are in GB; pass the string \"off\" to clear one.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "max_ram": { "type": "string", "description": "Ceiling on borrowed RAM in GB, or \"off\"" },
                    "local_ram_threshold": { "type": "string", "description": "Local RAM in use before new work goes to the donor, in GB, or \"off\"" },
                    "prefer": { "type": "string", "enum": ["auto", "local", "donor"] }
                },
                "additionalProperties": false
            }
        },
        {
            "name": "route_add",
            "description": "Append a routing rule. First match wins, so add specific patterns first.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Glob matched against image/container name, e.g. postgres:*" },
                    "target": { "type": "string", "enum": ["local", "donor"] },
                    "note": { "type": "string" }
                },
                "required": ["pattern", "target"]
            }
        },
        {
            "name": "route_explain",
            "description": "Explain where a given image or container name would run, and why.",
            "inputSchema": {
                "type": "object",
                "properties": { "image": { "type": "string" } },
                "required": ["image"]
            }
        },
        {
            "name": "link",
            "description": "Link this machine to a donor. Authorization must already work (else use setup_hint).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": { "type": "string", "description": "user@host or just host" },
                    "port": { "type": "integer", "description": "SSH port (default 22)" }
                },
                "required": ["target"]
            }
        },
        {
            "name": "add_forward",
            "description": "Forward an extra TCP port from the donor to localhost.",
            "inputSchema": { "type": "object", "properties": { "port": { "type": "integer" } }, "required": ["port"] }
        },
        {
            "name": "remove_forward",
            "description": "Stop forwarding a previously added TCP port.",
            "inputSchema": { "type": "object", "properties": { "port": { "type": "integer" } }, "required": ["port"] }
        },
        {
            "name": "setup_hint",
            "description": "How to run first-time setup, which is interactive. Returns the exact command for the user to run in a terminal.",
            "inputSchema": no_args()
        }
    ])
}

/// Dispatch a tool call to the orbit CLI subprocess and return its output.
async fn call_tool(params: &Value) -> Result<String> {
    let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);

    let argv: Vec<String> = match name {
        "status" => vec!["status".into()],
        "dashboard" => vec!["dashboard".into(), "--once".into(), "--json".into()],
        "up" => vec!["up".into()],
        "down" => vec!["down".into()],
        "doctor" => vec!["doctor".into()],
        "engines" => vec!["engines".into()],
        "list_forwards" => vec!["ports".into()],
        "donor_status" => vec!["donor".into(), "status".into()],
        "donor_doctor" => vec!["donor".into(), "doctor".into()],
        "limits_show" => vec!["limits".into(), "show".into()],
        "route_list" => vec!["route".into(), "list".into()],
        "limits_set" => {
            let mut v = vec!["limits".to_string(), "set".to_string()];
            for (key, flag) in [
                ("max_ram", "--max-ram"),
                ("local_ram_threshold", "--local-ram-threshold"),
                ("prefer", "--prefer"),
            ] {
                if let Some(val) = args.get(key).and_then(|x| x.as_str()) {
                    v.push(flag.into());
                    v.push(val.to_string());
                }
            }
            if v.len() == 2 {
                anyhow::bail!(
                    "limits_set needs at least one of: max_ram, local_ram_threshold, prefer"
                );
            }
            v
        }
        "route_add" => {
            let pattern = args
                .get("pattern")
                .and_then(|p| p.as_str())
                .ok_or_else(|| anyhow::anyhow!("route_add requires a 'pattern'"))?;
            let target = args
                .get("target")
                .and_then(|t| t.as_str())
                .ok_or_else(|| anyhow::anyhow!("route_add requires a 'target'"))?;
            let mut v = vec![
                "route".to_string(),
                "add".to_string(),
                pattern.to_string(),
                "--target".to_string(),
                target.to_string(),
            ];
            if let Some(note) = args.get("note").and_then(|n| n.as_str()) {
                v.push("--note".into());
                v.push(note.to_string());
            }
            v
        }
        "route_explain" => {
            let image = args
                .get("image")
                .and_then(|i| i.as_str())
                .ok_or_else(|| anyhow::anyhow!("route_explain requires an 'image'"))?;
            vec!["route".into(), "explain".into(), image.to_string()]
        }
        "link" => {
            let target = args
                .get("target")
                .and_then(|t| t.as_str())
                .ok_or_else(|| anyhow::anyhow!("link requires a 'target'"))?;
            let mut v = vec!["link".to_string(), target.to_string()];
            if let Some(p) = args.get("port").and_then(|p| p.as_u64()) {
                v.push("--port".into());
                v.push(p.to_string());
            }
            v
        }
        "add_forward" => {
            let port = args
                .get("port")
                .and_then(|p| p.as_u64())
                .ok_or_else(|| anyhow::anyhow!("add_forward requires a 'port'"))?;
            vec!["ports".into(), "add".into(), port.to_string()]
        }
        "remove_forward" => {
            let port = args
                .get("port")
                .and_then(|p| p.as_u64())
                .ok_or_else(|| anyhow::anyhow!("remove_forward requires a 'port'"))?;
            vec!["ports".into(), "rm".into(), port.to_string()]
        }
        "setup_hint" => {
            return Ok(
                "Run `runtime-orbit setup --ip <donor-ip>` in a terminal (omit --ip to \
                pick from a LAN scan). It is interactive: it authorizes this machine on the \
                donor — either by asking for the donor's login password once, or by showing a \
                6-digit pairing code to use with `runtime-orbit donor pair <this-ip>` on the \
                donor — then links, brings the borrow up, and runs an end-to-end self-test. \
                On the donor itself, the equivalent is `runtime-orbit donor setup`."
                    .to_string(),
            )
        }
        other => anyhow::bail!("unknown tool: {other}"),
    };

    run_cli(&argv).await
}

/// Invoke this same binary with the given args, capturing combined output.
async fn run_cli(args: &[String]) -> Result<String> {
    let exe = std::env::current_exe()?;
    let out = tokio::process::Command::new(exe)
        .args(args)
        .output()
        .await?;
    let mut text = String::new();
    text.push_str(&strip_ansi(&String::from_utf8_lossy(&out.stdout)));
    let errs = strip_ansi(&String::from_utf8_lossy(&out.stderr));
    if !errs.trim().is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&errs);
    }
    if !out.status.success() {
        anyhow::bail!("{}", text.trim());
    }
    Ok(text.trim().to_string())
}

/// Strip ANSI color codes so the AI sees clean text.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // skip until a letter (end of CSI sequence)
            for n in chars.by_ref() {
                if n.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}
