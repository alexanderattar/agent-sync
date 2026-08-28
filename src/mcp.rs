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

use crate::fsx::{
    env_reference, raw_secret_reason, safe_secret_reference_or_placeholder, sensitive_key,
    valid_env_name,
};

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
    #[serde(default)]
    pub bearer_token_env_var: Option<String>,
    pub headers_helper: Option<String>,
}

#[derive(Clone, Copy)]
enum JsonMcpFlavor {
    Claude,
    Cursor,
}

pub(crate) type CursorMcpSnapshot = (Option<Vec<u8>>, BTreeMap<String, Value>);

pub fn discover_claude_mcp(path: &Path) -> Result<BTreeMap<String, McpServer>> {
    discover_claude_mcp_with_policy(path, None)
}

pub(crate) fn discover_claude_mcp_for_export(
    path: &Path,
    selected: &[String],
) -> Result<BTreeMap<String, McpServer>> {
    discover_claude_mcp_with_policy(path, Some(selected))
}

fn discover_claude_mcp_with_policy(
    path: &Path,
    export_selection: Option<&[String]>,
) -> Result<BTreeMap<String, McpServer>> {
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
        if let Some(selected) = export_selection {
            if !mcp_selected(selected, name) {
                continue;
            }
            validate_claude_mcp_value_for_export(name, value)
                .with_context(|| format!("MCP server `{name}` from {}", path.display()))?;
        }
        if let Some(server) = mcp_from_json_value(value, JsonMcpFlavor::Claude) {
            if export_selection.is_some() {
                validate_mcp_server_for_export(name, &server)
                    .with_context(|| format!("MCP server `{name}` from {}", path.display()))?;
            }
            out.insert(name.clone(), server);
        }
    }
    Ok(out)
}

pub fn discover_codex_mcp(path: &Path) -> Result<BTreeMap<String, McpServer>> {
    discover_codex_mcp_with_policy(path, None)
}

pub(crate) fn discover_codex_mcp_for_export(
    path: &Path,
    selected: &[String],
) -> Result<BTreeMap<String, McpServer>> {
    discover_codex_mcp_with_policy(path, Some(selected))
}

fn discover_codex_mcp_with_policy(
    path: &Path,
    export_selection: Option<&[String]>,
) -> Result<BTreeMap<String, McpServer>> {
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
        if let Some(selected) = export_selection {
            if !mcp_selected(selected, name) {
                continue;
            }
        }
        let table = match item.as_table() {
            Some(table) => table,
            None if export_selection.is_some() => {
                bail!(
                    "MCP server `{name}` from {} must be a table",
                    path.display()
                )
            }
            None => continue,
        };
        if export_selection.is_some() {
            validate_codex_mcp_table_for_export(name, table)
                .with_context(|| format!("MCP server `{name}` from {}", path.display()))?;
        }
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
        if let Some(env) = table.get("env").and_then(Item::as_table_like) {
            server.env = env
                .iter()
                .filter_map(|(key, item)| {
                    item.as_str()
                        .filter(|value| portable_codex_literal_env_entry(key, value))
                        .map(|value| (key.to_string(), value.to_string()))
                })
                .collect();
        }
        for name in supported_codex_env_vars(table) {
            server.env.insert(name.clone(), format!("${{{name}}}"));
        }
        if let Some(headers) = table.get("env_http_headers").and_then(Item::as_table_like) {
            server.headers_env = headers
                .iter()
                .filter_map(|(key, item)| {
                    item.as_str()
                        .map(|value| (key.to_string(), value.to_string()))
                })
                .collect();
        }
        server.bearer_token_env_var = table
            .get("bearer_token_env_var")
            .and_then(Item::as_str)
            .filter(|name| valid_env_name(name))
            .map(ToString::to_string);
        if server.transport.is_some() {
            if export_selection.is_some() {
                validate_mcp_server_for_export(name, &server)
                    .with_context(|| format!("MCP server `{name}` from {}", path.display()))?;
            }
            out.insert(name.to_string(), server);
        }
    }
    Ok(out)
}

fn mcp_selected(selected: &[String], name: &str) -> bool {
    selected.is_empty() || selected.iter().any(|selected| selected == name)
}

pub fn discover_cursor_mcp_names(path: &Path) -> Result<BTreeSet<String>> {
    Ok(discover_cursor_mcp_values(path)?.into_keys().collect())
}

pub(crate) fn discover_cursor_mcp_values(path: &Path) -> Result<BTreeMap<String, Value>> {
    let (_, values) = read_cursor_mcp_snapshot(path)?;
    Ok(values)
}

pub(crate) fn read_cursor_mcp_snapshot(path: &Path) -> Result<CursorMcpSnapshot> {
    ensure_cursor_mcp_write_safe(path)?;
    let raw = match fs::read(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok((None, BTreeMap::new())),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let values = cursor_mcp_values_from_bytes(path, &raw)?;
    Ok((Some(raw), values))
}

fn cursor_mcp_values_from_bytes(path: &Path, raw: &[u8]) -> Result<BTreeMap<String, Value>> {
    let root: Value =
        serde_json::from_slice(raw).with_context(|| format!("parse {}", path.display()))?;
    let root = root
        .as_object()
        .with_context(|| format!("{} must contain a JSON object", path.display()))?;
    let Some(servers) = root.get("mcpServers") else {
        return Ok(BTreeMap::new());
    };
    let servers = servers
        .as_object()
        .with_context(|| format!("{}.mcpServers must be a JSON object", path.display()))?;
    Ok(servers
        .iter()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect())
}

pub fn discover_cursor_mcp(path: &Path) -> Result<BTreeMap<String, McpServer>> {
    discover_cursor_mcp_with_policy(path, None)
}

pub(crate) fn discover_cursor_mcp_for_export(
    path: &Path,
    selected: &[String],
) -> Result<BTreeMap<String, McpServer>> {
    discover_cursor_mcp_with_policy(path, Some(selected))
}

fn discover_cursor_mcp_with_policy(
    path: &Path,
    export_selection: Option<&[String]>,
) -> Result<BTreeMap<String, McpServer>> {
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let raw = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let root: Value =
        serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
    let root = root
        .as_object()
        .with_context(|| format!("{} must contain a JSON object", path.display()))?;
    let Some(servers) = root.get("mcpServers") else {
        return Ok(BTreeMap::new());
    };
    let servers = servers
        .as_object()
        .with_context(|| format!("{}.mcpServers must be a JSON object", path.display()))?;
    let mut out = BTreeMap::new();
    for (name, value) in servers {
        if let Some(selected) = export_selection {
            if !mcp_selected(selected, name) {
                continue;
            }
            validate_cursor_mcp_value_for_export(name, value)
                .with_context(|| format!("MCP server `{name}` from {}", path.display()))?;
        }
        let Some(server) = mcp_from_json_value(value, JsonMcpFlavor::Cursor) else {
            continue;
        };
        if export_selection.is_some() {
            validate_mcp_server_for_export(name, &server)
                .with_context(|| format!("MCP server `{name}` from {}", path.display()))?;
        }
        out.insert(name.clone(), server);
    }
    Ok(out)
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
            let entry = entry?;
            if let Some(name) = entry.file_name().to_str() {
                names.insert(name.to_string());
            }
            let metadata_path = entry.path().join("SERVER_METADATA.json");
            match fs::read_to_string(&metadata_path) {
                Ok(raw) => {
                    let metadata: Value = serde_json::from_str(&raw)
                        .with_context(|| format!("parse {}", metadata_path.display()))?;
                    let server_name = metadata
                        .get("serverName")
                        .and_then(Value::as_str)
                        .filter(|name| !name.is_empty())
                        .with_context(|| {
                            format!("{} has no serverName", metadata_path.display())
                        })?;
                    names.insert(server_name.to_string());
                }
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| format!("read {}", metadata_path.display()))
                }
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
    validate_mcp_servers_for_render(servers)?;
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
        map.insert(
            name.clone(),
            mcp_to_claude_value(server)
                .with_context(|| format!("render MCP server `{name}` for Claude"))?,
        );
    }
    Ok([serde_json::to_vec_pretty(&root)?, b"\n".to_vec()].concat())
}

