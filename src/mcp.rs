use std::{collections::BTreeMap, fs, path::Path};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use toml_edit::{value, Array, DocumentMut, Item, Table};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum McpTransport {
    Stdio,
    Http,
    Sse,
    WebSocket,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct McpServer {
    pub transport: Option<McpTransport>,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub url: Option<String>,
    pub env: BTreeMap<String, String>,
    pub headers_env: BTreeMap<String, String>,
    pub headers_helper: Option<String>,
}

pub fn discover_claude_mcp(path: &Path) -> Result<BTreeMap<String, McpServer>> {
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let raw = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let json: Value =
        serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
    let Some(servers) = json.get("mcpServers").and_then(Value::as_object) else {
        return Ok(BTreeMap::new());
    };
    let mut out = BTreeMap::new();
    for (name, value) in servers {
        if let Some(server) = mcp_from_claude_value(value) {
            out.insert(name.clone(), server);
        }
    }
    Ok(out)
}

pub fn discover_codex_mcp(path: &Path) -> Result<BTreeMap<String, McpServer>> {
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let raw = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let doc = raw
        .parse::<DocumentMut>()
        .with_context(|| format!("parse {}", path.display()))?;
    let Some(servers) = doc.get("mcp_servers").and_then(Item::as_table) else {
        return Ok(BTreeMap::new());
    };
    let mut out = BTreeMap::new();
    for (name, item) in servers.iter() {
        let Some(table) = item.as_table() else {
            continue;
        };
        let mut server = McpServer::default();
        if let Some(command) = table.get("command").and_then(Item::as_str) {
            server.transport = Some(McpTransport::Stdio);
            server.command = Some(command.to_string());
        }
        if let Some(url) = table.get("url").and_then(Item::as_str) {
            server.transport = Some(McpTransport::Http);
            server.url = Some(url.to_string());
        }
        if let Some(args) = table.get("args").and_then(Item::as_array) {
            server.args = args
                .iter()
                .filter_map(|item| item.as_str().map(ToString::to_string))
                .collect();
        }
        if let Some(env) = table.get("env").and_then(Item::as_table) {
            server.env = env
                .iter()
                .filter_map(|(key, item)| {
                    if secret_like_key(key) {
                        None
                    } else {
                        item.as_str()
                            .map(|value| (key.to_string(), value.to_string()))
                    }
                })
                .collect();
        }
        if let Some(headers) = table.get("env_http_headers").and_then(Item::as_table) {
            server.headers_env = headers
                .iter()
                .filter_map(|(key, item)| {
                    item.as_str()
                        .map(|value| (key.to_string(), value.to_string()))
                })
                .collect();
        }
        if server.transport.is_some() {
            out.insert(name.to_string(), server);
        }
    }
    Ok(out)
}

pub fn write_claude_mcp(path: &Path, servers: &BTreeMap<String, McpServer>) -> Result<Vec<u8>> {
    let mut root: Value = if path.exists() {
        serde_json::from_str(&fs::read_to_string(path)?)?
    } else {
        json!({})
    };
    if !root.is_object() {
        root = json!({});
    }
    let object = root.as_object_mut().expect("object checked");
    let entry = object.entry("mcpServers").or_insert_with(|| json!({}));
    if !entry.is_object() {
        *entry = json!({});
    }
    let map = entry.as_object_mut().expect("object checked");
    for (name, server) in servers {
        map.insert(name.clone(), mcp_to_claude_value(server));
    }
    Ok([serde_json::to_vec_pretty(&root)?, b"\n".to_vec()].concat())
}

pub fn write_codex_mcp(path: &Path, servers: &BTreeMap<String, McpServer>) -> Result<Vec<u8>> {
    let raw = if path.exists() {
        fs::read_to_string(path)?
    } else {
        String::new()
    };
    let mut doc = raw.parse::<DocumentMut>().unwrap_or_default();
    if !doc.as_table().contains_key("mcp_servers") {
        doc["mcp_servers"] = Item::Table(Table::new());
    }
    for (name, server) in servers {
        let mut table = Table::new();
        match server.transport {
            Some(McpTransport::Stdio) => {
                if let Some(command) = &server.command {
                    table["command"] = value(command);
                }
                let mut args = Array::new();
                for arg in &server.args {
                    args.push(arg.as_str());
                }
                table["args"] = value(args);
                if !server.env.is_empty() {
                    let mut env_table = Table::new();
                    for (key, value_string) in &server.env {
                        if !secret_like_key(key) {
                            env_table[key] = value(value_string);
                        }
                    }
                    if !env_table.is_empty() {
                        table["env"] = Item::Table(env_table);
                    }
                }
            }
            Some(McpTransport::Http) | None => {
                if let Some(url) = &server.url {
                    table["url"] = value(url);
                }
            }
            Some(McpTransport::Sse) | Some(McpTransport::WebSocket) => {
                if let Some(url) = &server.url {
                    table["url"] = value(url);
                }
            }
        }
        if !server.headers_env.is_empty() {
            let mut headers = Table::new();
            for (key, env_name) in &server.headers_env {
                headers[key] = value(env_name);
            }
            table["env_http_headers"] = Item::Table(headers);
        }
        doc["mcp_servers"][name] = Item::Table(table);
    }
    Ok(doc.to_string().into_bytes())
}

pub fn load_pack_mcp(pack: &Path) -> Result<BTreeMap<String, McpServer>> {
    let path = pack.join("mcp").join("servers.json");
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let raw = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))
}

