//! Talks to the Kairo server (account endpoints) and remembers the signed-in session.
//!
//! Everything here is blocking; call it from `cx.background_executor()`.

use std::{path::PathBuf, time::Duration};

use serde::{Deserialize, Serialize};
use serde_json::json;

pub const DEFAULT_SERVER: &str = "http://127.0.0.1:8080";
const KEYRING_SERVICE: &str = "kairo";

#[derive(Clone, Debug)]
pub struct Session {
    pub server: String,
    pub email: String,
    pub token: String,
    /// Read-only token for the `.ics` feed URL.
    pub feed_token: String,
}

#[derive(Deserialize)]
struct AuthResponse {
    token: String,
    email: String,
    feed_token: String,
}

#[derive(Deserialize)]
struct ErrorBody {
    error: String,
    #[serde(default)]
    retry_after: Option<u64>,
}

pub enum Validity {
    Valid,
    /// The server no longer knows this token (expired, revoked, account deleted).
    Rejected,
    /// Server unreachable: stay signed in and try again later.
    Offline,
}

pub(crate) fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(10)))
        .http_status_as_error(false)
        .build()
        .into()
}

pub fn normalize_server(raw: &str) -> Result<String, String> {
    let raw = raw.trim().trim_end_matches('/');
    if raw.is_empty() {
        return Err("Bitte eine Serveradresse eingeben.".into());
    }
    let url = if raw.contains("://") {
        raw.to_string()
    } else if raw.starts_with("localhost") || raw.starts_with("127.0.0.1") || raw.starts_with("[::1]") {
        format!("http://{raw}")
    } else {
        format!("https://{raw}")
    };
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err("Die Serveradresse muss mit http:// oder https:// beginnen.".into());
    }
    Ok(url)
}

fn error_message(status: u16, body: Option<ErrorBody>) -> String {
    match status {
        401 => "E-Mail-Adresse oder Passwort ist falsch.".into(),
        403 => "Registrierung nicht möglich (geschlossen oder Einladungscode falsch).".into(),
        409 => "Diese E-Mail-Adresse ist bereits registriert.".into(),
        429 => {
            let minutes = body.and_then(|b| b.retry_after).map_or(15, |s| s.div_ceil(60).max(1));
            format!("Zu viele Versuche. Bitte in etwa {minutes} Min. erneut versuchen.")
        }
        400 => body.map_or_else(|| "Ungültige Eingabe.".into(), |b| format!("Ungültige Eingabe: {}", b.error)),
        _ => format!("Serverfehler (HTTP {status})."),
    }
}

fn connection_error(err: ureq::Error) -> String {
    format!("Server nicht erreichbar: {err}")
}

pub fn device_name() -> String {
    let host = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_default();
    if host.is_empty() { "Kairo Desktop".into() } else { format!("Kairo Desktop ({host})") }
}

pub fn authenticate(
    server: &str,
    email: &str,
    password: &str,
    register: bool,
    invite_code: &str,
) -> Result<Session, String> {
    let server = normalize_server(server)?;
    let endpoint = if register { "register" } else { "login" };
    let mut body = json!({ "email": email, "password": password, "device": device_name() });
    if register && !invite_code.is_empty() {
        body["invite_code"] = invite_code.into();
    }
    let mut res = agent()
        .post(&format!("{server}/api/v1/auth/{endpoint}"))
        .send_json(&body)
        .map_err(connection_error)?;
    let status = res.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(error_message(status, res.body_mut().read_json().ok()));
    }
    let auth: AuthResponse = res.body_mut().read_json().map_err(|_| "Unerwartete Antwort des Servers.".to_string())?;
    Ok(Session { server, email: auth.email, token: auth.token, feed_token: auth.feed_token })
}