pub fn write_codex_mcp(path: &Path, servers: &BTreeMap<String, McpServer>) -> Result<Vec<u8>> {
    validate_mcp_servers_for_render(servers)?;
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
        if server.headers_helper.is_some() {
            bail!("MCP server `{name}` uses headersHelper, which Codex does not support");
        }
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
                    let mut env_vars = Array::new();
                    for (key, value_string) in &server.env {
                        if let Some(reference) = env_reference(value_string) {
                            if reference != key {
                                bail!(
                                    "MCP server `{name}` environment entry `{key}` references `{reference}`; Codex env_vars can only pass through the same variable name"
                                );
                            }
                            env_vars.push(key.as_str());
                        } else if secret_like_key(key) {
                            bail!(
                                "MCP server `{name}` environment entry `{key}` contains a literal credential"
                            );
                        } else {
                            env_table[key] = value(value_string);
                        }
                    }
                    if !env_table.is_empty() {
                        table["env"] = Item::Table(env_table);
                    }
                    if !env_vars.is_empty() {
                        table["env_vars"] = value(env_vars);
                    }
                }
            }
            Some(McpTransport::Http) | None => {
                if let Some(url) = &server.url {
                    table["url"] = value(url);
                }
            }
            Some(McpTransport::Sse) => {
                bail!("MCP server `{name}` uses SSE, which Codex does not support")
            }
            Some(McpTransport::WebSocket) => {
                bail!("MCP server `{name}` uses WebSocket, which Codex does not support")
            }
        }
        if !server.headers_env.is_empty() {
            let mut headers = Table::new();
            for (key, env_name) in &server.headers_env {
                headers[key] = value(env_name);
            }
            table["env_http_headers"] = Item::Table(headers);
        }
        if let Some(env_name) = &server.bearer_token_env_var {
            require_valid_bearer_env(name, env_name)?;
            table["bearer_token_env_var"] = value(env_name);
        }
        doc["mcp_servers"][name] = Item::Table(table);
    }
    Ok(doc.to_string().into_bytes())
}

