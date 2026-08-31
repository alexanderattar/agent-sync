#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::{symlink, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Output},
};

use sha2::{Digest, Sha256};

fn bin() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_BIN_EXE_agent-sync"));
    if path.is_absolute() {
        path
    } else {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(path)
    }
}

fn installer() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("install.sh")
}

fn write_executable(path: &Path, content: &[u8]) {
    fs::write(path, content).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn sha256(content: &[u8]) -> String {
    format!("{:x}", Sha256::digest(content))
}

fn prepare_release(root: &Path) -> (PathBuf, PathBuf) {
    let package = root.join("package");
    let asset = root.join("agent-sync.tar.gz");
    let checksum = root.join("agent-sync.tar.gz.sha256");
    fs::create_dir(&package).unwrap();
    fs::copy(bin(), package.join("agent-sync")).unwrap();
    let status = Command::new("tar")
        .args(["-C"])
        .arg(&package)
        .args(["-czf"])
        .arg(&asset)
        .arg("./agent-sync")
        .status()
        .unwrap();
    assert!(status.success());
    let asset_hash = sha256(&fs::read(&asset).unwrap());
    fs::write(&checksum, format!("{asset_hash}  agent-sync.tar.gz\n")).unwrap();
    (asset, checksum)
}

fn write_install_mocks(mock_dir: &Path) {
    write_executable(
        &mock_dir.join("curl"),
        br#"#!/bin/sh
output=
url=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --output) output="$2"; shift 2 ;;
    http*) url="$1"; shift ;;
    *) shift ;;
  esac
done
case "$url" in
  *.sha256) /bin/cp "$TEST_CHECKSUM" "$output" ;;
  *) /bin/cp "$TEST_ASSET" "$output" ;;
esac
"#,
    );
    write_executable(
        &mock_dir.join("uname"),
        br#"#!/bin/sh
case "$1" in
  -s) printf '%s\n' Linux ;;
  -m) printf '%s\n' x86_64 ;;
  *) exit 1 ;;
esac
"#,
    );
    write_executable(&mock_dir.join("gh"), b"#!/bin/sh\nexit 1\n");
    write_executable(
        &mock_dir.join("chmod"),
        br#"#!/bin/sh
/bin/chmod "$@"
if [ -n "${TEST_RACE_SYMLINK_TARGET:-}" ]; then
  /bin/mv "$TEST_RACE_SYMLINK_TARGET" "$TEST_RACE_ORIGINAL"
  /bin/ln -s "$TEST_RACE_SYMLINK_SOURCE" "$TEST_RACE_SYMLINK_TARGET"
elif [ -n "${TEST_RACE_TARGET:-}" ]; then
  /bin/cp "$TEST_RACE_REPLACEMENT" "$TEST_RACE_TARGET"
fi
"#,
    );
}

fn run_install(root: &Path, concurrent_target: Option<&[u8]>) -> Output {
    let install_dir = root.join("install");
    let mock_dir = root.join("mock-install-bin");
    fs::create_dir_all(&install_dir).unwrap();
    fs::create_dir_all(&mock_dir).unwrap();
    let (asset, checksum) = prepare_release(root);
    write_install_mocks(&mock_dir);

    let mut command = Command::new("/bin/sh");
    command
        .arg(installer())
        .args(["--version", "v1.2.3", "--install-dir"])
        .arg(&install_dir)
        .env("HOME", root)
        .env("PATH", format!("{}:/usr/bin:/bin", mock_dir.display()))
        .env("TEST_ASSET", &asset)
        .env("TEST_CHECKSUM", &checksum);
    if let Some(concurrent_target) = concurrent_target {
        let replacement = root.join("concurrent-target");
        fs::write(&replacement, concurrent_target).unwrap();
        command
            .env("TEST_RACE_TARGET", install_dir.join("agent-sync"))
            .env("TEST_RACE_REPLACEMENT", replacement);
    }
    command.output().unwrap()
}

