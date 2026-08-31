use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    ops::Range,
    path::{Path, PathBuf},
    str,
    sync::atomic::{AtomicU64, Ordering},
};

#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::{ffi::CString, os::unix::ffi::OsStrExt};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn ensure_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("create directory {}", path.display()))
}

pub fn write_atomic(path: &Path, content: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        ensure_dir(parent)?;
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("agent-sync");
    let (tmp, mut file) = loop {
        let sequence = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(
            ".{name}.agent-sync-{}-{sequence}.tmp",
            std::process::id()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => break (candidate, file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("create temp file {}", candidate.display()));
            }
        }
    };
    let existing_permissions = fs::metadata(path)
        .ok()
        .map(|metadata| metadata.permissions());
    let write_result = (|| -> Result<()> {
        if let Some(permissions) = existing_permissions {
            fs::set_permissions(&tmp, permissions)
                .with_context(|| format!("preserve permissions for {}", path.display()))?;
        }
        file.write_all(content)
            .with_context(|| format!("write temp file {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("sync temp file {}", tmp.display()))?;
        fs::rename(&tmp, path)
            .with_context(|| format!("rename {} to {}", tmp.display(), path.display()))?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    write_result
}

pub fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    copy_dir_with_export_policy(src, dst, None)
}

pub fn copy_dir_for_export(src: &Path, dst: &Path, resource: &str) -> Result<()> {
    copy_dir_with_export_policy(src, dst, Some(resource))
}

fn copy_dir_with_export_policy(
    src: &Path,
    dst: &Path,
    export_resource: Option<&str>,
) -> Result<()> {
    let src = resolve_root_dir(src)?;
    if let Some(resource) = export_resource {
        validate_export_tree(&src, resource)?;
    }
    ensure_dir(dst)?;
    for entry in WalkDir::new(&src)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| !should_ignore(entry.path()))
    {
        let entry = entry?;
        let path = entry.path();
        let rel = path.strip_prefix(&src)?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        let target = dst.join(rel);
        let file_type = entry.file_type();
        if let Some(resource) = export_resource {
            validate_export_entry(path, rel, file_type, resource)?;
        }
        if file_type.is_dir() {
            ensure_dir(&target)?;
        } else if file_type.is_file() {
            if let Some(parent) = target.parent() {
                ensure_dir(parent)?;
            }
            fs::copy(path, &target)
                .with_context(|| format!("copy {} to {}", path.display(), target.display()))?;
        } else if file_type.is_symlink() {
            if let Some(resource) = export_resource {
                bail!(
                    "refusing to export {resource}: symlinked content is not portable ({})",
                    path.display()
                );
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs as unix_fs;
                if let Some(parent) = target.parent() {
                    ensure_dir(parent)?;
                }
                let link_target = fs::read_link(path)
                    .with_context(|| format!("read symlink {}", path.display()))?;
                unix_fs::symlink(link_target, &target)
                    .with_context(|| format!("create symlink {}", target.display()))?;
            }
        }
    }
    Ok(())
}

fn validate_export_tree(src: &Path, resource: &str) -> Result<()> {
    for entry in WalkDir::new(src)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| !should_ignore(entry.path()))
    {
        let entry = entry?;
        let path = entry.path();
        let relative = path.strip_prefix(src)?;
        if relative.as_os_str().is_empty() {
            continue;
        }
        validate_export_entry(path, relative, entry.file_type(), resource)?;
    }
    Ok(())
}

pub fn copy_file_for_export(src: &Path, dst: &Path, resource: &str) -> Result<u64> {
    let metadata = fs::symlink_metadata(src)
        .with_context(|| format!("inspect export source {}", src.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!(
            "refusing to export {resource}: source is not a regular file ({})",
            src.display()
        );
    }
    let name = src
        .file_name()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("resource"));
    validate_export_entry(src, &name, metadata.file_type(), resource)?;
    fs::copy(src, dst).with_context(|| format!("copy {} to {}", src.display(), dst.display()))
}

fn validate_export_entry(
    path: &Path,
    relative: &Path,
    file_type: fs::FileType,
    resource: &str,
) -> Result<()> {
    if sensitive_export_path(relative) {
        bail!(
            "refusing to export {resource}: sensitive path {}",
            path.display()
        );
    }
    if file_type.is_symlink() {
        bail!(
            "refusing to export {resource}: symlinked content is not portable ({})",
            path.display()
        );
    }
    if !file_type.is_file() && !file_type.is_dir() {
        bail!(
            "refusing to export {resource}: unsupported file type ({})",
            path.display()
        );
    }
    if !file_type.is_file() {
        return Ok(());
    }
    let content =
        fs::read(path).with_context(|| format!("inspect export file {}", path.display()))?;
    if likely_binary_credential_container(relative, &content) {
        bail!(
            "refusing to export {resource}: likely binary credential container {}",
            path.display()
        );
    }
    let Ok(text) = str::from_utf8(&content) else {
        return Ok(());
    };
    if let Some((line, reason)) = raw_secret_in_text(text) {
        bail!(
            "refusing to export {resource}: {reason} at {}:{}",
            path.display(),
            line
        );
    }
    Ok(())
}

fn sensitive_export_path(path: &Path) -> bool {
    path.components().any(|component| {
        let name = component.as_os_str().to_string_lossy().to_ascii_lowercase();
        matches!(
            name.as_str(),
            ".aws"
                | ".gnupg"
                | ".netrc"
                | ".npmrc"
                | ".pypirc"
                | ".ssh"
                | "credentials"
                | "credentials.json"
                | "id_dsa"
                | "id_ed25519"
                | "id_ecdsa"
                | "id_rsa"
                | "secrets"
                | "secrets.json"
        ) || (name.starts_with(".env")
            && !matches!(
                name.as_str(),
                ".env.example" | ".env.sample" | ".env.template"
            ))
            || name.ends_with(".key")
            || name.ends_with(".p12")
            || name.ends_with(".pfx")
            || name.ends_with(".pem")
    })
}

const BINARY_CREDENTIAL_EXTENSIONS: &[&str] = &[
    "der",
    "jceks",
    "jks",
    "kdb",
    "kdbx",
    "keystore",
    "keychain",
    "keychain-db",
    "pkcs12",
];

const BINARY_CREDENTIAL_NAME_MARKERS: &[&str] = &[
    "credential",
    "keychain",
    "keystore",
    "password-store",
    "secret-store",
    "token-store",
];

const BINARY_CREDENTIAL_MAGIC: &[&[u8]] = &[
    // KeePass KDBX and Java KeyStore containers.
    &[0x03, 0xd9, 0xa2, 0x9a, 0x67, 0xfb, 0x4b, 0xb5],
    &[0xfe, 0xed, 0xfe, 0xed],
    &[0xce, 0xce, 0xce, 0xce],
];

fn likely_binary_credential_container(path: &Path, content: &[u8]) -> bool {
    let extension_is_sensitive = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            BINARY_CREDENTIAL_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str())
        });
    if extension_is_sensitive {
        return true;
    }
    if BINARY_CREDENTIAL_MAGIC
        .iter()
        .any(|magic| content.starts_with(magic))
    {
        return true;
    }
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let name = name.to_ascii_lowercase();
    BINARY_CREDENTIAL_NAME_MARKERS
        .iter()
        .any(|marker| name.contains(marker))
        && str::from_utf8(content).is_err()
}

const SENSITIVE_KEY_PARTS: &[&str] = &[
    "AUTH",
    "AUTHORIZATION",
    "BEARER",
    "COOKIE",
    "CREDENTIAL",
    "CREDENTIALS",
    "PASSCODE",
    "PASSPHRASE",
    "PASSWORD",
    "PASSWD",
    "SECRET",
];

const SENSITIVE_KEY_PHRASES: &[&str] = &["API_KEY", "ACCESS_KEY", "PRIVATE_KEY"];
const SENSITIVE_COMPACT_PARTS: &[&str] = &["APIKEY", "ACCESSKEY", "PRIVATEKEY"];
const SENSITIVE_COMPACT_SUFFIXES: &[&str] = &[
    "APITOKEN",
    "AUTHTOKEN",
    "ACCESSTOKEN",
    "REFRESHTOKEN",
    "SECRETKEY",
];

pub(crate) fn sensitive_key(key: &str) -> bool {
    let normalized = normalize_sensitive_key(key);
    let compact = normalized.replace('_', "");
    sensitive_key_part(&normalized)
        || sensitive_token_key(&normalized, &compact)
        || contains_any(&normalized, SENSITIVE_KEY_PHRASES)
        || contains_any(&compact, SENSITIVE_COMPACT_PARTS)
        || sensitive_key_suffix(&normalized)
}

fn sensitive_token_key(normalized: &str, compact: &str) -> bool {
    normalized == "TOKEN"
        || (normalized.ends_with("_TOKEN") && !normalized.ends_with("_PER_TOKEN"))
        || ends_with_any(compact, SENSITIVE_COMPACT_SUFFIXES)
}

fn normalize_sensitive_key(key: &str) -> String {
    key.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

fn sensitive_key_part(normalized: &str) -> bool {
    normalized
        .split('_')
        .any(|part| SENSITIVE_KEY_PARTS.contains(&part))
}

fn contains_any(value: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| value.contains(needle))
}

fn ends_with_any(value: &str, suffixes: &[&str]) -> bool {
    suffixes.iter().any(|suffix| value.ends_with(suffix))
}

fn sensitive_key_suffix(normalized: &str) -> bool {
    let Some(prefix) = normalized.strip_suffix("_KEY") else {
        return false;
    };
    prefix.split('_').any(|part| {
        matches!(
            part,
            "AUTH" | "DECRYPTION" | "ENCRYPTION" | "MASTER" | "PRIVATE" | "SESSION" | "SIGNING"
        )
    })
}

pub(crate) fn env_reference(value: &str) -> Option<&str> {
    value
        .trim()
        .strip_prefix("${")
        .and_then(|value| value.strip_suffix('}'))
        .filter(|value| valid_env_name(value))
}

