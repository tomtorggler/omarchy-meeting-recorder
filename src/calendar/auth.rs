//! Entra public-client authorization code flow with S256 PKCE and a loopback callback.
use super::{Config, Result, check_cancel, is_guid};
use serde_json::Value;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};
use url::Url;

const LOGIN: &str = "https://login.microsoftonline.com";
const SCOPE: &str = "https://graph.microsoft.com/Calendars.ReadBasic https://graph.microsoft.com/User.Read offline_access";

pub(super) struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: i64,
}

pub(super) fn http() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(25)))
        .max_redirects(0)
        .http_status_as_error(false)
        .build()
        .into()
}

pub(super) fn read_json<T: serde::de::DeserializeOwned>(body: &mut ureq::Body) -> Result<T> {
    let bytes = body
        .read_to_vec()
        .map_err(|_| "Could not read Microsoft response.")?;
    serde_json::from_slice(&bytes).map_err(|_| "Invalid Microsoft response.".into())
}

pub(super) fn discover(config: &Config) -> Result<String> {
    let domain = config
        .email
        .rsplit_once('@')
        .ok_or("Invalid email address.")?
        .1;
    let url = format!("{LOGIN}/{domain}/v2.0/.well-known/openid-configuration");
    let mut response = http()
        .get(&url)
        .call()
        .map_err(|_| "Could not reach Microsoft. Check your connection and try again.")?;
    if !response.status().is_success() {
        return Err("No Entra tenant was found for that email domain. Use your work or school sign-in address.".into());
    }
    let value: Value = read_json(response.body_mut())
        .map_err(|_| "Invalid Microsoft tenant discovery response.")?;
    tenant_from_issuer(value["issuer"].as_str().unwrap_or_default())
}

fn tenant_from_issuer(issuer: &str) -> Result<String> {
    let tenant = issuer
        .strip_prefix(&format!("{LOGIN}/"))
        .and_then(|s| s.strip_suffix("/v2.0"))
        .filter(|s| is_guid(s))
        .ok_or("Microsoft did not resolve that domain to an Entra tenant.")?;
    Ok(tenant.to_owned())
}

fn endpoint(config: &Config, operation: &str) -> Result<String> {
    if !is_guid(&config.tenant) || !is_guid(&config.client_id) {
        return Err("Reconnect Microsoft Calendar to discover its tenant.".into());
    }
    Ok(format!("{LOGIN}/{}/oauth2/v2.0/{operation}", config.tenant))
}

fn random_secret() -> Result<String> {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|_| "Could not create secure sign-in state.")?;
    Ok(base64url(&bytes))
}

fn base64url(bytes: &[u8]) -> String {
    gtk::glib::base64_encode(bytes)
        .trim_end_matches('=')
        .replace('+', "-")
        .replace('/', "_")
}

fn challenge(verifier: &str) -> String {
    let mut digest = gtk::glib::Checksum::new(gtk::glib::ChecksumType::Sha256).unwrap();
    digest.update(verifier.as_bytes());
    base64url(&digest.digest())
}

pub(super) fn sign_in(
    config: &Config,
    cancel: &AtomicBool,
    open_browser: impl FnOnce(String) -> Result<()>,
) -> Result<Tokens> {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .map_err(|_| "Could not open a local sign-in callback.")?;
    listener
        .set_nonblocking(true)
        .map_err(|_| "Could not prepare sign-in callback.")?;
    let port = listener
        .local_addr()
        .map_err(|_| "Could not find sign-in callback port.")?
        .port();
    let redirect = format!("http://localhost:{port}/");
    let state = random_secret()?;
    let verifier = random_secret()?;
    let mut url = Url::parse(&endpoint(config, "authorize")?).unwrap();
    url.query_pairs_mut().extend_pairs([
        ("client_id", config.client_id.as_str()),
        ("response_type", "code"),
        ("redirect_uri", &redirect),
        ("response_mode", "query"),
        ("scope", SCOPE),
        ("state", &state),
        ("code_challenge", &challenge(&verifier)),
        ("code_challenge_method", "S256"),
        ("login_hint", &config.email),
    ]);
    open_browser(url.to_string())?;
    let code = await_callback(&listener, &state, cancel)?;
    check_cancel(cancel)?;
    token_request(
        config,
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", &redirect),
            ("code_verifier", &verifier),
        ],
        None,
    )
}