pub(crate) fn render_cursor_mcp_additive_with_updates(
    path: &Path,
    existing: Option<&[u8]>,
    servers: &BTreeMap<String, McpServer>,
    managed_updates: &BTreeSet<String>,
) -> Result<Vec<u8>> {
    validate_mcp_servers_for_render(servers)?;
    let raw = match existing {
        Some(raw) => std::str::from_utf8(raw)
            .with_context(|| format!("{} must contain UTF-8 JSON", path.display()))?,
        None => "{}\n",
    };

    let root: Value =
        serde_json::from_str(raw).with_context(|| format!("parse {}", path.display()))?;
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
                serde_json::to_string(
                    &mcp_to_cursor_value(server)
                        .with_context(|| format!("render MCP server `{name}` for Cursor"))?
                )?
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    let root_layout = inspect_json_object(raw, first_non_whitespace(raw)?, Some("mcpServers"))?;
    let cursor_layout = root_layout
        .property_value
        .as_ref()
        .map(|range| inspect_json_object(raw, range.start, None))
        .transpose()?;
    let mut replacements = managed_updates
        .iter()
        .filter_map(|name| servers.get(name).map(|server| (name, server)))
        .map(|(name, server)| {
            let layout = root_layout
                .property_value
                .as_ref()
                .context("managed Cursor MCP update requires an mcpServers object")?;
            let property = inspect_json_object(raw, layout.start, Some(name))?
                .property_value
                .with_context(|| format!("managed Cursor MCP server `{name}` is missing"))?;
            Ok((
                property,
                serde_json::to_string(
                    &cursor_mcp_value(server)
                        .with_context(|| format!("render MCP server `{name}` for Cursor"))?,
                )?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    replacements.sort_by_key(|(range, _)| range.start);
    for pair in replacements.windows(2) {
        if pair[0].0.end > pair[1].0.start {
            bail!("managed Cursor MCP update ranges overlap");
        }
    }

    if missing.is_empty() && replacements.is_empty() {
        return Ok(raw.as_bytes().to_vec());
    }

    let (insertion_at, prefix) = if root_layout.property_value.is_some() {
        let layout = cursor_layout
            .as_ref()
            .context("Cursor mcpServers layout was not inspected")?;
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
    let insertion = if missing.is_empty() {
        String::new()
    } else {
        let suffix = if root_layout.property_value.is_some() {
            ""
        } else {
            "}"
        };
        format!("{prefix}{}{suffix}", missing.join(","))
    };

    let mut output = Vec::with_capacity(
        raw.len()
            + insertion.len()
            + replacements
                .iter()
                .map(|(_, value)| value.len())
                .sum::<usize>(),
    );
    let mut cursor = 0;
    for (range, value) in replacements {
        output.extend_from_slice(&raw.as_bytes()[cursor..range.start]);
        output.extend_from_slice(value.as_bytes());
        cursor = range.end;
    }
    output.extend_from_slice(&raw.as_bytes()[cursor..insertion_at]);
    output.extend_from_slice(insertion.as_bytes());
    output.extend_from_slice(&raw.as_bytes()[insertion_at..]);
    Ok(output)
}

pub(crate) fn cursor_mcp_value(server: &McpServer) -> Result<Value> {
    mcp_to_cursor_value(server)
}

pub(crate) fn cursor_mcp_server(value: &Value) -> Option<McpServer> {
    mcp_from_json_value(value, JsonMcpFlavor::Cursor)
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

fn mcp_from_json_value(value: &Value, flavor: JsonMcpFlavor) -> Option<McpServer> {
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
            if let Some(value) = value.as_str() {
                if let Some(env_name) = parse_json_env_reference(flavor, value) {
                    server.env.insert(key.clone(), format!("${{{env_name}}}"));
                } else if portable_mcp_literal_env_entry(key, value) {
                    server.env.insert(key.clone(), value.to_string());
                }
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
            if let Some(env_name) = parse_json_env_reference(flavor, value) {
                server
                    .headers_env
                    .insert(header.clone(), env_name.to_string());
            } else if header.eq_ignore_ascii_case("authorization") {
                server.bearer_token_env_var =
                    parse_json_bearer_env_reference(flavor, value).map(ToString::to_string);
            }
        }
    }
    if server.transport.is_some() {
        Some(server)
    } else {
        None
    }
}

fn mcp_to_claude_value(server: &McpServer) -> Result<Value> {
    mcp_to_json_value(server, JsonMcpFlavor::Claude)
}

fn mcp_to_cursor_value(server: &McpServer) -> Result<Value> {
    mcp_to_json_value(server, JsonMcpFlavor::Cursor)
}

fn mcp_to_json_value(server: &McpServer, flavor: JsonMcpFlavor) -> Result<Value> {
    let mut object = serde_json::Map::new();
    match server.transport {
        Some(McpTransport::Stdio) => {
            object.insert("type".to_string(), json!("stdio"));
            if let Some(command) = &server.command {
                object.insert("command".to_string(), json!(command));
            }
            object.insert("args".to_string(), json!(server.args));
            let env = server
                .env
                .iter()
                .map(|(key, value)| {
                    let value = env_reference(value)
                        .map(|name| render_json_env_reference(flavor, name))
                        .unwrap_or_else(|| value.clone());
                    (key.clone(), Value::String(value))
                })
                .collect::<serde_json::Map<_, _>>();
            object.insert("env".to_string(), Value::Object(env));
        }
        Some(McpTransport::Sse) => {
            object.insert("type".to_string(), json!("sse"));
            if let Some(url) = &server.url {
                object.insert("url".to_string(), json!(url));
            }
        }
        Some(McpTransport::WebSocket) => {
            if matches!(flavor, JsonMcpFlavor::Cursor) {
                bail!("Cursor does not support WebSocket MCP servers");
            }
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
        if matches!(flavor, JsonMcpFlavor::Cursor) {
            bail!("Cursor does not support headersHelper");
        }
        object.insert("headersHelper".to_string(), json!(helper));
    }
    if !server.headers_env.is_empty() {
        let headers: serde_json::Map<String, Value> = server
            .headers_env
            .iter()
            .map(|(header, env_name)| {
                (
                    header.clone(),
                    Value::String(render_json_env_reference(flavor, env_name)),
                )
            })
            .collect();
        object.insert("headers".to_string(), Value::Object(headers));
    }
    if let Some(env_name) = &server.bearer_token_env_var {
        if !valid_env_name(env_name) {
            bail!("bearer token environment variable name is invalid: {env_name:?}");
        }
        let authorization = object
            .entry("headers".to_string())
            .or_insert_with(|| Value::Object(serde_json::Map::new()))
            .as_object_mut()
            .context("MCP headers must be an object")?;
        if authorization
            .keys()
            .any(|header| header.eq_ignore_ascii_case("authorization"))
        {
            bail!("bearer token conflicts with an explicit Authorization header");
        }
        authorization.insert(
            "Authorization".to_string(),
            Value::String(format!(
                "Bearer {}",
                render_json_env_reference(flavor, env_name)
            )),
        );
    }
    Ok(Value::Object(object))
}

fn validate_codex_mcp_table_for_export(name: &str, table: &Table) -> Result<()> {
    reject_unknown_codex_mcp_fields(name, table)?;
    validate_codex_local_timeouts(name, table)?;
    let stdio = validate_codex_transport_for_export(name, table)?;
    validate_codex_args_for_export(name, table, stdio)?;
    if !stdio && (table.contains_key("env") || table.contains_key("env_vars")) {
        bail!("MCP server `{name}` can only use env or env_vars with a stdio command");
    }
    if stdio
        && (table.contains_key("env_http_headers") || table.contains_key("bearer_token_env_var"))
    {
        bail!("MCP server `{name}` can only use HTTP authentication fields with a URL");
    }
    if let Some(item) = table.get("env") {
        let env = item
            .as_table_like()
            .with_context(|| format!("MCP server `{name}` env must be a table"))?;
        for (key, item) in env.iter() {
            if let Some(value) = item.as_str() {
                if env_reference(value).is_some() {
                    bail!(
                        "MCP server `{name}` environment entry `{key}` uses a placeholder in Codex env, which forwards literal values; use env_vars for passthrough"
                    );
                }
                validate_mcp_env_entry(name, key, value)?;
            } else {
                bail!("MCP server `{name}` environment entry `{key}` must use a string value");
            }
        }
    }
    validate_codex_env_vars_for_export(name, table)?;
    if let Some(item) = table.get("bearer_token_env_var") {
        let env_name = item.as_str().with_context(|| {
            format!("MCP server `{name}` bearer_token_env_var must name an environment variable")
        })?;
        require_valid_bearer_env(name, env_name)?;
    }
    if table.contains_key("http_headers") {
        bail!(
            "MCP server `{name}` has literal HTTP headers; use env_http_headers with environment-variable names"
        );
    }
    if let Some(item) = table.get("env_http_headers") {
        let headers = item
            .as_table_like()
            .with_context(|| format!("MCP server `{name}` env_http_headers must be a table"))?;
        for (header, item) in headers.iter() {
            let env_name = item.as_str().with_context(|| {
                format!("HTTP header `{header}` must name an environment variable")
            })?;
            if !valid_env_name(env_name) {
                bail!(
                    "HTTP header `{header}` has unsafe environment reference; expected an environment-variable name"
                );
            }
        }
    }
    Ok(())
}

fn validate_claude_mcp_value_for_export(name: &str, value: &Value) -> Result<()> {
    validate_json_mcp_value_for_export(name, value, JsonMcpFlavor::Claude)
}

fn validate_cursor_mcp_value_for_export(name: &str, value: &Value) -> Result<()> {
    validate_json_mcp_value_for_export(name, value, JsonMcpFlavor::Cursor)
}

fn validate_json_mcp_value_for_export(
    name: &str,
    value: &Value,
    flavor: JsonMcpFlavor,
) -> Result<()> {
    let object = value
        .as_object()
        .with_context(|| format!("MCP server `{name}` must be an object"))?;
    reject_unknown_json_mcp_fields(name, object, flavor)?;
    let stdio = validate_json_transport_for_export(name, object, flavor)?;
    validate_claude_args_for_export(name, object, stdio)?;
    if !stdio && object.contains_key("env") {
        bail!("MCP server `{name}` can only use env with a stdio command");
    }
    if stdio && (object.contains_key("headers") || object.contains_key("headersHelper")) {
        bail!("MCP server `{name}` can only use HTTP headers with a URL");
    }
    validate_json_env_for_export(name, object, flavor)?;
    validate_json_headers_for_export(name, object, flavor)?;
    if let Some(value) = object.get("headersHelper") {
        value
            .as_str()
            .with_context(|| format!("MCP server `{name}` headersHelper must be a string"))?;
    }
    Ok(())
}

fn validate_json_env_for_export(
    name: &str,
    object: &serde_json::Map<String, Value>,
    flavor: JsonMcpFlavor,
) -> Result<()> {
    let Some(value) = object.get("env") else {
        return Ok(());
    };
    let env = value
        .as_object()
        .with_context(|| format!("MCP server `{name}` env must be an object"))?;
    for (key, value) in env {
        let value = value.as_str().with_context(|| {
            format!("MCP server `{name}` environment entry `{key}` must be a string")
        })?;
        let reference = parse_json_env_reference(flavor, value);
        if value.contains("${") && reference.is_none() {
            bail!("MCP server `{name}` environment entry `{key}` uses unsupported interpolation");
        }
        let normalized = reference.map(|env_name| format!("${{{env_name}}}"));
        validate_mcp_env_entry(name, key, normalized.as_deref().unwrap_or(value))?;
    }
    Ok(())
}

fn validate_json_headers_for_export(
    name: &str,
    object: &serde_json::Map<String, Value>,
    flavor: JsonMcpFlavor,
) -> Result<()> {
    let Some(value) = object.get("headers") else {
        return Ok(());
    };
    let (reference_hint, bearer_hint) = match flavor {
        JsonMcpFlavor::Claude => ("${NAME}", "Bearer ${NAME}"),
        JsonMcpFlavor::Cursor => ("${env:NAME}", "Bearer ${env:NAME}"),
    };
    let headers = value
        .as_object()
        .with_context(|| format!("MCP server `{name}` headers must be an object"))?;
    for (header, value) in headers {
        let value = value.as_str().with_context(|| {
            format!("HTTP header `{header}` must use a string environment reference")
        })?;
        let supported = parse_json_env_reference(flavor, value).is_some()
            || (header.eq_ignore_ascii_case("authorization")
                && parse_json_bearer_env_reference(flavor, value).is_some());
        if !supported {
            bail!(
                "MCP server `{name}` HTTP header `{header}` has an unsupported value; use {reference_hint} or Authorization: {bearer_hint}"
            );
        }
    }
    Ok(())
}

const CLAUDE_MCP_FIELDS: &[&str] = &[
    "type",
    "command",
    "args",
    "env",
    "url",
    "headers",
    "headersHelper",
];

const CURSOR_MCP_FIELDS: &[&str] = &["type", "command", "args", "env", "url", "headers"];

const CODEX_MCP_FIELDS: &[&str] = &[
    "command",
    "args",
    "env",
    "env_vars",
    "url",
    "env_http_headers",
    "bearer_token_env_var",
    "startup_timeout_sec",
    "tool_timeout_sec",
];

fn reject_unknown_json_mcp_fields(
    name: &str,
    object: &serde_json::Map<String, Value>,
    flavor: JsonMcpFlavor,
) -> Result<()> {
    let allowed = match flavor {
        JsonMcpFlavor::Claude => CLAUDE_MCP_FIELDS,
        JsonMcpFlavor::Cursor => CURSOR_MCP_FIELDS,
    };
    if let Some(field) = object
        .keys()
        .find(|field| !allowed.contains(&field.as_str()))
    {
        bail!("MCP server `{name}` contains unsupported field `{field}`");
    }
    Ok(())
}

fn reject_unknown_codex_mcp_fields(name: &str, table: &Table) -> Result<()> {
    if let Some(field) = table
        .iter()
        .map(|(field, _)| field)
        .find(|field| !CODEX_MCP_FIELDS.contains(field))
    {
        bail!("MCP server `{name}` contains unsupported field `{field}`");
    }
    Ok(())
}

fn validate_codex_local_timeouts(name: &str, table: &Table) -> Result<()> {
    for field in ["startup_timeout_sec", "tool_timeout_sec"] {
        let Some(item) = table.get(field) else {
            continue;
        };
        let positive = item.as_integer().is_some_and(|value| value > 0)
            || item
                .as_float()
                .is_some_and(|value| value.is_finite() && value > 0.0);
        if !positive {
            bail!("MCP server `{name}` {field} must be a positive number");
        }
    }
    Ok(())
}

fn validate_json_transport_for_export(
    name: &str,
    object: &serde_json::Map<String, Value>,
    flavor: JsonMcpFlavor,
) -> Result<bool> {
    let declared = optional_json_string(name, object, "type")?;
    let command = optional_json_string(name, object, "command")?;
    let url = optional_json_string(name, object, "url")?;
    match (declared, command.is_some(), url.is_some()) {
        (Some("stdio"), true, false) | (None, true, false) => Ok(true),
        (Some("http" | "streamable-http" | "sse"), false, true) | (None, false, true) => Ok(false),
        (Some("ws" | "websocket"), false, true) if matches!(flavor, JsonMcpFlavor::Claude) => {
            Ok(false)
        }
        (Some("stdio"), _, _) => {
            bail!("MCP server `{name}` type stdio requires command and forbids url")
        }
        (Some("http" | "streamable-http" | "sse"), _, _) => {
            bail!("MCP server `{name}` remote type requires url and forbids command")
        }
        (Some(other), _, _) => bail!("MCP server `{name}` has unsupported type `{other}`"),
        (None, _, _) => bail!("MCP server `{name}` must define exactly one of command or url"),
    }
}

fn optional_json_string<'a>(
    name: &str,
    object: &'a serde_json::Map<String, Value>,
    field: &str,
) -> Result<Option<&'a str>> {
    object
        .get(field)
        .map(|value| {
            value
                .as_str()
                .with_context(|| format!("MCP server `{name}` {field} must be a string"))
        })
        .transpose()
}

fn validate_claude_args_for_export(
    name: &str,
    object: &serde_json::Map<String, Value>,
    stdio: bool,
) -> Result<()> {
    let Some(value) = object.get("args") else {
        return Ok(());
    };
    if !stdio {
        bail!("MCP server `{name}` can only use args with a stdio command");
    }
    let arguments = value
        .as_array()
        .with_context(|| format!("MCP server `{name}` args must be an array"))?;
    if arguments.iter().any(|argument| !argument.is_string()) {
        bail!("MCP server `{name}` args must contain only strings");
    }
    Ok(())
}

fn validate_codex_transport_for_export(name: &str, table: &Table) -> Result<bool> {
    let command = optional_toml_string(name, table, "command")?;
    let url = optional_toml_string(name, table, "url")?;
    match (command.is_some(), url.is_some()) {
        (true, false) => Ok(true),
        (false, true) => Ok(false),
        _ => bail!("MCP server `{name}` must define exactly one of command or url"),
    }
}

fn optional_toml_string<'a>(name: &str, table: &'a Table, field: &str) -> Result<Option<&'a str>> {
    table
        .get(field)
        .map(|item| {
            item.as_str()
                .with_context(|| format!("MCP server `{name}` {field} must be a string"))
        })
        .transpose()
}

fn validate_codex_args_for_export(name: &str, table: &Table, stdio: bool) -> Result<()> {
    let Some(item) = table.get("args") else {
        return Ok(());
    };
    if !stdio {
        bail!("MCP server `{name}` can only use args with a stdio command");
    }
    let arguments = item
        .as_array()
        .with_context(|| format!("MCP server `{name}` args must be an array"))?;
    if arguments.iter().any(|argument| argument.as_str().is_none()) {
        bail!("MCP server `{name}` args must contain only strings");
    }
    Ok(())
}

fn validate_mcp_server_for_export(name: &str, server: &McpServer) -> Result<()> {
    if let Some(command) = &server.command {
        validate_mcp_text_field(name, "command", command)?;
    }
    validate_mcp_arguments(name, &server.args)?;
    if let Some(url) = &server.url {
        validate_mcp_url(name, url)?;
    }
    for (key, value) in &server.env {
        validate_mcp_env_entry(name, key, value)?;
    }
    for (header, env_name) in &server.headers_env {
        if !valid_env_name(env_name) {
            bail!(
                "MCP server `{name}` HTTP header `{header}` has unsafe environment reference; expected an environment-variable name"
            );
        }
    }
    if let Some(env_name) = &server.bearer_token_env_var {
        require_valid_bearer_env(name, env_name)?;
        if server
            .headers_env
            .keys()
            .any(|header| header.eq_ignore_ascii_case("authorization"))
        {
            bail!(
                "MCP server `{name}` configures both bearer_token_env_var and an Authorization header"
            );
        }
    }
    if let Some(helper) = &server.headers_helper {
        validate_mcp_text_field(name, "headersHelper", helper)?;
        let arguments = helper
            .split_whitespace()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        validate_mcp_arguments(name, &arguments)
            .context("headersHelper contains unsafe credential material")?;
    }
    Ok(())
}

fn validate_mcp_servers_for_render(servers: &BTreeMap<String, McpServer>) -> Result<()> {
    for (name, server) in servers {
        validate_mcp_server_for_export(name, server)
            .with_context(|| format!("render MCP server `{name}`"))?;
    }
    Ok(())
}

fn validate_mcp_env_entry(name: &str, key: &str, value: &str) -> Result<()> {
    if let Some(reference) = env_reference(value) {
        if reference != key {
            bail!(
                "MCP server `{name}` environment entry `{key}` references `{reference}`; portable passthrough requires the same variable name"
            );
        }
    } else if value.contains("${") {
        bail!(
            "MCP server `{name}` environment entry `{key}` uses an unsupported environment template"
        );
    }
    if sensitive_key(key) && env_reference(value).is_none() {
        bail!(
            "MCP server `{name}` environment entry `{key}` contains a literal credential; use an environment reference"
        );
    }
    if let Some(reason) = raw_secret_reason(value) {
        bail!("MCP server `{name}` environment entry `{key}` contains {reason}");
    }
    Ok(())
}

fn validate_mcp_text_field(name: &str, field: &str, value: &str) -> Result<()> {
    reject_unsupported_template(name, field, value)?;
    if let Some(reason) = raw_secret_reason(value) {
        bail!("MCP server `{name}` {field} contains {reason}");
    }
    Ok(())
}

fn validate_mcp_url(name: &str, url: &str) -> Result<()> {
    reject_unsupported_template(name, "URL", url)?;
    if let Some(reason) = raw_secret_reason(url) {
        bail!("MCP server `{name}` URL contains {reason}");
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum PendingArgument {
    Secret { flag_index: usize },
    Header { flag_index: usize },
}

fn validate_mcp_arguments(name: &str, arguments: &[String]) -> Result<()> {
    let mut pending = None;
    for (index, argument) in arguments.iter().enumerate() {
        reject_unsupported_template(name, &format!("argument {}", index + 1), argument)?;
        if let Some(expected) = pending.take() {
            validate_pending_argument(name, index, argument, expected)?;
            continue;
        }
        pending = validate_mcp_argument(name, index, argument)?;
    }
    if let Some(expected) = pending {
        reject_missing_argument_value(name, expected)?;
    }
    Ok(())
}

fn reject_unsupported_template(name: &str, field: &str, value: &str) -> Result<()> {
    if value.contains("${") {
        bail!(
            "MCP server `{name}` {field} uses unsupported environment template expansion; use env_vars, env_http_headers, or bearer_token_env_var"
        );
    }
    Ok(())
}

fn validate_pending_argument(
    name: &str,
    index: usize,
    argument: &str,
    pending: PendingArgument,
) -> Result<()> {
    match pending {
        PendingArgument::Secret { flag_index } => {
            if !safe_secret_reference_or_placeholder(argument) {
                bail!(
                    "MCP server `{name}` argument {} contains a literal credential after sensitive argument {}",
                    index + 1,
                    flag_index + 1
                );
            }
            Ok(())
        }
        PendingArgument::Header { flag_index } => validate_mcp_header_argument(name, argument)
            .with_context(|| format!("header value after argument {}", flag_index + 1)),
    }
}

fn validate_mcp_argument(
    name: &str,
    index: usize,
    argument: &str,
) -> Result<Option<PendingArgument>> {
    if is_header_flag(argument) {
        return Ok(Some(PendingArgument::Header { flag_index: index }));
    }
    if is_sensitive_flag(argument) {
        return Ok(Some(PendingArgument::Secret { flag_index: index }));
    }
    if let Some((flag, value)) = argument.split_once('=') {
        if validate_inline_mcp_argument(name, index, flag, value)? {
            return Ok(None);
        }
    }
    validate_standalone_mcp_argument(name, index, argument)?;
    Ok(None)
}

fn validate_inline_mcp_argument(name: &str, index: usize, flag: &str, value: &str) -> Result<bool> {
    if is_header_flag(flag) {
        validate_mcp_header_argument(name, value)
            .with_context(|| format!("argument {}", index + 1))?;
        return Ok(true);
    }
    if !is_sensitive_flag(flag) {
        return Ok(false);
    }
    if !safe_secret_reference_or_placeholder(value) {
        bail!(
            "MCP server `{name}` argument {} contains a literal credential",
            index + 1
        );
    }
    Ok(true)
}

fn validate_standalone_mcp_argument(name: &str, index: usize, argument: &str) -> Result<()> {
    if argument.contains("://") {
        return validate_mcp_url(name, argument).with_context(|| format!("argument {}", index + 1));
    }
    validate_mcp_text_field(name, &format!("argument {}", index + 1), argument)
}

fn reject_missing_argument_value(name: &str, pending: PendingArgument) -> Result<()> {
    match pending {
        PendingArgument::Secret { flag_index } => bail!(
            "MCP server `{name}` sensitive argument {} has no value",
            flag_index + 1
        ),
        PendingArgument::Header { flag_index } => bail!(
            "MCP server `{name}` header argument {} has no value",
            flag_index + 1
        ),
    }
}

fn validate_mcp_header_argument(name: &str, value: &str) -> Result<()> {
    if let Some((header, value)) = value.split_once(':').or_else(|| value.split_once('=')) {
        if sensitive_key(header) && !safe_secret_reference_or_placeholder(value) {
            bail!("MCP server `{name}` header argument `{header}` contains a literal credential");
        }
    }
    validate_mcp_text_field(name, "header argument", value)
}

fn is_header_flag(value: &str) -> bool {
    matches!(value, "-H" | "--header")
}

fn is_sensitive_flag(value: &str) -> bool {
    let value = value.trim_start_matches('-');
    !value.is_empty() && sensitive_key(value)
}

pub fn secret_like_key(key: &str) -> bool {
    sensitive_key(key)
}

fn portable_mcp_literal_env_entry(key: &str, value: &str) -> bool {
    !value.contains("${") && !secret_like_key(key)
}

fn portable_codex_literal_env_entry(key: &str, value: &str) -> bool {
    !secret_like_key(key) && env_reference(value).is_none()
}

fn supported_codex_env_vars(table: &Table) -> Vec<String> {
    table
        .get("env_vars")
        .and_then(Item::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            if let Some(name) = item.as_str().filter(|name| valid_env_name(name)) {
                return Some(name.to_string());
            }
            let inline = item.as_inline_table()?;
            let name = inline.get("name")?.as_str()?;
            let source = inline.get("source").and_then(|value| value.as_str());
            (valid_env_name(name) && source.is_none_or(|source| source == "local"))
                .then(|| name.to_string())
        })
        .collect()
}

fn validate_codex_env_vars_for_export(name: &str, table: &Table) -> Result<()> {
    let Some(item) = table.get("env_vars") else {
        return Ok(());
    };
    let variables = item
        .as_array()
        .with_context(|| format!("MCP server `{name}` env_vars must be an array"))?;
    let mut seen = BTreeSet::new();
    for variable in variables {
        let (env_name, source) = if let Some(env_name) = variable.as_str() {
            (env_name, "local")
        } else {
            let inline = variable.as_inline_table().with_context(|| {
                format!("MCP server `{name}` env_vars entries must be strings or tables")
            })?;
            if inline
                .iter()
                .any(|(key, _)| !matches!(key, "name" | "source"))
            {
                bail!("MCP server `{name}` env_vars table contains unsupported fields");
            }
            let env_name = inline
                .get("name")
                .and_then(|value| value.as_str())
                .with_context(|| {
                    format!("MCP server `{name}` env_vars table has no string name")
                })?;
            let source = match inline.get("source") {
                Some(value) => value.as_str().with_context(|| {
                    format!("MCP server `{name}` env_vars source must be a string")
                })?,
                None => "local",
            };
            (env_name, source)
        };
        if !valid_env_name(env_name) {
            bail!("MCP server `{name}` env_vars contains invalid name {env_name:?}");
        }
        if source != "local" {
            bail!(
                "MCP server `{name}` env_vars entry `{env_name}` uses unsupported source {source:?}"
            );
        }
        if !seen.insert(env_name) {
            bail!("MCP server `{name}` env_vars repeats `{env_name}`");
        }
        if table
            .get("env")
            .and_then(Item::as_table_like)
            .is_some_and(|env| env.contains_key(env_name))
        {
            bail!("MCP server `{name}` defines `{env_name}` in both env and env_vars");
        }
    }
    Ok(())
}

fn require_valid_bearer_env(name: &str, env_name: &str) -> Result<()> {
    if !valid_env_name(env_name) {
        bail!("MCP server `{name}` bearer token environment variable is invalid: {env_name:?}");
    }
    Ok(())
}

fn parse_json_env_reference(flavor: JsonMcpFlavor, value: &str) -> Option<&str> {
    match flavor {
        JsonMcpFlavor::Claude => env_reference(value),
        JsonMcpFlavor::Cursor => value
            .trim()
            .strip_prefix("${env:")
            .and_then(|value| value.strip_suffix('}'))
            .filter(|value| valid_env_name(value)),
    }
}

fn parse_json_bearer_env_reference(flavor: JsonMcpFlavor, value: &str) -> Option<&str> {
    let mut parts = value.split_whitespace();
    let scheme = parts.next()?;
    let reference = parts.next()?;
    if !scheme.eq_ignore_ascii_case("bearer") || parts.next().is_some() {
        return None;
    }
    parse_json_env_reference(flavor, reference)
}

fn render_json_env_reference(flavor: JsonMcpFlavor, env_name: &str) -> String {
    match flavor {
        JsonMcpFlavor::Claude => format!("${{{env_name}}}"),
        JsonMcpFlavor::Cursor => format!("${{env:{env_name}}}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_claude_and_cursor_round_trip_environment_and_bearer_references() {
        let temp = tempfile::tempdir().unwrap();
        let codex_path = temp.path().join("config.toml");
        fs::write(
            &codex_path,
            concat!(
                "[mcp_servers.stdio]\n",
                "command = \"safe-server\"\n",
                "env_vars = [\"API_TOKEN\"]\n",
                "[mcp_servers.stdio.env]\n",
                "MODE = \"portable\"\n",
                "[mcp_servers.http]\n",
                "url = \"https://example.invalid/mcp\"\n",
                "bearer_token_env_var = \"MCP_BEARER_TOKEN\"\n",
                "[mcp_servers.http.env_http_headers]\n",
                "X-Trace = \"TRACE_TOKEN\"\n",
            ),
        )
        .unwrap();

        let selected = ["stdio".to_string(), "http".to_string()];
        let from_codex = discover_codex_mcp_for_export(&codex_path, &selected).unwrap();
        assert_eq!(from_codex["stdio"].env["API_TOKEN"], "${API_TOKEN}");
        assert_eq!(from_codex["stdio"].env["MODE"], "portable");
        assert_eq!(
            from_codex["http"].bearer_token_env_var.as_deref(),
            Some("MCP_BEARER_TOKEN")
        );

        let claude_path = temp.path().join("claude.json");
        let claude = write_claude_mcp(&claude_path, &from_codex).unwrap();
        let claude_json: Value = serde_json::from_slice(&claude).unwrap();
        assert_eq!(
            claude_json["mcpServers"]["stdio"]["env"]["API_TOKEN"],
            "${API_TOKEN}"
        );
        fs::write(&claude_path, claude).unwrap();
        let from_claude = discover_claude_mcp_for_export(&claude_path, &selected).unwrap();
        assert_eq!(from_claude, from_codex);

        let cursor_path = temp.path().join("cursor.json");
        let cursor = render_cursor_mcp_additive_with_updates(
            &cursor_path,
            None,
            &from_claude,
            &BTreeSet::new(),
        )
        .unwrap();
        let cursor_json: Value = serde_json::from_slice(&cursor).unwrap();
        assert_eq!(
            cursor_json["mcpServers"]["stdio"]["env"]["API_TOKEN"],
            "${env:API_TOKEN}"
        );
        assert_eq!(
            cursor_json["mcpServers"]["http"]["headers"]["Authorization"],
            "Bearer ${env:MCP_BEARER_TOKEN}"
        );
        assert_eq!(
            cursor_json["mcpServers"]["http"]["headers"]["X-Trace"],
            "${env:TRACE_TOKEN}"
        );
        fs::write(&cursor_path, cursor).unwrap();
        let from_cursor = discover_cursor_mcp_for_export(&cursor_path, &selected).unwrap();
        assert_eq!(from_cursor, from_codex);

        let claude_from_cursor =
            write_claude_mcp(&temp.path().join("claude-from-cursor.json"), &from_cursor).unwrap();
        let claude_from_cursor: Value = serde_json::from_slice(&claude_from_cursor).unwrap();
        assert_eq!(
            claude_from_cursor["mcpServers"]["http"]["headers"]["Authorization"],
            "Bearer ${MCP_BEARER_TOKEN}"
        );

        let round_trip = String::from_utf8(write_codex_mcp(&codex_path, &from_cursor).unwrap())
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(
            round_trip["mcp_servers"]["stdio"]["env"]["MODE"].as_str(),
            Some("portable")
        );
        assert_eq!(
            round_trip["mcp_servers"]["stdio"]["env_vars"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|value| value.as_str())
                .collect::<Vec<_>>(),
            vec!["API_TOKEN"]
        );
        assert_eq!(
            round_trip["mcp_servers"]["http"]["bearer_token_env_var"].as_str(),
            Some("MCP_BEARER_TOKEN")
        );
    }

    #[test]
    fn codex_literal_env_placeholders_and_renamed_references_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let codex_path = temp.path().join("config.toml");
        fs::write(
            &codex_path,
            concat!(
                "[mcp_servers.unsafe]\n",
                "command = \"unsafe-server\"\n",
                "[mcp_servers.unsafe.env]\n",
                "API_TOKEN = \"${SERVICE_TOKEN}\"\n",
            ),
        )
        .unwrap();
        let error =
            discover_codex_mcp_for_export(&codex_path, &["unsafe".to_string()]).unwrap_err();
        assert!(format!("{error:#}").contains("Codex env, which forwards literal values"));

        let renamed = BTreeMap::from([(
            "unsafe".to_string(),
            McpServer {
                transport: Some(McpTransport::Stdio),
                command: Some("unsafe-server".to_string()),
                env: BTreeMap::from([("API_TOKEN".to_string(), "${SERVICE_TOKEN}".to_string())]),
                ..McpServer::default()
            },
        )]);
        let error = write_codex_mcp(&temp.path().join("target.toml"), &renamed).unwrap_err();
        assert!(format!("{error:#}").contains("requires the same variable name"));
    }

    #[test]
    fn unsupported_safe_claude_environment_and_header_forms_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("claude.json");
        for raw in [
            r#"{"mcpServers":{"unsafe":{"command":"server","env":{"PORT":3000}}}}"#,
            r#"{"mcpServers":{"unsafe":{"command":"server","env":{"PORT":"${PORT:-3000}"}}}}"#,
            r#"{"mcpServers":{"unsafe":{"command":"server","args":["--root","${HOME}/bin"]}}}"#,
            r#"{"mcpServers":{"unsafe":{"url":"${MCP_URL}"}}}"#,
            r#"{"mcpServers":{"unsafe":{"url":"https://example.invalid","headers":{"X-Trace":"prefix-${TRACE_ID}"}}}}"#,
        ] {
            fs::write(&path, raw).unwrap();
            let error = discover_claude_mcp_for_export(&path, &["unsafe".to_string()]).unwrap_err();
            assert!(format!("{error:#}").contains("MCP server `unsafe`"));
        }
    }

    #[test]
    fn selected_json_mcp_servers_reject_unmapped_or_malformed_fields() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("mcp.json");
        for raw in [
            r#"{"mcpServers":{"unsafe":{"command":"server","alwaysLoad":true}}}"#,
            r#"{"mcpServers":{"unsafe":{"url":"https://example.invalid","oauth":{}}}}"#,
            r#"{"mcpServers":{"unsafe":{"type":"custom","url":"https://example.invalid"}}}"#,
            r#"{"mcpServers":{"unsafe":{"command":"server","args":[1]}}}"#,
        ] {
            fs::write(&path, raw).unwrap();
            let error = discover_claude_mcp_for_export(&path, &["unsafe".to_string()]).unwrap_err();
            assert!(format!("{error:#}").contains("MCP server `unsafe`"));
        }

        fs::write(
            &path,
            r#"{"mcpServers":{"safe":{"type":"streamable-http","url":"https://example.invalid"}}}"#,
        )
        .unwrap();
        assert!(discover_claude_mcp_for_export(&path, &["safe".to_string()]).is_ok());
    }

    #[test]
    fn selected_cursor_servers_reject_nonportable_interpolation_and_fields() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("mcp.json");
        for raw in [
            r#"{"mcpServers":{"unsafe":{"command":"server","env":{"HOME_DIR":"${userHome}"}}}}"#,
            r#"{"mcpServers":{"unsafe":{"command":"server","args":["${workspaceFolder}"]}}}"#,
            r#"{"mcpServers":{"unsafe":{"command":"server","env":{"TOKEN":"${TOKEN}"}}}}"#,
            r#"{"mcpServers":{"unsafe":{"command":"server","envFile":".env"}}}"#,
            r#"{"mcpServers":{"unsafe":{"url":"https://example.invalid","auth":{}}}}"#,
            r#"{"mcpServers":{"unsafe":{"url":"https://example.invalid","headersHelper":"helper"}}}"#,
        ] {
            fs::write(&path, raw).unwrap();
            let error = discover_cursor_mcp_for_export(&path, &["unsafe".to_string()]).unwrap_err();
            assert!(format!("{error:#}").contains("MCP server `unsafe`"));
        }
    }

    #[test]
    fn selected_codex_servers_reject_unmapped_fields_and_malformed_args() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        for field in [
            "enabled = false",
            "required = true",
            "cwd = \"/tmp\"",
            "enabled_tools = [\"read\"]",
            "args = [1]",
            "startup_timeout_sec = 0",
            "tool_timeout_sec = \"slow\"",
        ] {
            fs::write(
                &path,
                format!("[mcp_servers.unsafe]\ncommand = \"server\"\n{field}\n"),
            )
            .unwrap();
            let error = discover_codex_mcp_for_export(&path, &["unsafe".to_string()]).unwrap_err();
            assert!(format!("{error:#}").contains("MCP server `unsafe`"));
        }

        fs::write(
            &path,
            "[mcp_servers.safe]\nurl = \"https://example.invalid\"\nstartup_timeout_sec = 30\ntool_timeout_sec = 60\n",
        )
        .unwrap();
        assert!(discover_codex_mcp_for_export(&path, &["safe".to_string()]).is_ok());
    }

    #[test]
    fn codex_render_rejects_nonportable_json_only_features() {
        let temp = tempfile::tempdir().unwrap();
        for (transport, helper) in [
            (McpTransport::Sse, None),
            (McpTransport::WebSocket, None),
            (McpTransport::Http, Some("headers-helper".to_string())),
        ] {
            let servers = BTreeMap::from([(
                "unsupported".to_string(),
                McpServer {
                    transport: Some(transport),
                    url: Some("https://example.invalid".to_string()),
                    headers_helper: helper,
                    ..McpServer::default()
                },
            )]);
            assert!(write_codex_mcp(&temp.path().join("config.toml"), &servers).is_err());
        }
    }

    #[test]
    fn cursor_export_discovery_returns_only_selected_safe_servers() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("mcp.json");
        fs::write(
            &path,
            r#"{
  "mcpServers": {
    "safe": {"command": "/usr/local/bin/safe", "args": ["serve"]},
    "unsafe": {
      "command": "/usr/local/bin/unsafe",
      "args": ["--api-key", "literal-secret-value"]
    }
  }
}
"#,
        )
        .unwrap();

        let safe = discover_cursor_mcp_for_export(&path, &["safe".to_string()]).unwrap();
        assert_eq!(safe.keys().cloned().collect::<Vec<_>>(), vec!["safe"]);

        let error = discover_cursor_mcp_for_export(&path, &["unsafe".to_string()]).unwrap_err();
        let error = format!("{error:#}");
        assert!(error.contains("MCP server `unsafe`"), "{error}");
        assert!(error.contains("literal credential"), "{error}");
    }

    #[test]
    fn sensitive_arguments_reject_literals_and_unportable_expansion() {
        for literal in [
            "ghp_12345678901234567890",
            "github_pat_12345678901234567890",
            "npm_12345678901234567890",
            "PRODUCTION_SECRET_VALUE_12345",
        ] {
            let arguments = vec!["--token".to_string(), literal.to_string()];
            let error = validate_mcp_arguments("unsafe", &arguments).unwrap_err();
            assert!(format!("{error:#}").contains("literal credential"));
        }

        let explicit = vec!["--token".to_string(), "${SERVICE_TOKEN}".to_string()];
        let error = validate_mcp_arguments("unsafe", &explicit).unwrap_err();
        assert!(format!("{error:#}").contains("unsupported environment template"));
    }

    #[test]
    fn mcp_text_scan_allows_source_type_declarations() {
        validate_mcp_text_field("safe", "command", "token: Option<String>,").unwrap();
        validate_mcp_text_field(
            "safe",
            "command",
            "Token: authentication token used by the service",
        )
        .unwrap();
    }

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

        let updated = String::from_utf8(
            render_cursor_mcp_additive_with_updates(
                &path,
                Some(original.as_bytes()),
                &servers,
                &BTreeSet::new(),
            )
            .unwrap(),
        )
        .unwrap();
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

    #[test]
    fn cursor_mcp_managed_update_preserves_unmanaged_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("mcp.json");
        let original = concat!(
            "{\n",
            "  \"cursorSetting\": \"\\u0061\",\n",
            "  \"mcpServers\": {\n",
            "    \"managed\": { \"command\": \"old\", \"args\": [\"serve\"] },\n",
            "    \"cursorOwned\": {\"command\":\"keep\",\"cursorOnly\":1.2300e+02}\n",
            "  }\n",
            "}\n",
        );
        fs::write(&path, original).unwrap();
        let servers = BTreeMap::from([(
            "managed".to_string(),
            McpServer {
                transport: Some(McpTransport::Stdio),
                command: Some("new".to_string()),
                args: vec!["serve".to_string()],
                ..McpServer::default()
            },
        )]);

        let updated = String::from_utf8(
            render_cursor_mcp_additive_with_updates(
                &path,
                Some(original.as_bytes()),
                &servers,
                &BTreeSet::from(["managed".to_string()]),
            )
            .unwrap(),
        )
        .unwrap();

        assert!(
            updated.contains("\"cursorOwned\": {\"command\":\"keep\",\"cursorOnly\":1.2300e+02}")
        );
        assert!(updated.contains("\"cursorSetting\": \"\\u0061\""));
        let parsed = serde_json::from_str::<Value>(&updated).unwrap();
        assert_eq!(parsed["mcpServers"]["managed"]["command"], "new");
        assert_eq!(
            parsed["mcpServers"]["managed"]["args"],
            serde_json::json!(["serve"])
        );
    }
}
