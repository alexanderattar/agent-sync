use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Context, Result};
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

pub fn backup_existing(
    backup_root: &Path,
    dest_root: &Path,
    dest: &Path,
) -> Result<Option<PathBuf>> {
    let metadata = match fs::symlink_metadata(dest) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("inspect {}", dest.display())),
    };
    if metadata.file_type().is_symlink() {
        anyhow::bail!("refusing to replace symlinked path {}", dest.display());
    }
    let backup = backup_path(backup_root, dest_root, dest);
    if let Some(parent) = backup.parent() {
        ensure_dir(parent)?;
    }
    if metadata.is_dir() {
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
    let staged = unique_nonexistent_sibling(dest, "new");
    if let Err(error) = copy_dir(src, &staged) {
        let _ = fs::remove_dir_all(&staged);
        return Err(error).with_context(|| format!("stage replacement for {}", dest.display()));
    }
    let backup = match backup_existing(backup_root, dest_root, dest) {
        Ok(backup) => backup,
        Err(error) => {
            let _ = fs::remove_dir_all(&staged);
            return Err(error);
        }
    };
    let previous = if dest.exists() {
        let previous = unique_nonexistent_sibling(dest, "old");
        if let Err(error) = fs::rename(dest, &previous) {
            let _ = fs::remove_dir_all(&staged);
            return Err(error)
                .with_context(|| format!("stage existing directory {}", dest.display()));
        }
        Some(previous)
    } else {
        None
    };
    if let Err(error) = fs::rename(&staged, dest) {
        let restore = previous.as_ref().map_or(Ok(()), |previous| {
            fs::rename(previous, dest)
                .with_context(|| format!("restore directory {}", dest.display()))
        });
        let _ = fs::remove_dir_all(&staged);
        return match restore {
            Ok(()) => Err(error)
                .with_context(|| format!("install staged directory {}", dest.display())),
            Err(restore_error) => Err(anyhow::anyhow!(
                "install staged directory {} failed: {error}; restore also failed: {restore_error:#}",
                dest.display()
            )),
        };
    }
    if let Some(previous) = previous {
        if previous.is_dir() && !previous.is_symlink() {
            let _ = fs::remove_dir_all(previous);
        } else {
            let _ = fs::remove_file(previous);
        }
    }
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

/// Restores a previously copied backup without exposing a missing or partially
/// copied destination. Callers must verify that the destination still contains
/// the content they installed before invoking this function.
pub fn restore_backup_atomically(backup: &Path, dest: &Path) -> Result<()> {
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

    let displaced = unique_nonexistent_sibling(dest, "rollback");
    if let Err(error) = fs::rename(dest, &displaced) {
        let _ = remove_path(&staged);
        return Err(error).with_context(|| format!("stage installed path {}", dest.display()));
    }
    if let Err(error) = fs::rename(&staged, dest) {
        let restore = fs::rename(&displaced, dest)
            .with_context(|| format!("restore installed path {}", dest.display()));
        let _ = remove_path(&staged);
        return match restore {
            Ok(()) => Err(error).with_context(|| {
                format!("install staged rollback for {}", dest.display())
            }),
            Err(restore_error) => Err(anyhow::anyhow!(
                "install staged rollback for {} failed: {error}; restoring the installed path also failed: {restore_error:#}",
                dest.display()
            )),
        };
    }

    remove_path(&displaced)
        .with_context(|| format!("remove displaced path {}", displaced.display()))?;
    Ok(())
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
}