fn await_callback(listener: &TcpListener, state: &str, cancel: &AtomicBool) -> Result<String> {
    let deadline = Instant::now() + Duration::from_secs(300);
    while Instant::now() < deadline {
        check_cancel(cancel)?;
        match listener.accept() {
            Ok((mut stream, _)) => {
                let response =
                    read_callback(&mut stream).and_then(|request| callback_code(&request, state));
                // Ignore favicon requests, probes, and callbacks for another login attempt.
                let Some(result) = response.ok().flatten() else {
                    reply(
                        &mut stream,
                        "400 Bad Request",
                        "This is not the current Meeting Recorder sign-in.",
                    );
                    continue;
                };
                reply(
                    &mut stream,
                    "200 OK",
                    if result.is_ok() {
                        "Sign-in received. You can close this tab and return to Meeting Recorder."
                    } else {
                        "Sign-in was not completed. Return to Meeting Recorder to try again."
                    },
                );
                return result;
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100))
            }
            Err(_) => return Err("Could not receive the Microsoft sign-in callback.".into()),
        }
    }
    Err("Sign-in timed out. Click Connect to try again.".into())
}

fn read_callback(stream: &mut TcpStream) -> Result<String> {
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .map_err(|_| "Callback unavailable.")?;
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 1024];
    let deadline = Instant::now() + Duration::from_secs(2);
    while bytes.len() < 16_384 && Instant::now() < deadline {
        let n = stream
            .read(&mut chunk)
            .map_err(|_| "Incomplete callback.")?;
        if n == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..n]);
        if bytes.windows(4).any(|w| w == b"\r\n\r\n") {
            return String::from_utf8(bytes).map_err(|_| "Invalid callback.".into());
        }
    }
    Err("Invalid callback.".into())
}

fn callback_code(request: &str, state: &str) -> Result<Option<Result<String>>> {
    let mut parts = request
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace();
    if parts.next() != Some("GET") {
        return Ok(None);
    }
    let target = parts.next().unwrap_or_default();
    if !target.starts_with("/?") {
        return Ok(None);
    }
    let url = Url::parse(&format!("http://localhost{target}")).map_err(|_| "Invalid callback.")?;
    let params: Vec<_> = url.query_pairs().collect();
    let unique = |key: &str| {
        let mut matches = params
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.to_string());
        let value = matches.next();
        if matches.next().is_some() {
            None
        } else {
            value
        }
    };
    if unique("state").as_deref() != Some(state) {
        return Ok(None);
    }
    if let Some(error) = unique("error") {
        return Ok(Some(Err(auth_error(&error))));
    }
    Ok(unique("code").filter(|s| !s.is_empty()).map(Ok))
}

fn reply(stream: &mut TcpStream, status: &str, body: &str) {
    let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nCache-Control: no-store\r\nContent-Security-Policy: default-src 'none'\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
}

pub(super) fn refresh(config: &Config, previous: &Tokens) -> Result<Tokens> {
    token_request(
        config,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", &previous.refresh_token),
        ],
        Some(&previous.refresh_token),
    )
}

fn token_request(
    config: &Config,
    fields: &[(&str, &str)],
    previous_refresh: Option<&str>,
) -> Result<Tokens> {
    let mut form = vec![("client_id", config.client_id.as_str()), ("scope", SCOPE)];
    form.extend_from_slice(fields);
    let mut response = http()
        .post(endpoint(config, "token")?)
        .send_form(form)
        .map_err(|_| "Could not reach Microsoft to complete sign-in.")?;
    let success = response.status().is_success();
    let value: Value =
        read_json(response.body_mut()).map_err(|_| "Invalid Microsoft sign-in response.")?;
    if !success {
        return Err(auth_error(value["error"].as_str().unwrap_or_default()));
    }
    parse_tokens(&value, previous_refresh)
}