pub(crate) fn valid_env_name(value: &str) -> bool {
    let mut characters = value.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

pub(crate) fn safe_secret_reference_or_placeholder(value: &str) -> bool {
    let value = trim_secret_candidate(value);
    if value.is_empty() {
        return true;
    }
    if known_secret_token(value) {
        return false;
    }
    if explicit_secret_reference(value) {
        return true;
    }
    placeholder_value(&value.to_ascii_lowercase())
}

fn trim_secret_candidate(value: &str) -> &str {
    value.trim_matches(|character: char| {
        character.is_ascii_whitespace() || matches!(character, '"' | '\'' | '`' | ',' | ';' | '\\')
    })
}

fn explicit_secret_reference(value: &str) -> bool {
    env_reference(value).is_some()
        || concatenated_template_references(value)
        || shell_env_reference(value).is_some()
        || shell_parameter_reference(value)
        || generated_env_name_reference(value)
        || shell_command_substitution_reference(value)
        || leading_shell_reference_with_safe_tail(value)
        || process_env_reference(value)
        || authorization_env_reference(value)
}

fn leading_shell_reference_with_safe_tail(value: &str) -> bool {
    leading_shell_reference(value).is_some_and(|(reference, suffix)| {
        (shell_env_reference(reference).is_some() || shell_parameter_reference(reference))
            && !credential_like_tail(suffix)
    })
}

fn shell_parameter_reference(value: &str) -> bool {
    let Some(inner) = value
        .strip_prefix("${")
        .and_then(|value| value.strip_suffix('}'))
    else {
        return false;
    };
    let inner = inner.strip_prefix('!').unwrap_or(inner);
    let inner = inner.strip_suffix(":-").unwrap_or(inner);
    valid_env_name(inner)
}

fn generated_env_name_reference(value: &str) -> bool {
    value.strip_prefix('$').and_then(env_reference).is_some()
}

fn shell_command_substitution_reference(value: &str) -> bool {
    value
        .strip_prefix("$(")
        .and_then(|value| value.strip_suffix(')'))
        .is_some_and(|command| {
            contains_unquoted_shell_variable_reference(command)
                && !contains_embedded_credential_literal(command)
                && !contains_credential_like_shell_argument(command)
        })
}

fn contains_credential_like_shell_argument(command: &str) -> bool {
    command
        .split([';', '|', '&', '{', '}'])
        .any(shell_segment_contains_credential_like_argument)
}

fn shell_segment_contains_credential_like_argument(segment: &str) -> bool {
    let mut command_seen = false;
    for token in segment.split_whitespace() {
        let token = token.trim_matches(|character: char| matches!(character, '(' | ')'));
        if token.is_empty() {
            continue;
        }
        if !command_seen {
            if let Some((_, value)) = token.split_once('=') {
                if credential_like_embedded_literal(value) {
                    return true;
                }
                continue;
            }
        }
        if !command_seen {
            command_seen = true;
            continue;
        }
        let candidate = token.trim_matches(|character: char| {
            matches!(character, '"' | '\'' | '`' | ',' | ')' | '(')
        });
        if let Some(suffix) = shell_variable_literal_suffix(candidate) {
            if credential_like_embedded_literal(suffix) {
                return true;
            }
            continue;
        }
        if !candidate.starts_with('-')
            && !candidate.contains('/')
            && !looks_like_cli_action_label(candidate)
            && credential_like_embedded_literal(candidate)
        {
            return true;
        }
    }
    false
}

fn shell_variable_literal_suffix(value: &str) -> Option<&str> {
    let rest = value.strip_prefix('$')?;
    if let Some(rest) = rest.strip_prefix('{') {
        return rest.split_once('}').map(|(_, suffix)| suffix);
    }
    let length = rest
        .chars()
        .take_while(|character| character == &'_' || character.is_ascii_alphanumeric())
        .map(char::len_utf8)
        .sum::<usize>();
    (length > 0).then_some(&rest[length..])
}

fn looks_like_cli_action_label(value: &str) -> bool {
    let mut parts = value.split('-');
    let Some(action) = parts.next() else {
        return false;
    };
    let known_action = matches!(
        action,
        "check"
            | "create"
            | "delete"
            | "emit"
            | "find"
            | "get"
            | "list"
            | "read"
            | "run"
            | "set"
            | "show"
            | "update"
            | "verify"
            | "write"
    );
    known_action
        && parts.clone().count() >= 1
        && parts.all(|part| {
            !part.is_empty()
                && part.chars().all(|character| character.is_ascii_lowercase())
                && !matches!(
                    part,
                    "auth" | "credential" | "key" | "password" | "secret" | "token"
                )
        })
}

fn contains_embedded_credential_literal(value: &str) -> bool {
    let mut quote = None;
    let mut escaped = false;
    let mut start = 0usize;
    for (index, character) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' {
            escaped = true;
            continue;
        }
        if let Some(active_quote) = quote {
            if character == active_quote {
                if credential_like_embedded_literal(&value[start..index]) {
                    return true;
                }
                quote = None;
            }
        } else if matches!(character, '\'' | '"' | '`') {
            quote = Some(character);
            start = index + character.len_utf8();
        }
    }
    false
}

fn credential_like_embedded_literal(value: &str) -> bool {
    let value = value.trim();
    if value.len() < 12
        || env_reference(value).is_some()
        || shell_env_reference(value).is_some()
        || shell_parameter_reference(value)
        || process_env_reference(value)
        || placeholder_value(&value.to_ascii_lowercase())
        || looks_like_public_url(value)
    {
        return false;
    }
    value.len() >= 12
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric())
        || value.chars().any(|character| {
            character.is_ascii_digit()
                || character.is_ascii_whitespace()
                || matches!(character, '-' | '/' | '_')
        })
}

fn contains_unquoted_shell_variable_reference(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut index = 0;
    let mut single_quoted = false;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 2,
            b'\'' => {
                single_quoted = !single_quoted;
                index += 1;
            }
            b'$' if !single_quoted => {
                let rest = &value[index + 1..];
                if let Some(inner) = rest.strip_prefix('{').and_then(|rest| rest.split_once('}')) {
                    if shell_parameter_reference(&format!("${{{}}}", inner.0)) {
                        return true;
                    }
                } else if rest
                    .chars()
                    .next()
                    .is_some_and(|character| character == '_' || character.is_ascii_alphabetic())
                {
                    return true;
                }
                index += 1;
            }
            _ => index += 1,
        }
    }
    false
}

fn concatenated_template_references(mut value: &str) -> bool {
    let mut found = false;
    while let Some(rest) = value.strip_prefix("${") {
        let Some((name, remaining)) = rest.split_once('}') else {
            return false;
        };
        let name = name.strip_prefix("env:").unwrap_or(name);
        if !name.split('.').all(valid_env_name) {
            return false;
        }
        found = true;
        value = remaining;
    }
    found && value.is_empty()
}

fn process_env_reference(value: &str) -> bool {
    let value = value.strip_suffix('!').unwrap_or(value);
    value
        .strip_prefix("process.env.")
        .is_some_and(valid_env_name)
}

fn authorization_env_reference(value: &str) -> bool {
    ["Bearer ", "Basic ", "bearer ", "basic "]
        .iter()
        .find_map(|marker| value.strip_prefix(marker))
        .is_some_and(|reference| {
            let reference = reference.trim_matches(|character: char| {
                character.is_ascii_whitespace()
                    || matches!(
                        character,
                        '"' | '\'' | '`' | ',' | ';' | ')' | ']' | '|' | '\\'
                    )
            });
            let exact = env_reference(reference).is_some()
                || concatenated_template_references(reference)
                || shell_env_reference(reference).is_some()
                || generated_env_name_reference(trim_secret_candidate(reference))
                || shell_parameter_reference(reference);
            exact || leading_shell_reference_with_safe_tail(reference)
        })
}

fn leading_shell_reference(value: &str) -> Option<(&str, &str)> {
    if let Some(rest) = value.strip_prefix("${") {
        let end = rest.find('}')? + 3;
        return Some((&value[..end], &value[end..]));
    }
    let rest = value.strip_prefix('$')?;
    let length = rest
        .chars()
        .take_while(|character| character == &'_' || character.is_ascii_alphanumeric())
        .map(char::len_utf8)
        .sum::<usize>();
    (length > 0).then_some((&value[..length + 1], &value[length + 1..]))
}

fn credential_like_tail(value: &str) -> bool {
    contains_embedded_credential_literal(value)
        || value.split_whitespace().any(|raw_word| {
            let label = raw_word.trim_matches(|character: char| {
                !character.is_ascii_alphanumeric() && !matches!(character, '-' | '/' | '_' | ':')
            });
            if label.ends_with(':') {
                return false;
            }
            let word = raw_word.trim_matches(|character: char| {
                !character.is_ascii_alphanumeric()
                    && !matches!(character, '$' | '-' | '/' | '_' | '{' | '}')
            });
            shell_env_reference(word).is_none()
                && !shell_parameter_reference(word)
                && credential_like_embedded_literal(word)
        })
}

const PLACEHOLDER_FRAGMENTS: &[&str] = &[
    "placeholder",
    "redacted",
    "-example-",
    "replace-me",
    "replace_me",
    "your-",
    "your_",
];

const PLACEHOLDER_PREFIXES: &[&str] = &["example-", "dummy-", "fake-"];
const PLACEHOLDER_VALUES: &[&str] = &[
    "...",
    "<secret>",
    "<api-key>",
    "<api_key>",
    "<apikey>",
    "<password>",
    "<token>",
    "apikey",
    "auto",
    "changeme",
    "dummy",
    "example",
    "fake",
    "false",
    "key",
    "none",
    "no",
    "null",
    "secret",
    "string",
    "test",
    "true",
    "xxx",
    "xxxxx",
    "yes",
];

fn placeholder_value(lower: &str) -> bool {
    PLACEHOLDER_VALUES.contains(&lower)
        || contains_any(lower, PLACEHOLDER_FRAGMENTS)
        || lower
            .split_whitespace()
            .next()
            .is_some_and(|value| value == "pscale_pw_xxx")
        || PLACEHOLDER_PREFIXES
            .iter()
            .any(|prefix| lower.starts_with(prefix))
}

fn shell_env_reference(value: &str) -> Option<&str> {
    value
        .strip_prefix('$')
        .filter(|value| valid_env_name(value))
}

pub(crate) fn raw_secret_reason(value: &str) -> Option<&'static str> {
    raw_secret_in_text(value).map(|(_, reason)| reason)
}

/// Redacts high-confidence secret tokens and literal sensitive assignments.
///
/// The returned count is the number of non-overlapping values that were replaced.
pub(crate) fn redact_known_secrets(text: &str) -> (String, usize) {
    let mut ranges = secret_token_ranges(text);
    ranges.extend(private_key_block_ranges(text));
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        ranges.extend(
            line_secret_ranges(line)
                .into_iter()
                .map(|range| range.start + offset..range.end + offset),
        );
        offset += line.len();
    }
    let ranges = merge_secret_ranges(ranges);
    redact_ranges(text, &ranges)
}

fn line_secret_ranges(line: &str) -> Vec<Range<usize>> {
    let mut ranges = secret_assignment_ranges(line);
    ranges.extend(authorization_secret_ranges(line));
    ranges.extend(literal_url_userinfo_ranges(line));
    ranges
}

fn merge_secret_ranges(mut ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    ranges.sort_by_key(|range| (range.start, range.end));
    let mut merged: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
    for range in ranges {
        if let Some(previous) = merged.last_mut() {
            if range.start < previous.end {
                previous.end = previous.end.max(range.end);
                continue;
            }
        }
        merged.push(range);
    }
    merged
}

fn redact_ranges(text: &str, ranges: &[Range<usize>]) -> (String, usize) {
    let mut redacted = String::with_capacity(text.len());
    let mut cursor = 0;
    for range in ranges {
        redacted.push_str(&text[cursor..range.start]);
        push_redacted_value(&mut redacted, &text[range.clone()]);
        cursor = range.end;
    }
    redacted.push_str(&text[cursor..]);
    (redacted, ranges.len())
}

fn push_redacted_value(output: &mut String, value: &str) {
    if !value.contains('\n') {
        output.push_str("[REDACTED]");
        return;
    }
    for line in value.split_inclusive('\n') {
        output.push_str("[REDACTED]");
        if line.ends_with("\r\n") {
            output.push_str("\r\n");
        } else if line.ends_with('\n') {
            output.push('\n');
        }
    }
}

fn private_key_block_ranges(text: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut cursor = 0;
    while cursor < text.len() {
        let line_end = line_end_after(text, cursor);
        if private_key_marker(&text[cursor..line_end], "begin") {
            let block_end = private_key_block_end(text, line_end);
            ranges.push(cursor..block_end);
            cursor = block_end;
        } else {
            cursor = line_end;
        }
    }
    ranges
}

fn private_key_block_end(text: &str, mut cursor: usize) -> usize {
    while cursor < text.len() {
        let line_end = line_end_after(text, cursor);
        if private_key_marker(&text[cursor..line_end], "end") {
            return line_end;
        }
        cursor = line_end;
    }
    text.len()
}

fn line_end_after(text: &str, start: usize) -> usize {
    text[start..]
        .find('\n')
        .map_or(text.len(), |relative| start + relative + 1)
}

fn private_key_marker(line: &str, action: &str) -> bool {
    let line = line.to_ascii_lowercase();
    line.contains(&format!("-----{action} ")) && line.contains("private key-----")
}

fn raw_secret_in_text(text: &str) -> Option<(usize, &'static str)> {
    for (index, line) in text.lines().enumerate() {
        let line_number = index + 1;
        let lower = line.to_ascii_lowercase();
        if lower.contains("-----begin ") && lower.contains("private key-----") {
            return Some((line_number, "private key material"));
        }
        if contains_known_secret_token(line) {
            return Some((line_number, "credential-like token"));
        }
        if let Some(reason) = raw_authorization_value(line) {
            return Some((line_number, reason));
        }
        if raw_assignment_secret(line) {
            return Some((line_number, "literal value assigned to a sensitive key"));
        }
        if let Some(reason) = raw_url_secret(line) {
            return Some((line_number, reason));
        }
    }
    None
}

fn raw_assignment_secret(line: &str) -> bool {
    !secret_assignment_ranges(line).is_empty()
}

