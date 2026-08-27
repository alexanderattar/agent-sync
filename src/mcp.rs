use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::ErrorKind,
    ops::Range,
    path::Path,
};

use anyhow::{bail, Context, Result};
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

pub fn discover_cursor_mcp_names(path: &Path) -> Result<BTreeSet<String>> {
    if !path.exists() {
        return Ok(BTreeSet::new());
    }
    let raw = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let root: Value =
        serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
    let root = root
        .as_object()
        .with_context(|| format!("{} must contain a JSON object", path.display()))?;
    let Some(servers) = root.get("mcpServers") else {
        return Ok(BTreeSet::new());
    };
    let servers = servers
        .as_object()
        .with_context(|| format!("{}.mcpServers must be a JSON object", path.display()))?;
    Ok(servers.keys().cloned().collect())
}

pub fn discover_cursor_project_mcp_names(cursor_home: &Path) -> Result<BTreeSet<String>> {
    let projects = cursor_home.join("projects");
    if !projects.exists() {
        return Ok(BTreeSet::new());
    }

    let mut names = BTreeSet::new();
    for project in
        fs::read_dir(&projects).with_context(|| format!("read {}", projects.display()))?
    {
        let mcp_dir = project?.path().join("mcps");
        if !mcp_dir.is_dir() {
            continue;
        }
        for entry in
            fs::read_dir(&mcp_dir).with_context(|| format!("read {}", mcp_dir.display()))?
        {
            if let Some(name) = entry?.file_name().to_str() {
                names.insert(name.to_string());
            }
        }
    }
    Ok(names)
}

pub fn discover_cursor_effective_mcp_names(
    cursor_home: &Path,
    cursor_config: &Path,
) -> Result<BTreeSet<String>> {
    let mut names = discover_cursor_mcp_names(cursor_config)?;
    names.extend(discover_cursor_project_mcp_names(cursor_home)?);
    Ok(names)
}

pub fn ensure_cursor_mcp_write_safe(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => bail!(
            "refusing to rewrite symlinked Cursor MCP config {}; update its target explicitly",
            path.display()
        ),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("inspect {}", path.display())),
    }
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