enum UninstallRace<'a> {
    Regular(&'a [u8]),
    Symlink(&'a Path),
}

fn non_agent_executable(label: &str) -> Vec<u8> {
    format!("#!/bin/sh\nprintf '%s\\n' {label:?} >> \"$TEST_EXECUTION_MARKER\"\nexit 97\n")
        .into_bytes()
}

fn run_uninstall(root: &Path, initial: &[u8], race: Option<UninstallRace<'_>>) -> Output {
    let install_dir = root.join("install");
    let mock_dir = root.join("mock-uninstall-bin");
    fs::create_dir_all(&install_dir).unwrap();
    fs::create_dir_all(&mock_dir).unwrap();
    write_executable(&install_dir.join("agent-sync"), initial);
    let (asset, checksum) = prepare_release(root);
    write_install_mocks(&mock_dir);

    let mut command = Command::new("/bin/sh");
    command
        .arg(installer())
        .args(["--uninstall", "--version", "v1.2.3", "--install-dir"])
        .arg(&install_dir)
        .env("HOME", root)
        .env("PATH", format!("{}:/usr/bin:/bin", mock_dir.display()))
        .env("TEST_ASSET", &asset)
        .env("TEST_CHECKSUM", &checksum)
        .env("TEST_EXECUTION_MARKER", root.join("target-executed"));
    match race {
        Some(UninstallRace::Regular(replacement)) => {
            let replacement_path = root.join("replacement");
            write_executable(&replacement_path, replacement);
            command
                .env("TEST_RACE_TARGET", install_dir.join("agent-sync"))
                .env("TEST_RACE_REPLACEMENT", replacement_path);
        }
        Some(UninstallRace::Symlink(replacement)) => {
            command
                .env("TEST_RACE_SYMLINK_TARGET", install_dir.join("agent-sync"))
                .env("TEST_RACE_SYMLINK_SOURCE", replacement)
                .env("TEST_RACE_ORIGINAL", root.join("original"));
        }
        None => {}
    }
    command.output().unwrap()
}

#[test]
fn install_commits_the_verified_staged_binary() {
    let temp = tempfile::tempdir().unwrap();
    let output = run_install(temp.path(), None);

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read(temp.path().join("install/agent-sync")).unwrap(),
        fs::read(bin()).unwrap()
    );
}

#[test]
fn install_preserves_a_target_that_appears_before_commit() {
    let temp = tempfile::tempdir().unwrap();
    let concurrent = b"concurrent target";
    let output = run_install(temp.path(), Some(concurrent));

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("appeared after preview"));
    assert_eq!(
        fs::read(temp.path().join("install/agent-sync")).unwrap(),
        concurrent
    );
}

#[test]
fn uninstall_uses_the_checked_removal_command() {
    let temp = tempfile::tempdir().unwrap();
    let initial = non_agent_executable("installed target ran");
    let output = run_uninstall(temp.path(), &initial, None);

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!temp.path().join("install/agent-sync").exists());
    assert!(!temp.path().join("target-executed").exists());
}

#[test]
fn uninstall_preserves_a_regular_file_replaced_after_the_hash_snapshot() {
    let temp = tempfile::tempdir().unwrap();
    let initial = non_agent_executable("installed target ran");
    let replacement = non_agent_executable("replacement target ran");
    let output = run_uninstall(
        temp.path(),
        &initial,
        Some(UninstallRace::Regular(&replacement)),
    );

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("changed during uninstall"));
    assert_eq!(
        fs::read(temp.path().join("install/agent-sync")).unwrap(),
        replacement
    );
    assert!(!temp.path().join("target-executed").exists());
}

#[test]
fn uninstall_preserves_a_symlink_replaced_after_the_hash_snapshot() {
    let temp = tempfile::tempdir().unwrap();
    let initial = non_agent_executable("installed target ran");
    let replacement = temp.path().join("symlink-replacement");
    write_executable(
        &replacement,
        &non_agent_executable("symlink replacement ran"),
    );
    let original_path = temp.path().join("original");
    let output = run_uninstall(
        temp.path(),
        &initial,
        Some(UninstallRace::Symlink(&replacement)),
    );

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("non-regular file"));
    let installed = temp.path().join("install/agent-sync");
    assert!(fs::symlink_metadata(&installed)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(fs::read_link(installed).unwrap(), replacement);
    assert!(original_path.exists());
    assert!(!temp.path().join("target-executed").exists());
}

#[test]
fn initial_symlink_is_refused_without_running_it() {
    let temp = tempfile::tempdir().unwrap();
    let install_dir = temp.path().join("install");
    fs::create_dir(&install_dir).unwrap();
    symlink(bin(), install_dir.join("agent-sync")).unwrap();

    let output = Command::new("/bin/sh")
        .arg(installer())
        .args(["--uninstall", "--install-dir"])
        .arg(&install_dir)
        .env("HOME", temp.path())
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Refusing to remove symlinked path"));
    assert!(fs::symlink_metadata(install_dir.join("agent-sync"))
        .unwrap()
        .file_type()
        .is_symlink());
}
