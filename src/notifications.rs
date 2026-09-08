//! Best-effort local alerts for failed scheduled runs. Never send error details.

#[cfg(target_os = "macos")]
const FAILURE_SCRIPT: &str = "display notification \"Daily sync needs attention. Ask your agent to check agent-sync health.\" with title \"agent-sync\"";

pub fn scheduled_sync_failure() {
    #[cfg(target_os = "macos")]
    {
        // Integration tests must not display notifications on the developer's Mac.
        if std::env::var_os("AGENT_SYNC_TEST_DISABLE_BACKGROUND").is_some() {
            return;
        }
        let result = std::process::Command::new("/usr/bin/osascript")
            .args(["-e", FAILURE_SCRIPT])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if !result.is_ok_and(|status| status.success()) {
            eprintln!("Warning: could not display the daily-sync notification. Ask your agent to check agent-sync health.");
        }
    }
}