fn secret_assignment_ranges(line: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    for (separator, character) in line.char_indices() {
        if !matches!(character, '=' | ':') {
            continue;
        }
        if character == '=' && repeated_operator_equals(line, separator) {
            continue;
        }
        if character == '=' && empty_quoted_value_at(line, separator) {
            continue;
        }
        if character == ':' && scope_resolution_colon(line, separator) {
            continue;
        }
        if character == ':' && markdown_code_function_separator(line, separator) {
            continue;
        }
        if character == ':' && matches!(line.as_bytes().get(separator + 1), Some(b'/' | b'\\')) {
            continue;
        }
        let before_separator = &line[..separator];
        if separator_in_url(before_separator) {
            continue;
        }
        if character == ':' && ternary_operator_at(line, separator) {
            continue;
        }
        let key_separator = comparison_key_separator(line, separator, character);
        let key_start = assignment_key_start(line, key_separator, character);
        let key = &line[key_start..key_separator];
        if !sensitive_key(key) {
            continue;
        }
        let value_start = separator + character.len_utf8();
        let Some(value) = assignment_value_range(line, value_start) else {
            continue;
        };
        let quoted = line[value.clone()].starts_with(['"', '\'', '`']);
        let redaction = assignment_redaction_range(line, value.clone(), quoted);
        if known_minified_token_enum_call(key, &line[value.clone()]) {
            continue;
        }
        if supabase_auth_uid_comparison(key, character, &line[value.clone()]) {
            continue;
        }
        if markdown_bold_label(key, character, &line[value.clone()]) {
            continue;
        }
        if markdown_definition(key, character, &line[value.clone()]) {
            continue;
        }
        if markdown_inline_placeholder(key, character, &line[value.clone()]) {
            continue;
        }
        if markdown_inline_documentation(key, character, &line[value.clone()]) {
            continue;
        }
        if placeholder_with_warning(&line[value.clone()]) {
            continue;
        }
        if literal_secret_assignment(&line[value], quoted) {
            ranges.push(redaction);
        }
    }
    ranges
}

fn markdown_code_function_separator(line: &str, separator: usize) -> bool {
    let Some(open) = line[..separator].rfind('`') else {
        return false;
    };
    let path = &line[open + 1..separator];
    if !path.contains('/') || !path.contains('.') {
        return false;
    }
    let Some(end) = line[separator + 1..].find('`') else {
        return false;
    };
    let function = &line[separator + 1..separator + 1 + end];
    !function.is_empty()
        && function
            .chars()
            .all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn markdown_inline_placeholder(key: &str, separator: char, value: &str) -> bool {
    if separator != ':' || key.split_whitespace().count() < 4 {
        return false;
    }
    let trimmed = value.trim_start();
    if let Some((placeholder, suffix)) = trimmed
        .strip_prefix('<')
        .and_then(|value| value.split_once('>'))
    {
        return !placeholder.is_empty()
            && placeholder
                .chars()
                .all(|character| character == '_' || character.is_ascii_alphanumeric())
            && suffix.starts_with('`');
    }
    let Some(inner) = value
        .trim()
        .strip_prefix('`')
        .and_then(|value| value.strip_suffix('`'))
    else {
        return false;
    };
    inner.split_once('=').is_some_and(|(_, placeholder)| {
        placeholder.starts_with('<')
            && placeholder.ends_with('>')
            && placeholder[1..placeholder.len() - 1]
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '-')
    })
}

fn placeholder_with_warning(value: &str) -> bool {
    value.split_once('←').is_some_and(|(placeholder, warning)| {
        placeholder_value(&placeholder.trim().to_ascii_lowercase())
            && warning.trim_start().starts_with("DO NOT")
            && !credential_like_tail(
                &warning
                    .split_whitespace()
                    .filter(|word| {
                        !matches!(word.to_ascii_lowercase().as_str(), "(client-visible)")
                    })
                    .collect::<Vec<_>>()
                    .join(" "),
            )
    })
}

fn empty_quoted_value_at(line: &str, separator: usize) -> bool {
    let mut quote = None;
    let mut escaped = false;
    for character in line[..separator].chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' {
            escaped = true;
            continue;
        }
        if let Some(active_quote) = quote {
            if character == active_quote {
                quote = None;
            }
        } else if matches!(character, '\'' | '"' | '`') {
            quote = Some(character);
        }
    }
    quote.is_some_and(|active_quote| line[separator + 1..].starts_with(active_quote))
}

fn markdown_bold_label(key: &str, separator: char, value: &str) -> bool {
    separator == ':' && key.trim_start().starts_with("**") && value.trim() == "**"
}

fn markdown_definition(key: &str, separator: char, value: &str) -> bool {
    if separator != ':' || !looks_like_documentation_value(value) {
        return false;
    }
    let key = key.trim();
    let numbered_bold = key.split_once('.').is_some_and(|(number, label)| {
        number.chars().all(|character| character.is_ascii_digit())
            && label.trim().starts_with("**")
            && label.trim()[2..].contains("**")
    });
    let scorecard = key.starts_with("**") && key.ends_with("**") && value.contains("| /10 |");
    let bullet_bold = key.starts_with("- **") && key[4..].contains("**");
    let example_bullet = (key.starts_with("- For ") && key.contains("(e.g., `"))
        || (key.starts_with('`') && key.contains('/') && key.ends_with("`)"));
    let token_schema = key.trim_matches('*').eq_ignore_ascii_case("token")
        && value.to_ascii_lowercase().contains("authentication token")
        && value.to_ascii_lowercase().contains("used by");
    numbered_bold || scorecard || bullet_bold || example_bullet || token_schema
}

fn supabase_auth_uid_comparison(key: &str, separator: char, value: &str) -> bool {
    let key = key.trim().trim_start_matches('(');
    let Some(suffix) = key.strip_prefix("auth.uid()") else {
        return false;
    };
    if separator != '=' || !suffix.chars().all(|character| character == ')') {
        return false;
    }
    let value = value.trim().trim_end_matches(|character: char| {
        character.is_ascii_whitespace() || matches!(character, ')' | ',' | ';')
    });
    value.ends_with("_id")
        && value
            .chars()
            .all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn scope_resolution_colon(line: &str, separator: usize) -> bool {
    matches!(line.as_bytes().get(separator.wrapping_sub(1)), Some(b':'))
        || matches!(line.as_bytes().get(separator + 1), Some(b':'))
}

fn known_minified_token_enum_call(key: &str, value: &str) -> bool {
    const LABELS: &[&str] = &[
        "absence_repeater",
        "any",
        "atomic",
        "capturing",
        "dot",
        "group",
        "keep",
        "newline",
        "text_segment",
    ];
    if key.trim_matches(|character: char| !character.is_ascii_alphanumeric()) != "token" {
        return false;
    }
    let value = value.trim().trim_end_matches([',', ';']);
    let Some(value) = value.strip_suffix(')') else {
        return false;
    };
    let Some((callee, arguments)) = value.split_once('(') else {
        return false;
    };
    if !(1..=2).contains(&callee.len())
        || !callee
            .chars()
            .all(|character| character.is_ascii_alphanumeric())
    {
        return false;
    }
    let Some(quote) = arguments
        .chars()
        .next()
        .filter(|quote| matches!(quote, '"' | '\''))
    else {
        return false;
    };
    let Some((label, tail)) = arguments[quote.len_utf8()..].split_once(quote) else {
        return false;
    };
    LABELS.contains(&label) && safe_minified_token_tail(tail)
}

fn safe_minified_token_tail(tail: &str) -> bool {
    let Some(tail) = tail.strip_prefix(',') else {
        return false;
    };
    let identifier_length = tail
        .chars()
        .take_while(|character| character.is_ascii_alphanumeric())
        .map(char::len_utf8)
        .sum::<usize>();
    if !(1..=2).contains(&identifier_length) {
        return false;
    }
    let tail = &tail[identifier_length..];
    tail.is_empty()
        || (tail.starts_with(",{") && tail.ends_with('}') && has_only_short_quoted_literals(tail))
}

fn has_only_short_quoted_literals(value: &str) -> bool {
    let mut quote = None;
    let mut escaped = false;
    let mut quoted_length = 0usize;
    for character in value.chars() {
        if escaped {
            escaped = false;
            quoted_length += 1;
            continue;
        }
        if character == '\\' {
            escaped = true;
            continue;
        }
        if let Some(active_quote) = quote {
            if character == active_quote {
                if quoted_length > 2 {
                    return false;
                }
                quote = None;
                quoted_length = 0;
            } else {
                quoted_length += 1;
            }
        } else if matches!(character, '\'' | '"' | '`') {
            quote = Some(character);
        }
    }
    quote.is_none() && !escaped
}

fn ternary_operator_at(line: &str, separator: usize) -> bool {
    let mut questions = Vec::new();
    let mut quote = None;
    let mut escaped = false;
    let mut depth = [0usize; 3];
    for (index, character) in line[..separator].char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' {
            escaped = true;
            continue;
        }
        if let Some(active_quote) = quote {
            if character == active_quote {
                quote = None;
            }
            continue;
        }
        if matches!(character, '"' | '\'' | '`') {
            quote = Some(character);
            continue;
        }
        match character {
            '(' => depth[0] += 1,
            ')' => depth[0] = depth[0].saturating_sub(1),
            '[' => depth[1] += 1,
            ']' => depth[1] = depth[1].saturating_sub(1),
            '{' => depth[2] += 1,
            '}' => depth[2] = depth[2].saturating_sub(1),
            '?' if !line[index + 1..].starts_with(['.', '?']) => questions.push((index, depth)),
            ':' => {
                if let Some(position) = questions
                    .iter()
                    .rposition(|(_, question_depth)| *question_depth == depth)
                {
                    questions.remove(position);
                }
            }
            _ => {}
        }
    }
    questions
        .iter()
        .rfind(|(_, question_depth)| *question_depth == depth)
        .is_some_and(|(question, _)| !line[question + '?'.len_utf8()..separator].trim().is_empty())
}

fn repeated_operator_equals(line: &str, separator: usize) -> bool {
    separator
        .checked_sub(1)
        .and_then(|previous| line.as_bytes().get(previous))
        == Some(&b'=')
}

fn comparison_key_separator(line: &str, separator: usize, character: char) -> usize {
    if character != '=' {
        return separator;
    }
    separator
        .checked_sub(1)
        .filter(|previous| matches!(line.as_bytes().get(*previous), Some(b'!' | b'<' | b'>')))
        .unwrap_or(separator)
}

fn markdown_inline_documentation(key: &str, separator: char, value: &str) -> bool {
    separator == ':'
        && key.split_whitespace().count() >= 4
        && value.trim_start().starts_with('`')
        && looks_like_documentation_value(trim_secret_candidate(value))
}

fn assignment_key_start(line: &str, separator: usize, character: char) -> usize {
    let structural_start = line[..separator]
        .rfind([',', ';', '{', '[', '|', '—'])
        .map_or(0, |index| {
            index + line[index..].chars().next().unwrap().len_utf8()
        });
    if character != '=' {
        return structural_start;
    }
    let candidate = &line[structural_start..separator];
    let trimmed = candidate.trim_end();
    trimmed
        .rfind(char::is_whitespace)
        .map_or(structural_start, |index| {
            structural_start + index + trimmed[index..].chars().next().unwrap().len_utf8()
        })
}

fn separator_in_url(before_separator: &str) -> bool {
    let segment_start = before_separator
        .rfind(|character: char| {
            character.is_ascii_whitespace()
                || matches!(character, '"' | '\'' | '`' | ',' | ';' | '(' | ')')
        })
        .map_or(0, |index| index + 1);
    let segment = &before_separator[segment_start..];
    segment.contains("://")
        || segment
            .rfind('?')
            .is_some_and(|query| segment[..query].contains('/'))
}