pub fn validate(session: &Session) -> Validity {
    let res = agent()
        .get(&format!("{}/api/v1/auth/me", session.server))
        .header("Authorization", &format!("Bearer {}", session.token))
        .call();
    match res {
        Ok(res) if res.status().is_success() => Validity::Valid,
        Ok(res) if res.status().as_u16() == 401 => Validity::Rejected,
        Ok(_) | Err(_) => Validity::Offline,
    }
}

/// Best effort: tells the server to revoke this device's token.
pub fn logout(session: &Session) {
    let _ = agent()
        .post(&format!("{}/api/v1/auth/logout", session.server))
        .header("Authorization", &format!("Bearer {}", session.token))
        .send_empty();
}

// ---------------------------------------------------------------------------------------
// Persistence: server/email in a config file, the token in the OS keychain
// ---------------------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct Stored {
    server: String,
    #[serde(alias = "username")]
    email: String,
    feed_token: String,
}

fn config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("kairo").join("session.json"))
}

fn keyring_entry(server: &str, email: &str) -> Option<keyring::Entry> {
    keyring::Entry::new(KEYRING_SERVICE, &format!("{email}@{server}")).ok()
}

/// Remembers the session. Returns false if the token could not be put into the keychain
/// (the session then only lives until the app closes).
pub fn save(session: &Session) -> bool {
    let stored = Stored {
        server: session.server.clone(),
        email: session.email.clone(),
        feed_token: session.feed_token.clone(),
    };
    if let Some(path) = config_path() {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_string_pretty(&stored) {
            let _ = std::fs::write(path, json);
        }
    }
    keyring_entry(&session.server, &session.email)
        .is_some_and(|entry| entry.set_password(&session.token).is_ok())
}

pub fn load() -> Option<Session> {
    let stored: Stored = serde_json::from_str(&std::fs::read_to_string(config_path()?).ok()?).ok()?;
    let token = keyring_entry(&stored.server, &stored.email)?.get_password().ok()?;
    Some(Session { server: stored.server, email: stored.email, token, feed_token: stored.feed_token })
}

/// Last used server and email, to prefill the login form.
pub fn last_login() -> Option<(String, String)> {
    let stored: Stored = serde_json::from_str(&std::fs::read_to_string(config_path()?).ok()?).ok()?;
    Some((stored.server, stored.email))
}

/// Forgets the token but keeps server/email for the next login.
pub fn forget_token(session: &Session) {
    if let Some(entry) = keyring_entry(&session.server, &session.email) {
        let _ = entry.delete_credential();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs a running server: `KAIRO_TEST_SERVER=http://127.0.0.1:8099 cargo test -- --ignored`
    #[test]
    #[ignore]
    fn register_login_validate_logout() {
        let server = std::env::var("KAIRO_TEST_SERVER").expect("KAIRO_TEST_SERVER");
        let name = format!("t{}", std::process::id());
        let pw = "correct horse battery";

        let registered = authenticate(&server, &name, pw, true, "").unwrap();
        assert!(authenticate(&server, &name, pw, true, "").unwrap_err().contains("vergeben"));
        assert!(authenticate(&server, &name, "wrong-wrong-wrong", false, "").unwrap_err().contains("falsch"));

        let session = authenticate(&server, &name, pw, false, "").unwrap();
        assert!(matches!(validate(&session), Validity::Valid));
        logout(&session);
        assert!(matches!(validate(&session), Validity::Rejected));
        assert!(matches!(validate(&registered), Validity::Valid));

        let offline = Session { server: "http://127.0.0.1:1".into(), ..registered };
        assert!(matches!(validate(&offline), Validity::Offline));
    }

    #[test]
    fn server_urls_are_normalized() {
        assert_eq!(normalize_server("localhost:8080/").unwrap(), "http://localhost:8080");
        assert_eq!(normalize_server("kairo.example.com").unwrap(), "https://kairo.example.com");
        assert!(normalize_server("  ").is_err());
        assert!(normalize_server("ftp://x").is_err());
    }
}
