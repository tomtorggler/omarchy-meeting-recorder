//! Linux Secret Service via libsecret's small CLI. Tokens never enter argv or files.
use super::{Config, Result, auth::Tokens};
use std::io::Write;
use std::process::{Command, Stdio};

fn command(config: &Config, operation: &str) -> Command {
    let mut command = Command::new("timeout");
    command.args(["--kill-after=2", "30", "secret-tool", operation]);
    if operation == "store" {
        command.args(["--label", "Meeting Recorder — Microsoft Calendar"]);
    }
    command.args([
        "--",
        "application",
        crate::APP_ID,
        "purpose",
        "microsoft-calendar",
        "account",
        &config.email,
        "client-id",
        &config.client_id,
        "tenant",
        &config.tenant,
    ]);
    command
        .stderr(Stdio::null())
        .stdout(Stdio::piped())
        .stdin(Stdio::null());
    command
}

const KEYRING_ERROR: &str = "Could not access the desktop keyring. Install libsecret (secret-tool) and unlock your keyring, then try again.";

pub(super) fn load(config: &Config) -> Result<Tokens> {
    let output = command(config, "lookup")
        .output()
        .map_err(|_| KEYRING_ERROR)?;
    if !output.status.success() {
        return Err("Calendar sign-in is unavailable or the keyring is locked. Unlock it or reconnect in Calendar settings.".into());
    }
    decode(&output.stdout)
}

pub(super) fn save(config: &Config, tokens: &Tokens) -> Result<()> {
    // secret-tool accepts at most 8192 bytes. Access tokens can exceed that;
    // retain only the refresh token and keep access tokens in process memory.
    let bytes = encode(tokens)?;
    let mut child = command(config, "store")
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|_| KEYRING_ERROR)?;
    let written = child.stdin.take().unwrap().write_all(&bytes);
    let output = child.wait_with_output().map_err(|_| KEYRING_ERROR)?;
    if written.is_err() || !output.status.success() {
        return Err(KEYRING_ERROR.into());
    }
    Ok(())
}

pub(super) fn clear(config: &Config) -> Result<()> {
    if config.tenant.is_empty() {
        return Ok(());
    }
    let output = command(config, "clear")
        .stderr(Stdio::piped())
        .output()
        .map_err(|_| KEYRING_ERROR)?;
    if !cleared(output.status.code(), &output.stderr) {
        return Err("Calendar is off. Its saved sign-in could not be removed; unlock the keyring and try Disconnect again.".into());
    }
    Ok(())
}

fn encode(tokens: &Tokens) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(&tokens.refresh_token)
        .map_err(|_| "Could not encode sign-in credentials.")?;
    if bytes.len() >= 8192 {
        return Err(
            "The Microsoft refresh token exceeds the desktop keyring tool's input limit.".into(),
        );
    }
    Ok(bytes)
}

fn decode(bytes: &[u8]) -> Result<Tokens> {
    let refresh_token: String = serde_json::from_slice(bytes)
        .map_err(|_| "Reconnect Microsoft Calendar in Calendar settings.")?;
    if refresh_token.is_empty() {
        return Err("Reconnect Microsoft Calendar in Calendar settings.".into());
    }
    Ok(Tokens {
        access_token: String::new(),
        refresh_token,
        expires_at: 0,
    })
}

fn cleared(code: Option<i32>, stderr: &[u8]) -> bool {
    // libsecret returns 1 without an error when no matching item exists.
    code == Some(0) || (code == Some(1) && stderr.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn persists_only_refresh_token_and_requires_refresh_after_restart() {
        let token = Tokens {
            access_token: "access-secret".repeat(2000),
            refresh_token: "refresh-secret".into(),
            expires_at: i64::MAX,
        };
        let bytes = encode(&token).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("access-secret"));
        let restored = decode(&bytes).unwrap();
        assert_eq!(restored.refresh_token, token.refresh_token);
        assert!(restored.access_token.is_empty());
        assert_eq!(restored.expires_at, 0);
    }
    #[test]
    fn clearing_is_idempotent_but_does_not_hide_keyring_failure() {
        assert!(cleared(Some(0), b""));
        assert!(cleared(Some(1), b""));
        assert!(!cleared(Some(1), b"keyring locked"));
        assert!(!cleared(Some(124), b""));
    }
}