fn assignment_value_range(line: &str, start: usize) -> Option<Range<usize>> {
    let rest = &line[start..];
    let leading = rest.len() - rest.trim_start().len();
    let operator_length = rest[leading..]
        .chars()
        .take_while(|character| matches!(character, '=' | '>' | '~'))
        .map(char::len_utf8)
        .sum::<usize>();
    let after_operator = &rest[leading + operator_length..];
    let operator_spacing = after_operator.len() - after_operator.trim_start().len();
    let value_start = start + leading + operator_length + operator_spacing;
    let value = &line[value_start..];
    if value.is_empty() {
        return None;
    }
    if let Some(quote) = value
        .chars()
        .next()
        .filter(|quote| matches!(quote, '"' | '\'' | '`'))
    {
        if let Some(end) = quoted_assignment_end(value, quote) {
            return Some(value_start..value_start + end);
        }
    }
    if value.starts_with("${") {
        if let Some(end) = value.find('}') {
            return Some(value_start..value_start + end + 1);
        }
    }
    let length = unquoted_assignment_length(value);
    let trailing = value[..length].trim_end().len();
    (trailing > 0).then_some(value_start..value_start + trailing)
}

fn quoted_assignment_end(value: &str, quote: char) -> Option<usize> {
    let mut escaped = false;
    let mut command_depth = 0usize;
    let mut previous = None;
    for (index, character) in value.char_indices().skip(1) {
        if escaped {
            escaped = false;
            previous = Some(character);
            continue;
        }
        if character == '\\' {
            escaped = true;
            previous = Some(character);
            continue;
        }
        if quote == '"' && previous == Some('$') && character == '(' {
            command_depth += 1;
        } else if quote == '"' && command_depth > 0 && character == ')' {
            command_depth -= 1;
        } else if character == quote && command_depth == 0 {
            return Some(index + character.len_utf8());
        }
        previous = Some(character);
    }
    None
}

fn unquoted_assignment_length(value: &str) -> usize {
    let mut angle_depth = 0;
    let mut square_depth = 0;
    let mut parenthesis_depth = 0;
    let mut curly_depth = 0;
    let mut quote = None;
    let mut escaped = false;
    for (index, character) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' {
            escaped = true;
            continue;
        }
        if let Some(active_quote) = quote {
            if character == active_quote {
                quote = None;
            }
            continue;
        }
        if matches!(character, '"' | '\'' | '`') {
            quote = Some(character);
            continue;
        }
        match character {
            '<' => angle_depth += 1,
            '>' if angle_depth > 0 => angle_depth -= 1,
            '[' => square_depth += 1,
            ']' if square_depth > 0 => square_depth -= 1,
            '(' => parenthesis_depth += 1,
            ')' if parenthesis_depth > 0 => parenthesis_depth -= 1,
            '{' => curly_depth += 1,
            '}' if curly_depth > 0 => curly_depth -= 1,
            ',' | '}' | ']' | ';'
                if assignment_delimiters_closed(
                    angle_depth,
                    square_depth,
                    parenthesis_depth,
                    curly_depth,
                ) =>
            {
                return index;
            }
            _ => {}
        }
    }
    value.len()
}

fn assignment_delimiters_closed(
    angle: usize,
    square: usize,
    parenthesis: usize,
    curly: usize,
) -> bool {
    [angle, square, parenthesis, curly]
        .iter()
        .all(|depth| *depth == 0)
}

fn assignment_redaction_range(line: &str, range: Range<usize>, quoted: bool) -> Range<usize> {
    if !quoted {
        return range;
    }
    let quote_length = line[range.clone()].chars().next().unwrap().len_utf8();
    range.start + quote_length..range.end - quote_length
}

fn literal_secret_assignment(value: &str, quoted: bool) -> bool {
    if safe_secret_reference_or_placeholder(value) {
        return false;
    }
    if looks_like_public_url(value) {
        return false;
    }
    if !authorization_secret_ranges(value).is_empty() {
        return false;
    }
    !(!quoted
        && (looks_like_type_declaration(value)
            || looks_like_code_expression(value)
            || looks_like_short_code_identifier(value)))
}

fn looks_like_short_code_identifier(value: &str) -> bool {
    let value = value.trim().trim_end_matches([',', ';']);
    (1..=2).contains(&value.len())
        && value
            .chars()
            .all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn looks_like_public_url(value: &str) -> bool {
    let value = trim_secret_candidate(value);
    let absolute = value.starts_with("https://") || value.starts_with("http://");
    let domain_path = value.split_once('/').is_some_and(|(host, _)| {
        host.contains('.')
            && !host.contains('@')
            && host.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '.' | '-')
            })
    });
    (absolute || domain_path) && raw_url_secret(value).is_none()
}

fn looks_like_code_expression(value: &str) -> bool {
    let value = value.trim().trim_end_matches([',', ';']);
    if value.contains('.')
        && value
            .split('.')
            .all(|part| !part.is_empty() && part.chars().all(valid_identifier_character))
    {
        return true;
    }
    let Some(open) = value.find('(') else {
        return false;
    };
    if !value.ends_with(')') {
        return false;
    }
    if value[open + 1..].trim_start().starts_with(['"', '\'', '`']) {
        return false;
    }
    let callee = value[..open].trim();
    let callee = callee
        .strip_prefix("await ")
        .or_else(|| callee.strip_prefix("new "))
        .unwrap_or(callee);
    !callee.is_empty()
        && callee.chars().all(valid_identifier_character)
        && (!contains_embedded_credential_literal(value) || zod_min_validation_expression(value))
}

fn zod_min_validation_expression(value: &str) -> bool {
    let Some(arguments) = value
        .strip_prefix("z.string().min(")
        .and_then(|value| value.strip_suffix(')'))
    else {
        return false;
    };
    let Some((minimum, message)) = arguments.split_once(',') else {
        return false;
    };
    let minimum = minimum.trim();
    if minimum.is_empty() || !minimum.chars().all(|character| character.is_ascii_digit()) {
        return false;
    }
    let message = message
        .trim()
        .strip_prefix(['\'', '"'])
        .and_then(|message| message.strip_suffix(['\'', '"']));
    message == Some(format!("Password must be at least {minimum} characters").as_str())
}

fn valid_identifier_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '_' | '.')
}

const DECLARATION_TYPES: &[&str] = &[
    "bool", "f32", "f64", "i8", "i16", "i32", "i64", "i128", "isize", "number", "str", "string",
    "String", "u8", "u16", "u32", "u64", "u128", "unknown", "usize",
];

fn looks_like_type_declaration(value: &str) -> bool {
    let value = value.trim().trim_end_matches(|character: char| {
        character.is_ascii_whitespace() || matches!(character, ',' | ';' | ')' | '{')
    });
    if DECLARATION_TYPES.contains(&value) {
        return true;
    }
    if value.starts_with('&') && value.ends_with("str") {
        return true;
    }
    let has_type_delimiters = value.contains(['<', '[']) && value.contains(['>', ']']);
    has_type_delimiters
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || character.is_ascii_whitespace()
                || matches!(
                    character,
                    '_' | ':' | '<' | '>' | '[' | ']' | ',' | '&' | '\''
                )
        })
}

const DOCUMENTATION_WORDS: &[&str] = &[
    "a",
    "accessible",
    "an",
    "by",
    "confirms",
    "does",
    "enforced",
    "for",
    "from",
    "is",
    "must",
    "of",
    "properly",
    "saved",
    "search",
    "should",
    "shows",
    "suggests",
    "tells",
    "that",
    "the",
    "this",
    "to",
    "used",
    "which",
];

const EXPLANATORY_WORDS: &[&str] = &[
    "accessible",
    "confirms",
    "does",
    "enforced",
    "is",
    "must",
    "saved",
    "search",
    "should",
    "shows",
    "suggests",
    "tells",
    "this",
    "used",
    "which",
];

fn looks_like_documentation_value(value: &str) -> bool {
    let words = value
        .split_whitespace()
        .map(|word| {
            word.trim_matches(|character: char| !character.is_ascii_alphanumeric())
                .to_ascii_lowercase()
        })
        .collect::<Vec<_>>();
    let documentation_words = words
        .iter()
        .filter(|word| DOCUMENTATION_WORDS.contains(&word.as_str()))
        .count();
    words.len() >= 4
        && documentation_words >= 2
        && words
            .iter()
            .any(|word| EXPLANATORY_WORDS.contains(&word.as_str()))
}

fn raw_authorization_value(line: &str) -> Option<&'static str> {
    (!authorization_secret_ranges(line).is_empty()).then_some("literal authorization value")
}

const AUTHORIZATION_MARKERS: &[&str] = &["bearer ", "basic "];

fn authorization_secret_ranges(line: &str) -> Vec<Range<usize>> {
    let lower = line.to_ascii_lowercase();
    let mut ranges = Vec::new();
    for marker in AUTHORIZATION_MARKERS {
        let mut cursor = 0;
        while let Some(relative) = lower[cursor..].find(marker) {
            let marker_start = cursor + relative;
            let candidate_start = marker_start + marker.len();
            let candidate_end = authorization_candidate_end(line, candidate_start);
            let candidate = &line[candidate_start..candidate_end];
            if authorization_marker_context(line, marker_start)
                && literal_authorization_candidate(
                    candidate,
                    &generic_authorization_word(candidate),
                )
            {
                ranges.push(candidate_start..candidate_end);
            }
            cursor = candidate_end.max(candidate_start);
        }
    }
    ranges
}

fn authorization_marker_context(line: &str, marker_start: usize) -> bool {
    let before = line[..marker_start].to_ascii_lowercase();
    before.contains("authorization")
        || before.trim().is_empty()
        || line[marker_start..].starts_with("Bearer ")
        || line[marker_start..].starts_with("Basic ")
}

fn authorization_candidate_end(line: &str, start: usize) -> usize {
    line[start..]
        .char_indices()
        .find(|(_, character)| authorization_candidate_delimiter(*character))
        .map_or(line.len(), |(relative, _)| start + relative)
}

fn authorization_candidate_delimiter(character: char) -> bool {
    character.is_whitespace() || matches!(character, '"' | '\'' | '`' | ',' | ';' | ')' | ']')
}

fn generic_authorization_word(candidate: &str) -> String {
    candidate
        .trim_matches(|character: char| !character.is_ascii_alphanumeric())
        .to_ascii_lowercase()
}

const GENERIC_AUTHORIZATION_WORDS: &[&str] = &[
    "auth",
    "authentication",
    "credential",
    "credentials",
    "token",
    "tokens",
];

fn literal_authorization_candidate(candidate: &str, generic: &str) -> bool {
    candidate.len() >= 12
        && !GENERIC_AUTHORIZATION_WORDS.contains(&generic)
        && !safe_secret_reference_or_placeholder(candidate)
}

fn raw_url_secret(line: &str) -> Option<&'static str> {
    if !literal_url_userinfo_ranges(line).is_empty() {
        return Some("credential-bearing URL user info");
    }
    if !line.contains("://")
        && !line
            .find('?')
            .is_some_and(|query| line[..query].contains('/'))
    {
        return None;
    }
    let query = line
        .split_once('?')?
        .1
        .split('#')
        .next()
        .unwrap_or_default();
    for item in query.split(['&', ';']) {
        let Some((key, value)) = item.split_once('=') else {
            continue;
        };
        let value_end = value
            .char_indices()
            .find(|(_, character)| {
                character.is_whitespace() || matches!(character, '"' | '\'' | '`' | ',' | ')' | ']')
            })
            .map_or(value.len(), |(index, _)| index);
        let value = &value[..value_end];
        if sensitive_key(key) && !safe_secret_reference_or_placeholder(value) {
            return Some("literal credential in a URL query");
        }
    }
    None
}

fn literal_url_userinfo_ranges(line: &str) -> Vec<Range<usize>> {
    url_userinfo_ranges(line)
        .into_iter()
        .filter(|range| !safe_url_userinfo(&line[range.clone()]))
        .collect()
}

fn safe_url_userinfo(value: &str) -> bool {
    if value.eq_ignore_ascii_case("[REDACTED]") {
        return true;
    }
    let credential = value
        .rsplit_once(':')
        .map_or(value, |(_, password)| password);
    safe_secret_reference_or_placeholder(credential)
}