pub fn save_pack_mcp(pack: &Path, servers: &BTreeMap<String, McpServer>) -> Result<()> {
    let path = pack.join("mcp").join("servers.json");
    if let Some(parent) = path.parent() {
        crate::fsx::ensure_dir(parent)?;
    }
    let raw = serde_json::to_vec_pretty(servers)?;
    crate::fsx::write_atomic(&path, &[raw, b"\n".to_vec()].concat())
}

fn mcp_from_claude_value(value: &Value) -> Option<McpServer> {
    let object = value.as_object()?;
    let mut server = McpServer::default();
    let type_value = object.get("type").and_then(Value::as_str);
    if let Some(command) = object.get("command").and_then(Value::as_str) {
        server.transport = Some(McpTransport::Stdio);
        server.command = Some(command.to_string());
    }
    if let Some(url) = object.get("url").and_then(Value::as_str) {
        server.transport = match type_value {
            Some("sse") => Some(McpTransport::Sse),
            Some("ws") | Some("websocket") => Some(McpTransport::WebSocket),
            _ => Some(McpTransport::Http),
        };
        server.url = Some(url.to_string());
    }
    if let Some(args) = object.get("args").and_then(Value::as_array) {
        server.args = args
            .iter()
            .filter_map(|arg| arg.as_str().map(ToString::to_string))
            .collect();
    }
    if let Some(env) = object.get("env").and_then(Value::as_object) {
        for (key, value) in env {
            if secret_like_key(key) {
                continue;
            }
            if let Some(value) = value.as_str() {
                server.env.insert(key.clone(), value.to_string());
            }
        }
    }
    if let Some(helper) = object.get("headersHelper").and_then(Value::as_str) {
        server.headers_helper = Some(helper.to_string());
    }
    if let Some(headers) = object.get("headers").and_then(Value::as_object) {
        for (header, value) in headers {
            let Some(value) = value.as_str() else {
                continue;
            };
            if let Some(env_name) = parse_env_reference(value) {
                server
                    .headers_env
                    .insert(header.clone(), env_name.to_string());
            }
        }
    }
    if server.transport.is_some() {
        Some(server)
    } else {
        None
    }
}

fn mcp_to_claude_value(server: &McpServer) -> Value {
    let mut object = serde_json::Map::new();
    match server.transport {
        Some(McpTransport::Stdio) => {
            object.insert("type".to_string(), json!("stdio"));
            if let Some(command) = &server.command {
                object.insert("command".to_string(), json!(command));
            }
            object.insert("args".to_string(), json!(server.args));
            object.insert("env".to_string(), json!(server.env));
        }
        Some(McpTransport::Sse) => {
            object.insert("type".to_string(), json!("sse"));
            if let Some(url) = &server.url {
                object.insert("url".to_string(), json!(url));
            }
        }
        Some(McpTransport::WebSocket) => {
            object.insert("type".to_string(), json!("ws"));
            if let Some(url) = &server.url {
                object.insert("url".to_string(), json!(url));
            }
        }
        Some(McpTransport::Http) | None => {
            object.insert("type".to_string(), json!("http"));
            if let Some(url) = &server.url {
                object.insert("url".to_string(), json!(url));
            }
        }
    }
    if let Some(helper) = &server.headers_helper {
        object.insert("headersHelper".to_string(), json!(helper));
    }
    if !server.headers_env.is_empty() {
        let headers: serde_json::Map<String, Value> = server
            .headers_env
            .iter()
            .map(|(header, env_name)| (header.clone(), json!(format!("${{{env_name}}}"))))
            .collect();
        object.insert("headers".to_string(), Value::Object(headers));
    }
    Value::Object(object)
}

pub fn secret_like_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    [
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "API_KEY",
        "AUTHORIZATION",
        "PRIVATE",
    ]
    .iter()
    .any(|needle| upper.contains(needle))
}

fn parse_env_reference(value: &str) -> Option<&str> {
    value
        .strip_prefix("${")
        .and_then(|value| value.strip_suffix('}'))
        .filter(|value| !value.is_empty())
}