fn parse_tokens(value: &Value, previous_refresh: Option<&str>) -> Result<Tokens> {
    let access_token = value["access_token"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("Microsoft returned no access token.")?
        .to_owned();
    let refresh_token = value["refresh_token"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or(previous_refresh)
        .ok_or("Microsoft returned no refresh token. Reconnect and grant offline access.")?
        .to_owned();
    let lifetime = value["expires_in"].as_i64().unwrap_or(0).clamp(0, 86400);
    Ok(Tokens {
        access_token,
        refresh_token,
        expires_at: crate::ipc::now() + lifetime,
    })
}

fn auth_error(code: &str) -> String {
    match code {
        "access_denied" => "Sign-in or calendar consent was declined. Your organization may require administrator approval.",
        "invalid_grant" | "interaction_required" | "login_required" => "Your Microsoft session needs attention. Reconnect in Calendar settings.",
        "invalid_client" | "unauthorized_client" | "invalid_request" => "Check the client ID and Entra app registration: use a Mobile and desktop application with the http://localhost redirect URI.",
        "invalid_scope" => "Add delegated Microsoft Graph Calendars.ReadBasic and User.Read permissions to the Entra app.",
        _ => "Microsoft could not complete sign-in. Check the app registration and your organization's access policy.",
    }.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retains_refresh_token_with_unusual_access_lifetime() {
        for lifetime in [
            serde_json::Value::Null,
            serde_json::json!(-1),
            serde_json::json!(172800),
        ] {
            let token = parse_tokens(&serde_json::json!({"access_token":"access", "refresh_token":"rotated", "expires_in":lifetime}), Some("previous")).unwrap();
            assert_eq!(token.refresh_token, "rotated");
            assert!(token.expires_at <= crate::ipc::now() + 86400);
        }
        let token = parse_tokens(
            &serde_json::json!({"access_token":"access", "expires_in":3600}),
            Some("previous"),
        )
        .unwrap();
        assert_eq!(token.refresh_token, "previous");
    }

    #[test]
    fn pkce_matches_rfc7636_vector() {
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert_eq!(random_secret().unwrap().len(), 43);
    }
    #[test]
    fn callback_requires_matching_unique_state() {
        for request in [
            "GET /favicon.ico HTTP/1.1",
            "GET /?code=secret&state=wrong HTTP/1.1",
            "GET /?code=secret&state=ok&state=ok HTTP/1.1",
            "POST /?code=secret&state=ok HTTP/1.1",
            "GET /?code=a&code=b&state=ok HTTP/1.1",
        ] {
            assert!(callback_code(request, "ok").unwrap().is_none());
        }
        assert_eq!(
            callback_code("GET /?code=a%2Bb&state=ok HTTP/1.1", "ok")
                .unwrap()
                .unwrap()
                .unwrap(),
            "a+b"
        );
        assert!(
            callback_code("GET /?error=access_denied&state=ok HTTP/1.1", "ok")
                .unwrap()
                .unwrap()
                .is_err()
        );
    }
    #[test]
    fn only_accepts_resolved_microsoft_tenant_issuer() {
        let id = "01234567-89ab-cdef-0123-456789abcdef";
        assert_eq!(
            tenant_from_issuer(&format!("{LOGIN}/{id}/v2.0")).unwrap(),
            id
        );
        for issuer in [
            "https://evil.test/tenant/v2.0",
            "https://login.microsoftonline.com/{tenantid}/v2.0",
            "https://login.microsoftonline.com/common/v2.0",
        ] {
            assert!(tenant_from_issuer(issuer).is_err());
        }
    }
}