fn url_userinfo_ranges(line: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut cursor = 0;
    while let Some(relative) = line[cursor..].find("://") {
        let authority_start = cursor + relative + 3;
        let authority_end = url_authority_end(line, authority_start);
        let authority = &line[authority_start..authority_end];
        if let Some(at) = authority.rfind('@').filter(|at| *at > 0) {
            ranges.push(authority_start..authority_start + at);
        }
        if authority_end == line.len() {
            break;
        }
        cursor = authority_end + line[authority_end..].chars().next().unwrap().len_utf8();
    }
    ranges
}

fn url_authority_end(line: &str, start: usize) -> usize {
    line[start..]
        .char_indices()
        .find(|(_, character)| url_authority_delimiter(*character))
        .map_or(line.len(), |(relative, _)| start + relative)
}

fn url_authority_delimiter(character: char) -> bool {
    character.is_whitespace() || matches!(character, '/' | '?' | '#' | '"' | '\'' | '`' | ',' | ';')
}

fn contains_known_secret_token(text: &str) -> bool {
    !secret_token_ranges(text).is_empty()
}

#[derive(Clone, Copy)]
struct SecretPrefix {
    prefix: &'static str,
    minimum_length: usize,
}

const SECRET_PREFIXES: &[SecretPrefix] = &[
    SecretPrefix {
        prefix: "sk-",
        minimum_length: 12,
    },
    SecretPrefix {
        prefix: "ghp_",
        minimum_length: 20,
    },
    SecretPrefix {
        prefix: "github_pat_",
        minimum_length: 20,
    },
    SecretPrefix {
        prefix: "glpat-",
        minimum_length: 15,
    },
    SecretPrefix {
        prefix: "npm_",
        minimum_length: 20,
    },
    SecretPrefix {
        prefix: "pypi-",
        minimum_length: 20,
    },
];

fn secret_token_ranges(text: &str) -> Vec<Range<usize>> {
    candidate_token_ranges(text)
        .into_iter()
        .filter(|range| {
            known_secret_token(&text[range.clone()])
                && !explicit_env_reference_range(text, range.clone())
        })
        .collect()
}

fn candidate_token_ranges(text: &str) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut start = None;
    for (index, character) in text.char_indices() {
        if secret_token_character(character) {
            start.get_or_insert(index);
        } else if let Some(start) = start.take() {
            ranges.push(start..index);
        }
    }
    if let Some(start) = start {
        ranges.push(start..text.len());
    }
    ranges
}

fn secret_token_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.')
}

fn explicit_env_reference_range(text: &str, range: Range<usize>) -> bool {
    let candidate = &text[range.clone()];
    if !valid_env_name(candidate) {
        return false;
    }
    let before = &text[..range.start];
    let after = &text[range.end..];
    (before.ends_with("${") && after.starts_with('}'))
        || before.ends_with('$')
        || before.ends_with("process.env.")
}

fn known_secret_token(token: &str) -> bool {
    prefixed_secret_token(token)
        || slack_secret_token(token)
        || aws_access_key_id(token)
        || google_api_key(token)
        || looks_like_jwt(token)
}

fn prefixed_secret_token(token: &str) -> bool {
    SECRET_PREFIXES.iter().any(|pattern| {
        token.starts_with(pattern.prefix)
            && token.len() >= pattern.minimum_length
            && token.chars().all(secret_token_character)
    })
}

fn slack_secret_token(token: &str) -> bool {
    token.starts_with("xox")
        && token.contains('-')
        && token.len() >= 20
        && token.chars().all(secret_token_character)
}

fn aws_access_key_id(token: &str) -> bool {
    token.starts_with("AKIA")
        && token.len() == 20
        && token
            .chars()
            .all(|character| character.is_ascii_alphanumeric())
}

fn google_api_key(token: &str) -> bool {
    token.starts_with("AIza") && token.len() >= 20 && token.chars().all(secret_token_character)
}

fn looks_like_jwt(token: &str) -> bool {
    if !token.starts_with("eyJ") || token.len() < 30 {
        return false;
    }
    let parts = token.split('.').collect::<Vec<_>>();
    parts.len() == 3
        && parts.iter().all(|part| {
            !part.is_empty()
                && part.chars().all(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '=')
                })
        })
}

pub fn backup_path(backup_root: &Path, dest_root: &Path, dest: &Path) -> PathBuf {
    match dest.strip_prefix(dest_root) {
        Ok(relative) => backup_root.join(relative),
        Err(_) => {
            let mut hasher = Sha256::new();
            hasher.update(dest.to_string_lossy().as_bytes());
            let hash = format!("{:x}", hasher.finalize());
            let name = dest
                .file_name()
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| std::ffi::OsStr::new("resource"));
            backup_root.join("external").join(hash).join(name)
        }
    }
}

pub fn replace_dir_with_backup_if_unchanged(
    backup_root: &Path,
    dest_root: &Path,
    src: &Path,
    dest: &Path,
    expected_sha256: Option<&str>,
) -> Result<Option<PathBuf>> {
    let staged = unique_nonexistent_sibling(dest, "new");
    if let Err(error) = copy_dir(src, &staged) {
        let _ = fs::remove_dir_all(&staged);
        return Err(error).with_context(|| format!("stage replacement for {}", dest.display()));
    }
    if expected_sha256.is_none() {
        return install_new_target(&staged, dest).map(|()| None);
    }
    let previous = displace_existing(dest)?;
    let actual_sha256 = match previous.as_deref() {
        Some(previous) => require_displaced_directory(previous)
            .and_then(|()| hash_path(previous))
            .map(Some),
        None => Ok(None),
    };
    let actual_sha256 = match actual_sha256 {
        Ok(hash) => hash,
        Err(error) => {
            let _ = fs::remove_dir_all(&staged);
            return Err(error_after_restore(
                error.context("inspect displaced target before directory replacement"),
                dest,
                previous.as_deref(),
            ));
        }
    };
    if actual_sha256.as_deref() != expected_sha256 {
        let _ = fs::remove_dir_all(&staged);
        return Err(error_after_restore(
            anyhow::anyhow!("{} changed after preview; stopped", dest.display()),
            dest,
            previous.as_deref(),
        ));
    }
    let backup = match backup_displaced(backup_root, dest_root, dest, previous.as_deref()) {
        Ok(backup) => backup,
        Err(error) => {
            let _ = fs::remove_dir_all(&staged);
            return Err(error_after_restore(error, dest, previous.as_deref()));
        }
    };
    if let Err(error) = rename_noreplace(&staged, dest) {
        let _ = fs::remove_dir_all(&staged);
        return Err(error_after_restore(error.into(), dest, previous.as_deref()))
            .with_context(|| format!("install staged directory {}", dest.display()));
    }
    let _ = remove_displaced(previous.as_deref());
    Ok(backup)
}

pub fn replace_file_with_backup_if_unchanged(
    backup_root: &Path,
    dest_root: &Path,
    dest: &Path,
    expected: Option<&[u8]>,
    content: &[u8],
) -> Result<Option<PathBuf>> {
    let staged = unique_nonexistent_sibling(dest, "new");
    write_atomic(&staged, content)
        .with_context(|| format!("stage replacement for {}", dest.display()))?;
    replace_staged_file_if_unchanged(
        &staged,
        dest,
        expected,
        None,
        Some((backup_root, dest_root)),
    )
}

pub fn replace_file_if_unchanged(
    dest: &Path,
    expected: &[u8],
    content: &[u8],
    permissions: Option<&fs::Permissions>,
) -> Result<()> {
    let staged = unique_nonexistent_sibling(dest, "restore");
    write_atomic(&staged, content)
        .with_context(|| format!("stage replacement for {}", dest.display()))?;
    replace_staged_file_if_unchanged(&staged, dest, Some(expected), permissions, None).map(|_| ())
}

#[cfg(unix)]
pub fn install_staged_executable_if_unchanged(
    staged: &Path,
    dest: &Path,
    expected_sha256: Option<&str>,
) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let staged_parent = staged
        .parent()
        .context("staged installer executable has no parent directory")?
        .canonicalize()
        .with_context(|| {
            format!(
                "resolve staged installer directory for {}",
                staged.display()
            )
        })?;
    let dest_parent = dest
        .parent()
        .context("installer destination has no parent directory")?
        .canonicalize()
        .with_context(|| format!("resolve installer destination for {}", dest.display()))?;
    if staged_parent != dest_parent || staged == dest {
        bail!("staged installer executable must be a sibling of the destination");
    }
    let metadata = fs::symlink_metadata(staged)
        .with_context(|| format!("inspect staged installer executable {}", staged.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!(
            "staged installer executable is not a regular file: {}",
            staged.display()
        );
    }
    if expected_sha256.is_some_and(|hash| !is_sha256(hash)) {
        bail!("installer expected target hash is malformed");
    }
    fs::set_permissions(staged, fs::Permissions::from_mode(0o755))
        .with_context(|| format!("make staged installer executable {}", staged.display()))?;
    let Some(expected_sha256) = expected_sha256 else {
        return install_new_target(staged, dest);
    };

    let previous = displace_existing(dest)?;
    let actual_sha256 = match previous
        .as_deref()
        .map(|previous| require_displaced_file(previous).and_then(|()| hash_path(previous)))
        .transpose()
    {
        Ok(actual_sha256) => actual_sha256,
        Err(error) => {
            return Err(stop_file_replacement(
                staged,
                dest,
                previous.as_deref(),
                error,
            ))
        }
    };
    if actual_sha256.as_deref() != Some(expected_sha256) {
        return Err(stop_file_replacement(
            staged,
            dest,
            previous.as_deref(),
            anyhow::anyhow!("{} changed during installation; stopped", dest.display()),
        ));
    }
    if let Err(error) = rename_noreplace(staged, dest) {
        let _ = fs::remove_file(staged);
        return Err(error_after_restore(error.into(), dest, previous.as_deref()))
            .with_context(|| format!("install staged executable {}", dest.display()));
    }
    let _ = remove_displaced(previous.as_deref());
    Ok(())
}

#[cfg(not(unix))]
pub fn install_staged_executable_if_unchanged(
    _staged: &Path,
    _dest: &Path,
    _expected_sha256: Option<&str>,
) -> Result<()> {
    bail!("safe executable installation requires Unix filesystem primitives")
}

#[cfg(unix)]
pub fn remove_installed_executable_if_unchanged(dest: &Path, expected_sha256: &str) -> Result<()> {
    if !is_sha256(expected_sha256) {
        bail!("installer expected target hash is malformed");
    }
    let previous = displace_existing(dest)?;
    let actual_sha256 = match previous
        .as_deref()
        .map(|previous| require_displaced_file(previous).and_then(|()| hash_path(previous)))
        .transpose()
    {
        Ok(actual_sha256) => actual_sha256,
        Err(error) => return Err(error_after_restore(error, dest, previous.as_deref())),
    };
    if actual_sha256.as_deref() != Some(expected_sha256) {
        return Err(error_after_restore(
            anyhow::anyhow!("{} changed during uninstall; stopped", dest.display()),
            dest,
            previous.as_deref(),
        ));
    }
    match remove_displaced(previous.as_deref()) {
        Ok(()) => Ok(()),
        Err(error) => Err(error_after_restore(error, dest, previous.as_deref())),
    }
}