pub fn write_cursor_mcp_additive(
    path: &Path,
    servers: &BTreeMap<String, McpServer>,
) -> Result<Vec<u8>> {
    ensure_cursor_mcp_write_safe(path)?;
    let raw = if path.exists() {
        fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?
    } else {
        "{}\n".to_string()
    };

    let root: Value =
        serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
    let root_object = root
        .as_object()
        .with_context(|| format!("{} must contain a JSON object", path.display()))?;
    let cursor_servers = root_object
        .get("mcpServers")
        .map(|value| {
            value
                .as_object()
                .with_context(|| format!("{}.mcpServers must be a JSON object", path.display()))
        })
        .transpose()?;
    let missing = servers
        .iter()
        .filter(|(name, _)| cursor_servers.is_none_or(|existing| !existing.contains_key(*name)))
        .map(|(name, server)| {
            Ok(format!(
                "{}:{}",
                serde_json::to_string(name)?,
                serde_json::to_string(&mcp_to_claude_value(server))?
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    if missing.is_empty() {
        return Ok(raw.into_bytes());
    }

    let root_layout = inspect_json_object(&raw, first_non_whitespace(&raw)?, Some("mcpServers"))?;
    let (insertion_at, prefix) = if let Some(range) = root_layout.property_value.as_ref() {
        let layout = inspect_json_object(&raw, range.start, None)?;
        (
            layout.closing_brace,
            if layout.has_members { "," } else { "" },
        )
    } else {
        (
            root_layout.closing_brace,
            if root_layout.has_members {
                ",\"mcpServers\":{"
            } else {
                "\"mcpServers\":{"
            },
        )
    };
    let suffix = if root_layout.property_value.is_some() {
        ""
    } else {
        "}"
    };
    let insertion = format!("{prefix}{}{suffix}", missing.join(","));

    let mut output = Vec::with_capacity(raw.len() + insertion.len());
    output.extend_from_slice(&raw.as_bytes()[..insertion_at]);
    output.extend_from_slice(insertion.as_bytes());
    output.extend_from_slice(&raw.as_bytes()[insertion_at..]);
    Ok(output)
}

struct JsonObjectLayout {
    closing_brace: usize,
    has_members: bool,
    property_value: Option<Range<usize>>,
}

fn inspect_json_object(
    raw: &str,
    object_start: usize,
    property: Option<&str>,
) -> Result<JsonObjectLayout> {
    let bytes = raw.as_bytes();
    if bytes.get(object_start) != Some(&b'{') {
        bail!("expected JSON object at byte {object_start}");
    }

    let mut cursor = object_start + 1;
    let mut has_members = false;
    let mut property_value = None;
    loop {
        cursor = skip_json_whitespace(bytes, cursor);
        match bytes.get(cursor) {
            Some(b'}') => {
                return Ok(JsonObjectLayout {
                    closing_brace: cursor,
                    has_members,
                    property_value,
                });
            }
            Some(b'"') => {}
            _ => bail!("expected JSON object key at byte {cursor}"),
        }

        has_members = true;
        let key_start = cursor;
        let key_end = json_string_end(bytes, key_start)?;
        let key: String = serde_json::from_str(&raw[key_start..key_end])?;
        cursor = skip_json_whitespace(bytes, key_end);
        if bytes.get(cursor) != Some(&b':') {
            bail!("expected ':' after JSON object key at byte {cursor}");
        }
        cursor = skip_json_whitespace(bytes, cursor + 1);
        let value_start = cursor;
        let value_end = json_value_end(bytes, value_start)?;
        if property == Some(key.as_str()) {
            if property_value.is_some() {
                bail!("duplicate `{key}` property is not safe to update");
            }
            property_value = Some(value_start..value_end);
        }

        cursor = skip_json_whitespace(bytes, value_end);
        match bytes.get(cursor) {
            Some(b',') => cursor += 1,
            Some(b'}') => {
                return Ok(JsonObjectLayout {
                    closing_brace: cursor,
                    has_members,
                    property_value,
                });
            }
            _ => bail!("expected ',' or '}}' at byte {cursor}"),
        }
    }
}

fn first_non_whitespace(raw: &str) -> Result<usize> {
    let index = skip_json_whitespace(raw.as_bytes(), 0);
    if index == raw.len() {
        bail!("JSON document is empty");
    }
    Ok(index)
}

fn skip_json_whitespace(bytes: &[u8], mut cursor: usize) -> usize {
    while bytes
        .get(cursor)
        .is_some_and(|byte| matches!(byte, b' ' | b'\n' | b'\r' | b'\t'))
    {
        cursor += 1;
    }
    cursor
}

fn json_string_end(bytes: &[u8], start: usize) -> Result<usize> {
    if bytes.get(start) != Some(&b'"') {
        bail!("expected JSON string at byte {start}");
    }
    let mut cursor = start + 1;
    while let Some(byte) = bytes.get(cursor) {
        match byte {
            b'"' => return Ok(cursor + 1),
            b'\\' => cursor += 2,
            _ => cursor += 1,
        }
    }
    bail!("unterminated JSON string at byte {start}")
}

fn json_value_end(bytes: &[u8], start: usize) -> Result<usize> {
    match bytes.get(start) {
        Some(b'"') => json_string_end(bytes, start),
        Some(b'{') | Some(b'[') => {
            let mut closing = vec![if bytes[start] == b'{' { b'}' } else { b']' }];
            let mut cursor = start + 1;
            while let Some(byte) = bytes.get(cursor) {
                match byte {
                    b'"' => cursor = json_string_end(bytes, cursor)?,
                    b'{' => {
                        closing.push(b'}');
                        cursor += 1;
                    }
                    b'[' => {
                        closing.push(b']');
                        cursor += 1;
                    }
                    b'}' | b']' => {
                        if closing.pop() != Some(*byte) {
                            bail!("mismatched JSON delimiter at byte {cursor}");
                        }
                        cursor += 1;
                        if closing.is_empty() {
                            return Ok(cursor);
                        }
                    }
                    _ => cursor += 1,
                }
            }
            bail!("unterminated JSON value at byte {start}")
        }
        Some(_) => {
            let mut cursor = start;
            while bytes.get(cursor).is_some_and(|byte| {
                !matches!(byte, b',' | b'}' | b']' | b' ' | b'\n' | b'\r' | b'\t')
            }) {
                cursor += 1;
            }
            Ok(cursor)
        }
        None => bail!("missing JSON value at byte {start}"),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_mcp_insertion_preserves_existing_root_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("mcp.json");
        let original = "{\n  \"cursorSetting\": 1.2300e+02,\n  \"escaped\": \"\\u0061\"\n}\n";
        fs::write(&path, original).unwrap();
        let servers = BTreeMap::from([(
            "example".to_string(),
            McpServer {
                transport: Some(McpTransport::Http),
                url: Some("https://example.invalid/mcp".to_string()),
                ..McpServer::default()
            },
        )]);

        let updated =
            String::from_utf8(write_cursor_mcp_additive(&path, &servers).unwrap()).unwrap();
        let insertion_at = original.rfind('}').unwrap();
        let insertion_end = updated.len() - (original.len() - insertion_at);
        assert_eq!(&updated[..insertion_at], &original[..insertion_at]);
        assert_eq!(&updated[insertion_end..], &original[insertion_at..]);
        assert!(updated[insertion_at..insertion_end].starts_with(",\"mcpServers\":{"));

        let parsed: Value = serde_json::from_str(&updated).unwrap();
        assert_eq!(parsed["cursorSetting"], 123.0);
        assert_eq!(parsed["escaped"], "a");
        assert_eq!(
            parsed["mcpServers"]["example"]["url"],
            "https://example.invalid/mcp"
        );
    }
}
