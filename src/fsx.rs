use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

pub fn ensure_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("create directory {}", path.display()))
}

pub fn write_atomic(path: &Path, content: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        ensure_dir(parent)?;
    }
    let tmp = path.with_extension(format!(
        "{}tmp",
        path.extension()
            .and_then(|extension| extension.to_str())
            .map(|extension| format!("{extension}."))
            .unwrap_or_default()
    ));
    {
        let mut file = fs::File::create(&tmp)
            .with_context(|| format!("create temp file {}", tmp.display()))?;
        file.write_all(content)
            .with_context(|| format!("write temp file {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("sync temp file {}", tmp.display()))?;
    }
    fs::rename(&tmp, path)
        .with_context(|| format!("rename {} to {}", tmp.display(), path.display()))?;
    Ok(())
}

pub fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    let src = resolve_root_dir(src)?;
    ensure_dir(dst)?;
    for entry in WalkDir::new(&src).follow_links(false) {
        let entry = entry?;
        let path = entry.path();
        if should_ignore(path) {
            if entry.file_type().is_dir() {
                continue;
            }
            continue;
        }
        let rel = path.strip_prefix(&src)?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        let target = dst.join(rel);
        let file_type = entry.file_type();
        if file_type.is_dir() {
            ensure_dir(&target)?;
        } else if file_type.is_file() {
            if let Some(parent) = target.parent() {
                ensure_dir(parent)?;
            }
            fs::copy(path, &target)
                .with_context(|| format!("copy {} to {}", path.display(), target.display()))?;
        } else if file_type.is_symlink() {
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

pub fn backup_path(backup_root: &Path, dest_root: &Path, dest: &Path) -> PathBuf {
    let rel = dest.strip_prefix(dest_root).unwrap_or(dest);
    backup_root.join(rel)
}

pub fn backup_existing(
    backup_root: &Path,
    dest_root: &Path,
    dest: &Path,
) -> Result<Option<PathBuf>> {
    if !dest.exists() {
        return Ok(None);
    }
    let backup = backup_path(backup_root, dest_root, dest);
    if let Some(parent) = backup.parent() {
        ensure_dir(parent)?;
    }
    if dest.is_dir() && !dest.is_symlink() {
        copy_dir(dest, &backup)?;
    } else {
        fs::copy(dest, &backup)
            .with_context(|| format!("backup {} to {}", dest.display(), backup.display()))?;
    }
    Ok(Some(backup))
}

pub fn replace_dir_with_backup(
    backup_root: &Path,
    dest_root: &Path,
    src: &Path,
    dest: &Path,
) -> Result<Option<PathBuf>> {
    let backup = backup_existing(backup_root, dest_root, dest)?;
    if dest.exists() {
        if dest.is_dir() && !dest.is_symlink() {
            fs::remove_dir_all(dest)
                .with_context(|| format!("remove existing directory {}", dest.display()))?;
        } else {
            fs::remove_file(dest)
                .with_context(|| format!("remove existing file {}", dest.display()))?;
        }
    }
    copy_dir(src, dest)?;
    Ok(backup)
}

pub fn replace_file_with_backup(
    backup_root: &Path,
    dest_root: &Path,
    dest: &Path,
    content: &[u8],
) -> Result<Option<PathBuf>> {
    let backup = backup_existing(backup_root, dest_root, dest)?;
    write_atomic(dest, content)?;
    Ok(backup)
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
        for entry in WalkDir::new(&path).follow_links(false) {
            let entry = entry?;
            let entry_path = entry.path();
            if should_ignore(entry_path) {
                continue;
            }
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