#[cfg(not(unix))]
pub fn remove_installed_executable_if_unchanged(
    _dest: &Path,
    _expected_sha256: &str,
) -> Result<()> {
    bail!("safe executable removal requires Unix filesystem primitives")
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn replace_staged_file_if_unchanged(
    staged: &Path,
    dest: &Path,
    expected: Option<&[u8]>,
    permissions: Option<&fs::Permissions>,
    backup_location: Option<(&Path, &Path)>,
) -> Result<Option<PathBuf>> {
    if expected.is_none() {
        return install_new_target(staged, dest).map(|()| None);
    }
    let previous = displace_existing(dest)?;
    let actual = match read_displaced_file(previous.as_deref()) {
        Ok(content) => content,
        Err(error) => {
            return Err(stop_file_replacement(
                staged,
                dest,
                previous.as_deref(),
                error.context("read displaced target before file replacement"),
            ));
        }
    };
    if actual.as_deref() != expected {
        return Err(stop_file_replacement(
            staged,
            dest,
            previous.as_deref(),
            anyhow::anyhow!("{} changed after preview; stopped", dest.display()),
        ));
    }
    if let Err(error) = preserve_staged_file_permissions(staged, previous.as_deref(), permissions) {
        return Err(stop_file_replacement(
            staged,
            dest,
            previous.as_deref(),
            error,
        ))
        .with_context(|| format!("preserve permissions for {}", dest.display()));
    }
    let backup = match backup_displaced_if_requested(backup_location, dest, previous.as_deref()) {
        Ok(backup) => backup,
        Err(error) => {
            return Err(stop_file_replacement(
                staged,
                dest,
                previous.as_deref(),
                error,
            ));
        }
    };
    if let Err(error) = rename_noreplace(staged, dest) {
        let _ = fs::remove_file(staged);
        return Err(error_after_restore(error.into(), dest, previous.as_deref()))
            .with_context(|| format!("install staged file {}", dest.display()));
    }
    let _ = remove_displaced(previous.as_deref());
    Ok(backup)
}

fn read_displaced_file(previous: Option<&Path>) -> Result<Option<Vec<u8>>> {
    let Some(previous) = previous else {
        return Ok(None);
    };
    require_displaced_file(previous)?;
    fs::read(previous)
        .with_context(|| format!("read displaced target {}", previous.display()))
        .map(Some)
}

fn preserve_staged_file_permissions(
    staged: &Path,
    previous: Option<&Path>,
    permissions: Option<&fs::Permissions>,
) -> Result<()> {
    let Some(previous) = previous else {
        return Ok(());
    };
    let permissions = match permissions {
        Some(permissions) => permissions.clone(),
        None => fs::metadata(previous)?.permissions(),
    };
    fs::set_permissions(staged, permissions)?;
    Ok(())
}

fn backup_displaced_if_requested(
    backup_location: Option<(&Path, &Path)>,
    dest: &Path,
    previous: Option<&Path>,
) -> Result<Option<PathBuf>> {
    match backup_location {
        Some((backup_root, dest_root)) => backup_displaced(backup_root, dest_root, dest, previous),
        None => Ok(None),
    }
}

fn stop_file_replacement(
    staged: &Path,
    dest: &Path,
    previous: Option<&Path>,
    error: anyhow::Error,
) -> anyhow::Error {
    let _ = fs::remove_file(staged);
    error_after_restore(error, dest, previous)
}

fn displace_existing(dest: &Path) -> Result<Option<PathBuf>> {
    loop {
        let previous = unique_nonexistent_sibling(dest, "old");
        match rename_noreplace(dest, &previous) {
            Ok(()) => return Ok(Some(previous)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("stage existing target {}", dest.display()));
            }
        }
    }
}

fn restore_displaced(dest: &Path, previous: Option<&Path>) -> Result<()> {
    let Some(previous) = previous else {
        return Ok(());
    };
    rename_noreplace(previous, dest).with_context(|| {
        format!(
            "restore target {}; the displaced copy remains at {}",
            dest.display(),
            previous.display()
        )
    })
}

fn error_after_restore(
    error: anyhow::Error,
    dest: &Path,
    previous: Option<&Path>,
) -> anyhow::Error {
    if previous.is_none() {
        return error.context(format!("target {} remained missing", dest.display()));
    }
    match restore_displaced(dest, previous) {
        Ok(()) => anyhow::anyhow!("{error:#}; restored target {}", dest.display()),
        Err(restore_error) => anyhow::anyhow!(
            "{error:#}; a concurrent target was preserved at {}; the displaced target was preserved for recovery because it could not be restored: {restore_error:#}",
            dest.display()
        ),
    }
}

fn install_new_target(staged: &Path, dest: &Path) -> Result<()> {
    if let Err(error) = rename_noreplace(staged, dest) {
        let _ = remove_path(staged);
        if error.kind() == io::ErrorKind::AlreadyExists {
            bail!(
                "{} appeared after preview; preserved it and stopped",
                dest.display()
            );
        }
        return Err(error).with_context(|| format!("install new target {}", dest.display()));
    }
    Ok(())
}

fn require_displaced_file(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect displaced target {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("refusing to replace non-regular file {}", path.display());
    }
    Ok(())
}

fn require_displaced_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect displaced target {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("refusing to replace non-directory {}", path.display());
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn rename_noreplace(source: &Path, destination: &Path) -> io::Result<()> {
    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "source path contains NUL"))?;
    let destination = CString::new(destination.as_os_str().as_bytes()).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "destination path contains NUL")
    })?;
    let result = rename_noreplace_platform(&source, &destination);
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
fn rename_noreplace_platform(source: &CString, destination: &CString) -> libc::c_long {
    // SAFETY: Both pointers come from live CStrings. renameat2 does not retain them.
    unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    }
}

#[cfg(target_os = "macos")]
fn rename_noreplace_platform(source: &CString, destination: &CString) -> libc::c_long {
    // SAFETY: Both pointers come from live CStrings. renamex_np does not retain them.
    unsafe { libc::renamex_np(source.as_ptr(), destination.as_ptr(), libc::RENAME_EXCL).into() }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn rename_noreplace(_source: &Path, _destination: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "safe replacement requires atomic no-replace rename support",
    ))
}

fn remove_displaced(previous: Option<&Path>) -> Result<()> {
    let Some(previous) = previous else {
        return Ok(());
    };
    remove_path(previous).with_context(|| format!("remove displaced copy {}", previous.display()))
}

fn backup_displaced(
    backup_root: &Path,
    dest_root: &Path,
    dest: &Path,
    previous: Option<&Path>,
) -> Result<Option<PathBuf>> {
    let Some(previous) = previous else {
        return Ok(None);
    };
    let backup = backup_path(backup_root, dest_root, dest);
    if let Some(parent) = backup.parent() {
        ensure_dir(parent)?;
    }
    let metadata = fs::symlink_metadata(previous)
        .with_context(|| format!("inspect displaced target {}", previous.display()))?;
    if metadata.is_dir() {
        copy_dir(previous, &backup)?;
    } else if metadata.is_file() {
        fs::copy(previous, &backup)
            .with_context(|| format!("backup {} to {}", dest.display(), backup.display()))?;
    } else {
        bail!(
            "displaced target is not a regular file or directory: {}",
            previous.display()
        );
    }
    Ok(Some(backup))
}

/// Restores a copied backup only when the destination still contains the
/// content installed by the caller.
pub fn restore_backup_atomically_if_unchanged(
    backup: &Path,
    dest: &Path,
    expected_sha256: &str,
) -> Result<()> {
    let metadata = fs::symlink_metadata(backup)
        .with_context(|| format!("inspect backup {}", backup.display()))?;
    if metadata.file_type().is_symlink() {
        anyhow::bail!("refusing to restore symlinked backup {}", backup.display());
    }

    if !metadata.is_file() && !metadata.is_dir() {
        anyhow::bail!("backup is not a file or directory: {}", backup.display());
    }

    let staged = unique_nonexistent_sibling(dest, "restore");
    let stage_result = if metadata.is_dir() {
        copy_dir(backup, &staged)
    } else {
        fs::copy(backup, &staged)
            .with_context(|| format!("stage file backup {}", backup.display()))
            .and_then(|_| {
                OpenOptions::new()
                    .write(true)
                    .open(&staged)
                    .with_context(|| format!("open staged backup {}", staged.display()))?
                    .sync_all()
                    .with_context(|| format!("sync staged backup {}", staged.display()))
            })
    };
    if let Err(error) = stage_result {
        let _ = remove_path(&staged);
        return Err(error).with_context(|| format!("stage restore for {}", dest.display()));
    }

    let displaced = match displace_existing(dest) {
        Ok(displaced) => displaced,
        Err(error) => {
            let _ = remove_path(&staged);
            return Err(error);
        }
    };
    let actual_sha256 = match displaced.as_deref().map(hash_regular_path).transpose() {
        Ok(actual_sha256) => actual_sha256,
        Err(error) => {
            let _ = remove_path(&staged);
            return Err(error_after_restore(error, dest, displaced.as_deref()));
        }
    };
    if actual_sha256.as_deref() != Some(expected_sha256) {
        let _ = remove_path(&staged);
        return Err(error_after_restore(
            anyhow::anyhow!("{} changed before rollback; stopped", dest.display()),
            dest,
            displaced.as_deref(),
        ));
    }
    if let Err(error) = rename_noreplace(&staged, dest) {
        let _ = remove_path(&staged);
        return Err(error_after_restore(
            error.into(),
            dest,
            displaced.as_deref(),
        ))
        .with_context(|| format!("install staged rollback for {}", dest.display()));
    }

    let _ = remove_displaced(displaced.as_deref());
    Ok(())
}

/// Removes a managed target only when it still has the installed content.
pub fn remove_target_if_unchanged(dest: &Path, expected_sha256: &str) -> Result<()> {
    let displaced = displace_existing(dest)?;
    let actual_sha256 = match displaced.as_deref().map(hash_regular_path).transpose() {
        Ok(actual_sha256) => actual_sha256,
        Err(error) => return Err(error_after_restore(error, dest, displaced.as_deref())),
    };
    if actual_sha256.as_deref() != Some(expected_sha256) {
        return Err(error_after_restore(
            anyhow::anyhow!("{} changed before removal; stopped", dest.display()),
            dest,
            displaced.as_deref(),
        ));
    }
    match remove_displaced(displaced.as_deref()) {
        Ok(()) => Ok(()),
        Err(error) => Err(error_after_restore(error, dest, displaced.as_deref())),
    }
}

/// Removes a regular file only when its bytes still match the caller's snapshot.
pub fn remove_file_if_unchanged(dest: &Path, expected: &[u8]) -> Result<()> {
    let displaced = displace_existing(dest)?;
    let actual = match read_displaced_file(displaced.as_deref()) {
        Ok(actual) => actual,
        Err(error) => return Err(error_after_restore(error, dest, displaced.as_deref())),
    };
    if actual.as_deref() != Some(expected) {
        return Err(error_after_restore(
            anyhow::anyhow!("{} changed before removal; stopped", dest.display()),
            dest,
            displaced.as_deref(),
        ));
    }
    match remove_displaced(displaced.as_deref()) {
        Ok(()) => Ok(()),
        Err(error) => Err(error_after_restore(error, dest, displaced.as_deref())),
    }
}

fn hash_regular_path(path: &Path) -> Result<String> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect displaced target {}", path.display()))?;
    if metadata.file_type().is_symlink() || (!metadata.is_file() && !metadata.is_dir()) {
        bail!("refusing to replace non-regular target {}", path.display());
    }
    hash_path(path)
}

fn remove_path(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path).with_context(|| format!("remove directory {}", path.display()))
    } else {
        fs::remove_file(path).with_context(|| format!("remove file {}", path.display()))
    }
}

pub fn read_to_string_if_exists(path: &Path) -> Result<Option<String>> {
    if path.exists() {
        Ok(Some(
            fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?,
        ))
    } else {
        Ok(None)
    }
}

pub fn hash_path(path: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    if path.is_file() {
        hasher.update(fs::read(path)?);
    } else {
        let path = resolve_root_dir(path)?;
        let mut entries = Vec::new();
        for entry in WalkDir::new(&path)
            .follow_links(false)
            .into_iter()
            .filter_entry(|entry| !should_ignore(entry.path()))
        {
            let entry = entry?;
            let entry_path = entry.path();
            let rel = entry_path.strip_prefix(&path)?;
            if rel.as_os_str().is_empty() {
                continue;
            }
            entries.push(rel.to_path_buf());
        }
        entries.sort();
        for rel in entries {
            let full = path.join(&rel);
            hasher.update(rel.to_string_lossy().as_bytes());
            if full.is_file() {
                hasher.update(fs::read(full)?);
            } else if full.is_symlink() {
                hasher.update(b"symlink:");
                hasher.update(fs::read_link(full)?.to_string_lossy().as_bytes());
            } else if full.is_dir() {
                hasher.update(b"dir");
            }
        }
    }
    Ok(format!("{:x}", hasher.finalize()))
}

pub fn hash_bytes(content: &[u8]) -> String {
    format!("{:x}", Sha256::digest(content))
}

pub fn path_content_equal(a: &Path, b: &Path) -> Result<bool> {
    if !a.exists() || !b.exists() {
        return Ok(false);
    }
    Ok(hash_path(a)? == hash_path(b)?)
}

pub fn should_ignore(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some(".DS_Store" | ".git")
    )
}

fn resolve_root_dir(path: &Path) -> Result<PathBuf> {
    if path.is_symlink() && path.is_dir() {
        fs::canonicalize(path).with_context(|| format!("resolve symlink {}", path.display()))
    } else {
        Ok(path.to_path_buf())
    }
}

fn unique_nonexistent_sibling(path: &Path, label: &str) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("agent-sync");
    loop {
        let sequence = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(
            ".{name}.agent-sync-{label}-{}-{sequence}",
            std::process::id()
        ));
        if !candidate.exists() {
            return candidate;
        }
    }
}

pub fn list_named_skill_dirs(root: &Path) -> Result<Vec<(String, PathBuf)>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(root).with_context(|| format!("read {}", root.display()))? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() && !path.is_symlink() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        if path.join("SKILL.md").exists() {
            out.push((name, path));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_secret_scan_handles_minified_data_without_flagging_prose_or_references() {
        assert!(raw_secret_reason(r#"{"safe":"value","apiKey":"literal-secret-value"}"#).is_some());
        assert!(raw_secret_reason("Token: authentication token used by the service\n").is_none());
        assert!(raw_secret_reason("API_KEY=${SERVICE_API_KEY}\n").is_none());
        assert!(raw_secret_reason("API_KEY=$SERVICE_API_KEY\n").is_none());
        assert!(raw_secret_reason("apiKey = process.env.SERVICE_API_KEY\n").is_none());
    }

    #[test]
    fn known_token_prefixes_are_not_mistaken_for_environment_names() {
        for token in [
            "ghp_12345678901234567890",
            "github_pat_12345678901234567890",
            "npm_12345678901234567890",
        ] {
            assert!(!safe_secret_reference_or_placeholder(token), "{token}");
            assert_eq!(raw_secret_reason(token), Some("credential-like token"));
        }
        assert!(safe_secret_reference_or_placeholder("${GITHUB_TOKEN}"));
        assert!(safe_secret_reference_or_placeholder("$GITHUB_TOKEN"));
        assert!(safe_secret_reference_or_placeholder(
            "process.env.GITHUB_TOKEN"
        ));
    }

    #[test]
    fn url_query_scan_allows_environment_templates_but_rejects_literals() {
        assert!(
            raw_secret_reason("fetch(`https://mainnet.example/?api-key=${API_KEY}`, {").is_none()
        );
        assert!(raw_secret_reason("`POST /v0/transactions/?api-key=KEY`").is_none());
        assert!(raw_secret_reason("https://mainnet.example/?api-key=SECRET").is_none());
        assert!(raw_secret_reason(
            "https://api.example/${subpath}?api-key=${SERVICE_API_KEY}${queryString}"
        )
        .is_none());
        assert_eq!(
            raw_secret_reason("https://mainnet.example/?api-key=literal-secret-value"),
            Some("literal credential in a URL query")
        );
        assert!(raw_secret_reason("https://api.example/?api-key=${env.SERVICE_API_KEY}").is_none());
        assert_eq!(
            raw_secret_reason("POST /v0/transactions/?api-key=literal-secret-value"),
            Some("literal credential in a URL query")
        );
        assert_eq!(
            raw_secret_reason("https://mainnet.example/?api-key=${API_KEY}literal-secret-value"),
            Some("literal credential in a URL query")
        );
    }

    #[test]
    fn assignment_scan_distinguishes_types_documentation_and_literals() {
        assert!(shell_command_substitution_reference(
            r#"$({ RELAY_JWT_CACHE_CONSUMER=relay-curl "$dev_cache_helper" emit; } 2>/dev/null)"#
        ));
        assert!(safe_secret_reference_or_placeholder(
            r#""$({ RELAY_JWT_CACHE_CONSUMER=relay-curl "$dev_cache_helper" emit; } 2>/dev/null)""#
        ));
        for declaration in [
            "token: Option<String>,",
            "function connect(apiKey: string) {",
            "password: String,",
            "api_key: Optional[str]",
            "Token: authentication token used by the service",
            "| **Authorization**: RBAC/ABAC properly enforced | /10 | |",
            "collection: { key: collectionMint, verified: false },",
            "accessKey:U",
            "C.tokens?o.push(...C.tokens):C.token&&o.push(C.token)",
            "token:pi(\"keep\",t)",
            "if(t===\".\")return{token:ie(\"dot\",t)}",
            "token:Me(\"capturing\",t,{...t!==\"(\"&&{name:t.slice(3,-1)}})",
            "token:ie(\"newline\",t,{negate:B===\"N\"})",
            "token:Me(t[2]===\"<\"?\"lookbehind\":\"lookahead\",t,{negate:t.endsWith(\"!\")})",
            "use mollusk_svm_programs_token::token;",
            "using (auth.uid() = user_id);  -- auth.uid() called per row!",
            "using ((select auth.uid()) = user_id);  -- Called once, cached",
            "**Cookie caching:**",
            "1. **No auth**: Tells the user how to get started (set key or sign up)",
            "2. **API key only** (no JWT): Confirms auth but can't show credit usage — suggests calling `agenticSignup` to detect existing account",
            "3. **Full JWT session**: Shows plan, rate limits, credit usage breakdown (API/RPC/webhooks/overage), billing cycle with days remaining, and burn-rate projections with warnings",
            "- **API key**: saved to shared config path (accessible by both MCP and CLI)",
            "- **Keypair**: saved to `~/.helius-cli/keypair.json`",
            "- **JWT**: saved to shared config for authenticated session features",
            "# NEXT_PUBLIC_HELIUS_API_KEY=xxx  ← DO NOT DO THIS",
            "isLoggedInCache = document.cookie.includes('auth=')",
            "const accessToken = jwt.sign({ address }, JWT_SECRET, { expiresIn: \"24h\" });",
            "password: z.string().min(8, 'Password must be at least 8 characters'),",
            "secretKey: keypair.secretKey,",
            "{scheme}://phantom-auth-callback?wallet_id=...&session_id=...",
            "token_account_state: true",
            "\"token_program\": \"TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA\",",
            "\"associated_token_address\": \"H7iLu4DPFpzEx1AGN8BCN7Qg966YFndt781p6ukhgki9\",",
            "\"price_per_token\": 56.47,",
            "Response includes: `token_info` (for fungibles: balance, decimals, price_info).",
            "need basic account/program monitoring",
            "const helius = createHelius({ apiKey: \"apiKey\" })",
            "const HELIUS_API_KEY = process.env.HELIUS_API_KEY!;",
            "headers: { Authorization: `Bearer ${globalThis.relayAccessTokens.dev}` },",
            "| `admin chains` | GET | `https://api.internal.relay.link/admin/chains` | `Authorization: Bearer $RELAY_BEARER` |",
            "| `refund` | POST | `https://api.internal.relay.link/admin/refund` | `Authorization: Bearer $RELAY_BEARER`; money movement |",
            "| `hub balance` | GET | `http://relay-protocol-hub.platform.svc.cluster.local/queries/balances/{solver}/v1` | `x-api-key: $RELAY_HUB_API_KEY`; internal network only |",
            "| `localhost/chains/update` | POST | `http://localhost:3001/chains/update` | `Authorization: Bearer $RELAY_BEARER`; `x-admin-api-key: $RELAY_ADMIN_API_KEY` |",
            r#"curl_args+=(-H "Authorization: Bearer \$${token_env}")"#,
            "-H \"x-api-key: $JUPITER_API_KEY\" \\",
            "auth=\"auto\"",
            "auth=\"yes\"",
            "auth=\"no\"",
            "if [[ \"$auth\" == \"auto\" ]]; then",
            r#"token="${!token_env:-}""#,
            r#"token="$({ RELAY_JWT_CACHE_CONSUMER=relay-curl "$dev_cache_helper" emit; } 2>/dev/null)""#,
            r#"token="$({ RELAY_JWT_CACHE_CONSUMER=relay-curl "$dev_cache_helper" emit-for-relay-curl; } 2>/dev/null)""#,
            "psql 'host=xxx.example port=6432 password=pscale_pw_xxx dbname=mydb'",
            "request an API key at: `https://pond.example/build/api-key`",
            "* API Key: `pond.example/build/api-key`",
            "- **\"onboarding\" / \"API key\" / \"setup\"** — Account setup: `references/onboarding.md`",
            "When access has downstream work, state these as separate authorization boundaries: `VPN or JWT access does not authorize the downstream action.`",
            "Use exactly one URL-encoded query: `integratorId=<exact-id>` when the ID is known.",
            "Output a sanitized copy with exactly `x-api-key: <DEV_API_KEY>`. Include only nonsecret headers.",
            "- For file paths (e.g., `src/auth/login.ts`): search for `\"filePath\"` matches",
            "- For function notation (e.g., `src/auth/login.ts:verifyToken`): search for the function name in `\"name\"` fields filtered by the file path",
        ] {
            let reason = raw_secret_reason(declaration);
            let ranges = secret_assignment_ranges(declaration)
                .into_iter()
                .map(|range| declaration[range].to_string())
                .collect::<Vec<_>>();
            assert!(
                reason.is_none(),
                "{declaration}: {reason:?}, assignment values: {ranges:?}"
            );
        }
        for literal in [
            "password: correct horse battery staple",
            "password: the correct horse battery staple",
            "API_TOKEN=PRODUCTION_SECRET_VALUE_12345",
            "github_token=PRODUCTION_SECRET_VALUE_12345",
            "| API_KEY: real-secret-value | /10 |",
            "url=\"https://example.invalid\"; API_TOKEN=\"real-secret-value\"",
            "apiToken = String(\"real-secret-value\")",
            "apiToken = derive(prefix, \"real-secret-value\")",
            "apiToken = derive(prefix, \"correcthorsebatterystaple\")",
            "apiToken = derive(prefix, \"abcdefghijklmnop\")",
            "apiToken = derive(prefix, \"abcdefghijkl/mnopqrstuvwx\")",
            "token:String(\"real-secret-value\")",
            "token: real-secret-value",
            "token:pi(\"keep\",\"real-secret-value\")",
            "apiToken:Me(\"real-secret-value\")",
            "API_KEY = real-secret-value!",
            "psql 'password=pscale_pw_livevalue dbname=mydb'",
            "API_TOKEN: `the real secret value`",
            "Set the production API key: `real-secret-value`",
            "apiKey?: real-secret-value",
            "if [[ \"$auth\" == \"real-secret-value\" ]]; then",
            "auth.uid() = real-secret-value",
            "using (auth.uid() = \"real-secret-value\");",
            "using ((select auth.uid()) = \"real-secret-value\");",
            "**Cookie: real-secret-value**",
            "Token: this is the correct horse battery staple",
            "API_KEY=real-secret-value ← DO NOT COMMIT",
            "API_KEY=xxx ← DO NOT COMMIT real-secret-value",
            "`API_KEY`: real-secret-value",
            "| `Authorization: Bearer $RELAY_BEARER`; real-secret-value |",
            "isLoggedInCache = document.cookie.includes('auth=real-secret-value')",
            "const x = condition ? { apiKey: \"real-secret-value\" } : other;",
            r#"token="${!token_env:-real-secret-value}""#,
            r#"token="$(printf real-secret-value)""#,
            r#"token="$(printf "$prefix"; printf real-secret-value)""#,
            r#"token="$(printf $prefix-real-secret-value)""#,
            r#"token="$(REAL=real-secret-value; printf "$REAL")""#,
            r#"token="$(printf "$prefix"; printf emit-real-secret-value)""#,
        ] {
            assert_eq!(
                raw_secret_reason(literal),
                Some("literal value assigned to a sensitive key"),
                "{literal}"
            );
        }
        assert_eq!(
            raw_secret_reason("Authorization: `Bearer real-secret-value`"),
            Some("literal authorization value")
        );
    }

    #[test]
    fn redaction_replaces_only_high_confidence_secret_values() {
        let token = "ghp_12345678901234567890";
        let input = format!(
            "token: Option<String>,\npassword: correct horse battery staple\napi_key = \"{token}\"\nToken: authentication token used by the service\nUse {token} here.\nAPI_KEY=${{SERVICE_API_KEY}}\n"
        );

        let (redacted, count) = redact_known_secrets(&input);

        assert_eq!(count, 3);
        assert!(redacted.contains("token: Option<String>,"));
        assert!(redacted.contains("password: [REDACTED]"));
        assert!(redacted.contains("api_key = \"[REDACTED]\""));
        assert!(redacted.contains("Token: authentication token used by the service"));
        assert!(redacted.contains("Use [REDACTED] here."));
        assert!(redacted.contains("API_KEY=${SERVICE_API_KEY}"));
        assert!(!redacted.contains(token));
    }

    #[test]
    fn redaction_covers_url_authorization_and_private_key_material() {
        let url_password = "url-password-value";
        let bearer = "opaqueBearerValue12345";
        let basic = "dXNlcjpwYXNzd29yZA==";
        let private_key_body = "MIIEvQIBADANBgkqhkiG9w0BAQEFAASC";
        let input = format!(
            "Endpoint: https://service:{url_password}@example.invalid/mcp\nAuthorization: Bearer {bearer}\nProxy-Authorization: Basic {basic}\nBefore key\n-----BEGIN PRIVATE KEY-----\n{private_key_body}\n-----END PRIVATE KEY-----\nAfter key\n"
        );
        assert!(raw_secret_reason(&input).is_some());
        assert!(
            raw_secret_reason("postgresql://<user>:<password>@<host>.example:5432/<database>")
                .is_none()
        );

        let (redacted, count) = redact_known_secrets(&input);

        assert_eq!(count, 4);
        for secret in [url_password, bearer, basic, private_key_body] {
            assert!(!redacted.contains(secret), "secret survived: {secret}");
        }
        assert!(redacted.contains("https://[REDACTED]@example.invalid/mcp"));
        assert!(redacted.contains("Authorization: Bearer [REDACTED]"));
        assert!(redacted.contains("Proxy-Authorization: Basic [REDACTED]"));
        assert!(redacted.contains("Before key\n[REDACTED]\n[REDACTED]\n[REDACTED]\nAfter key"));
        assert_eq!(redacted.matches('\n').count(), input.matches('\n').count());
        assert_eq!(raw_secret_reason(&redacted), None, "{redacted}");
    }

    #[test]
    fn authorization_redaction_preserves_prose_and_explicit_references() {
        let input = "Bearer authentication token used by the service\nAuthorization: Bearer ${SERVICE_TOKEN}\nAuthorization: Basic $BASIC_AUTH\n";

        let (redacted, count) = redact_known_secrets(input);

        assert_eq!(count, 0);
        assert_eq!(redacted, input);
        assert_eq!(raw_secret_reason(&redacted), None);
    }

    #[test]
    fn binary_credential_containers_are_rejected_selectively() {
        assert!(likely_binary_credential_container(
            Path::new("credential-cache.bin"),
            &[0xff, 0x00]
        ));
        assert!(likely_binary_credential_container(
            Path::new("vault.kdbx"),
            &[0x00]
        ));
        assert!(likely_binary_credential_container(
            Path::new("blob.bin"),
            &[0x03, 0xd9, 0xa2, 0x9a, 0x67, 0xfb, 0x4b, 0xb5]
        ));
        assert!(!likely_binary_credential_container(
            Path::new("logo.png"),
            &[0xff, 0x00]
        ));
    }

    #[test]
    fn external_backup_paths_stay_under_the_backup_root() {
        let backup_root = Path::new("/tmp/backups/run");
        let backup = backup_path(
            backup_root,
            Path::new("/Users/example"),
            Path::new("/private/config.toml"),
        );
        assert!(backup.starts_with(backup_root));
        assert_ne!(backup, Path::new("/private/config.toml"));
        assert_eq!(backup.file_name().unwrap(), "config.toml");
    }

    #[test]
    fn checked_file_add_preserves_target_that_appeared_after_preview() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("config.json");
        let backup_root = temp.path().join("backups");
        fs::write(&destination, b"cursor-owned").unwrap();

        let error = replace_file_with_backup_if_unchanged(
            &backup_root,
            temp.path(),
            &destination,
            None,
            b"agent-sync",
        )
        .unwrap_err();

        assert!(error.to_string().contains("appeared after preview"));
        assert_eq!(fs::read(&destination).unwrap(), b"cursor-owned");
    }

    #[test]
    fn checked_file_update_restores_target_that_changed_after_preview() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("config.json");
        let backup_root = temp.path().join("backups");
        fs::write(&destination, b"previewed").unwrap();
        let expected = fs::read(&destination).unwrap();
        fs::write(&destination, b"newer-user-content").unwrap();

        let error = replace_file_with_backup_if_unchanged(
            &backup_root,
            temp.path(),
            &destination,
            Some(&expected),
            b"agent-sync",
        )
        .unwrap_err();

        assert!(error.to_string().contains("changed after preview"));
        assert_eq!(fs::read(&destination).unwrap(), b"newer-user-content");
    }

    #[test]
    fn checked_directory_add_preserves_target_that_appeared_after_preview() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("skill");
        let backup_root = temp.path().join("backups");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("SKILL.md"), b"agent-sync").unwrap();
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("SKILL.md"), b"cursor-owned").unwrap();

        let error = replace_dir_with_backup_if_unchanged(
            &backup_root,
            temp.path(),
            &source,
            &destination,
            None,
        )
        .unwrap_err();

        assert!(error.to_string().contains("appeared after preview"));
        assert_eq!(
            fs::read(destination.join("SKILL.md")).unwrap(),
            b"cursor-owned"
        );
    }

    #[test]
    fn checked_directory_update_restores_target_that_changed_after_preview() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("skill");
        let backup_root = temp.path().join("backups");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("SKILL.md"), b"agent-sync").unwrap();
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("SKILL.md"), b"previewed").unwrap();
        let expected = hash_path(&destination).unwrap();
        fs::write(destination.join("SKILL.md"), b"newer-user-content").unwrap();

        let error = replace_dir_with_backup_if_unchanged(
            &backup_root,
            temp.path(),
            &source,
            &destination,
            Some(&expected),
        )
        .unwrap_err();

        assert!(error.to_string().contains("changed after preview"));
        assert_eq!(
            fs::read(destination.join("SKILL.md")).unwrap(),
            b"newer-user-content"
        );
    }

    #[test]
    fn checked_backup_restore_preserves_a_concurrent_edit() {
        let temp = tempfile::tempdir().unwrap();
        let backup = temp.path().join("backup.json");
        let destination = temp.path().join("config.json");
        fs::write(&backup, b"original").unwrap();
        fs::write(&destination, b"installed").unwrap();
        let installed_sha256 = hash_path(&destination).unwrap();
        fs::write(&destination, b"concurrent-edit").unwrap();

        let error =
            restore_backup_atomically_if_unchanged(&backup, &destination, &installed_sha256)
                .unwrap_err();

        assert!(error.to_string().contains("changed before rollback"));
        assert_eq!(fs::read(&destination).unwrap(), b"concurrent-edit");
    }

    #[test]
    fn checked_removal_preserves_a_concurrent_edit() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("config.json");
        fs::write(&destination, b"installed").unwrap();
        let installed_sha256 = hash_path(&destination).unwrap();
        fs::write(&destination, b"concurrent-edit").unwrap();

        let error = remove_target_if_unchanged(&destination, &installed_sha256).unwrap_err();

        assert!(error.to_string().contains("changed before removal"));
        assert_eq!(fs::read(&destination).unwrap(), b"concurrent-edit");
    }

    #[test]
    fn checked_file_removal_preserves_a_concurrent_edit() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("schedule.plist");
        fs::write(&destination, b"previewed").unwrap();
        fs::write(&destination, b"concurrent-edit").unwrap();

        let error = remove_file_if_unchanged(&destination, b"previewed").unwrap_err();

        assert!(error.to_string().contains("changed before removal"));
        assert_eq!(fs::read(&destination).unwrap(), b"concurrent-edit");
    }

    #[test]
    fn checked_file_removal_refuses_a_directory() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("schedule.plist");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("sentinel"), b"cursor-owned").unwrap();

        let error = remove_file_if_unchanged(&destination, b"previewed").unwrap_err();

        assert!(error.to_string().contains("non-regular file"));
        assert_eq!(
            fs::read(destination.join("sentinel")).unwrap(),
            b"cursor-owned"
        );
    }

    #[cfg(unix)]
    #[test]
    fn executable_install_preserves_target_that_appeared_after_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let staged = temp.path().join(".agent-sync.new");
        let destination = temp.path().join("agent-sync");
        fs::write(&staged, b"release").unwrap();
        fs::write(&destination, b"concurrent").unwrap();

        let error =
            install_staged_executable_if_unchanged(&staged, &destination, None).unwrap_err();

        assert!(error.to_string().contains("appeared after preview"));
        assert_eq!(fs::read(&destination).unwrap(), b"concurrent");
    }

    #[cfg(unix)]
    #[test]
    fn executable_install_preserves_target_that_changed_after_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let staged = temp.path().join(".agent-sync.new");
        let destination = temp.path().join("agent-sync");
        fs::write(&staged, b"release").unwrap();
        fs::write(&destination, b"previewed").unwrap();
        let expected = hash_path(&destination).unwrap();
        fs::write(&destination, b"concurrent").unwrap();

        let error = install_staged_executable_if_unchanged(&staged, &destination, Some(&expected))
            .unwrap_err();

        assert!(error.to_string().contains("changed during installation"));
        assert_eq!(fs::read(&destination).unwrap(), b"concurrent");
    }

    #[cfg(unix)]
    #[test]
    fn executable_install_refuses_a_symlink_that_replaced_the_target() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let staged = temp.path().join(".agent-sync.new");
        let destination = temp.path().join("agent-sync");
        let concurrent = temp.path().join("concurrent");
        fs::write(&staged, b"release").unwrap();
        fs::write(&destination, b"previewed").unwrap();
        let expected = hash_path(&destination).unwrap();
        fs::remove_file(&destination).unwrap();
        fs::write(&concurrent, b"concurrent").unwrap();
        symlink(&concurrent, &destination).unwrap();

        let error = install_staged_executable_if_unchanged(&staged, &destination, Some(&expected))
            .unwrap_err();

        assert!(error.to_string().contains("non-regular file"));
        assert!(fs::symlink_metadata(&destination)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn executable_install_commits_with_executable_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let staged = temp.path().join(".agent-sync.new");
        let destination = temp.path().join("agent-sync");
        fs::write(&staged, b"release").unwrap();
        fs::write(&destination, b"previewed").unwrap();
        let expected = hash_path(&destination).unwrap();

        install_staged_executable_if_unchanged(&staged, &destination, Some(&expected)).unwrap();

        assert_eq!(fs::read(&destination).unwrap(), b"release");
        assert_eq!(
            fs::metadata(&destination).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[cfg(unix)]
    #[test]
    fn executable_uninstall_preserves_target_that_changed_after_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("agent-sync");
        fs::write(&destination, b"previewed").unwrap();
        let expected = hash_path(&destination).unwrap();
        fs::write(&destination, b"concurrent").unwrap();

        let error = remove_installed_executable_if_unchanged(&destination, &expected).unwrap_err();

        assert!(error.to_string().contains("changed during uninstall"));
        assert_eq!(fs::read(&destination).unwrap(), b"concurrent");
    }

    #[cfg(unix)]
    #[test]
    fn executable_uninstall_refuses_a_symlink_that_replaced_the_target() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("agent-sync");
        let concurrent = temp.path().join("concurrent");
        fs::write(&destination, b"previewed").unwrap();
        let expected = hash_path(&destination).unwrap();
        fs::remove_file(&destination).unwrap();
        fs::write(&concurrent, b"concurrent").unwrap();
        symlink(&concurrent, &destination).unwrap();

        let error = remove_installed_executable_if_unchanged(&destination, &expected).unwrap_err();

        assert!(error.to_string().contains("non-regular file"));
        assert!(fs::symlink_metadata(&destination)
            .unwrap()
            .file_type()
            .is_symlink());
    }
}
