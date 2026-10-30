//! Google sign-in for a desktop app: browser consent, a loopback redirect and PKCE,
//! then a refresh token kept on disk.
//!
//! The app asks for the `drive.file` scope, so it can only see files it created.

use std::{
    io,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::Mutex,
};

use crate::replicate::ReplicationError;

const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub(crate) const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const DRIVE_FILE_SCOPE: &str = "https://www.googleapis.com/auth/drive.file";
/// Refresh this long before the token runs out, so an upload never starts on a
/// token about to die.
const EXPIRY_MARGIN: Duration = Duration::from_secs(60);
const REFRESH_ATTEMPTS: u32 = 4;

/// What is saved between runs. The file holds a long-lived credential.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DriveCredentials {
    pub client_id: String,
    pub client_secret: Option<String>,
    pub refresh_token: String,
}

impl DriveCredentials {
    pub fn load(path: &Path) -> Result<Self, ReplicationError> {
        let text = std::fs::read_to_string(path).map_err(|err| {
            format!(
                "not signed in to Google Drive ({}: {err}); run `ingot gdrive-login`",
                path.display()
            )
        })?;
        Ok(serde_json::from_str(&text)?)
    }

    /// Readable by this user only, and swapped in whole, so a crash can't leave a
    /// half-written credential.
    pub fn save(&self, path: &Path) -> Result<(), ReplicationError> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut tmp = path.to_path_buf();
        tmp.as_mut_os_string().push(".tmp");
        let mut options = std::fs::File::options();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        io::Write::write_all(
            &mut options.open(&tmp)?,
            serde_json::to_string_pretty(self)?.as_bytes(),
        )?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: u64,
    refresh_token: Option<String>,
}

struct AccessToken {
    value: String,
    expires_at: SystemTime,
}

/// Hands out a valid access token. Concurrent uploads share one refresh.
pub(crate) struct TokenSource {
    credentials: DriveCredentials,
    token_url: String,
    http: reqwest::Client,
    current: Mutex<Option<AccessToken>>,
}

impl TokenSource {
    pub(crate) fn new(
        credentials: DriveCredentials,
        token_url: String,
        http: reqwest::Client,
    ) -> Self {
        TokenSource {
            credentials,
            token_url,
            http,
            current: Mutex::new(None),
        }
    }

    pub(crate) async fn access_token(&self) -> Result<String, ReplicationError> {
        let mut current = self.current.lock().await;
        if let Some(token) = &*current
            && token.expires_at > SystemTime::now() + EXPIRY_MARGIN
        {
            return Ok(token.value.clone());
        }

        let mut form = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", self.credentials.refresh_token.as_str()),
            ("client_id", self.credentials.client_id.as_str()),
        ];
        if let Some(secret) = &self.credentials.client_secret {
            form.push(("client_secret", secret));
        }
        // A network blip here shouldn't fail a file that could upload a second later.
        let mut attempt = 0;
        let response = loop {
            attempt += 1;
            let sent = self.http.post(&self.token_url).form(&form).send().await;
            match sent {
                Ok(response) if !response.status().is_server_error() => break response,
                Err(err) if attempt >= REFRESH_ATTEMPTS => {
                    return Err(format!("could not reach Google to sign in: {err}").into());
                }
                Ok(response) if attempt >= REFRESH_ATTEMPTS => break response,
                _ => tokio::time::sleep(Duration::from_secs(u64::from(attempt))).await,
            }
        };
        let status = response.status();
        if !status.is_success() {
            // The body is Google's error description, which holds no credentials.
            let body = response.text().await.unwrap_or_default();
            return Err(format!(
                "Google refused the sign-in ({status}): {body}; run `ingot gdrive-login` again"
            )
            .into());
        }
        let token: TokenResponse = response.json().await?;
        let value = token.access_token.clone();
        *current = Some(AccessToken {
            value: token.access_token,
            expires_at: SystemTime::now() + Duration::from_secs(token.expires_in),
        });
        Ok(value)
    }

    /// Drops `rejected` so the next caller refreshes, unless another upload already
    /// replaced it.
    pub(crate) async fn invalidate(&self, rejected: &str) {
        let mut current = self.current.lock().await;
        if current
            .as_ref()
            .is_some_and(|token| token.value == rejected)
        {
            *current = None;
        }
    }
}

fn random_url_safe() -> String {
    URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>())
}

fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Pulls the authorization code from the browser's redirect request line, rejecting
/// a redirect that doesn't carry the `state` this sign-in sent.
fn code_from_redirect(request: &str, state: &str) -> Result<String, ReplicationError> {
    let target = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or("malformed redirect from the browser")?;
    let url = reqwest::Url::parse(&format!("http://127.0.0.1{target}"))?;
    let param = |name: &str| {
        url.query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
    };
    if let Some(error) = param("error") {
        return Err(format!("Google sign-in failed: {error}").into());
    }
    if param("state").as_deref() != Some(state) {
        return Err("sign-in response did not match this request".into());
    }
    param("code").ok_or_else(|| "sign-in response had no code".into())
}

/// Runs the browser sign-in and saves the result to `credentials_path`. `open_browser`
/// receives the consent URL.
pub async fn drive_login(
    client_id: String,
    client_secret: Option<String>,
    credentials_path: PathBuf,
    open_browser: impl FnOnce(&str),
) -> Result<(), ReplicationError> {
    drive_login_with(
        AUTH_URL,
        TOKEN_URL,
        client_id,
        client_secret,
        credentials_path,
        open_browser,
    )
    .await
}

async fn drive_login_with(
    auth_url: &str,
    token_url: &str,
    client_id: String,
    client_secret: Option<String>,
    credentials_path: PathBuf,
    open_browser: impl FnOnce(&str),
) -> Result<(), ReplicationError> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let redirect_uri = format!("http://127.0.0.1:{}", listener.local_addr()?.port());
    let (verifier, state) = (random_url_safe(), random_url_safe());

    let consent = reqwest::Url::parse_with_params(
        auth_url,
        [
            ("client_id", client_id.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
            ("response_type", "code"),
            ("scope", DRIVE_FILE_SCOPE),
            ("code_challenge", pkce_challenge(&verifier).as_str()),
            ("code_challenge_method", "S256"),
            ("state", state.as_str()),
        ],
    )?;
    open_browser(consent.as_str());

    let (mut stream, _) = listener.accept().await?;
    let mut buffer = vec![0u8; 8192];
    let read = stream.read(&mut buffer).await?;
    let request = String::from_utf8_lossy(&buffer[..read]).into_owned();
    let code = code_from_redirect(&request, &state);
    let page = match &code {
        Ok(_) => "Signed in. You can close this tab and go back to Ingot.",
        Err(_) => "Sign-in failed. Go back to Ingot for details.",
    };
    stream
        .write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
                page.len()
            )
            .as_bytes(),
        )
        .await?;
    let code = code?;

    let mut form = vec![
        ("grant_type", "authorization_code"),
        ("code", code.as_str()),
        ("client_id", client_id.as_str()),
        ("code_verifier", verifier.as_str()),
        ("redirect_uri", redirect_uri.as_str()),
    ];
    if let Some(secret) = &client_secret {
        form.push(("client_secret", secret));
    }
    let response = reqwest::Client::new()
        .post(token_url)
        .form(&form)
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(format!("Google refused the sign-in ({status}): {body}").into());
    }
    let token: TokenResponse = response.json().await?;
    let refresh_token = token
        .refresh_token
        .ok_or("Google did not return a refresh token")?;

    DriveCredentials {
        client_id,
        client_secret,
        refresh_token,
    }
    .save(&credentials_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_matches_the_rfc_7636_example() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn redirect_needs_the_matching_state() {
        let ok = "GET /?state=abc&code=the%2Fcode HTTP/1.1\r\nHost: x\r\n\r\n";
        assert_eq!(code_from_redirect(ok, "abc").unwrap(), "the/code");
        assert!(code_from_redirect(ok, "other").is_err());
        assert!(code_from_redirect("GET /?error=access_denied HTTP/1.1", "abc").is_err());
        assert!(code_from_redirect("", "abc").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn saved_credentials_are_private_and_load_back() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("dir/token.json");
        let credentials = DriveCredentials {
            client_id: "id".into(),
            client_secret: None,
            refresh_token: "refresh".into(),
        };
        credentials.save(&path).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(
            DriveCredentials::load(&path).unwrap().refresh_token,
            "refresh"
        );
    }

    #[tokio::test]
    async fn full_sign_in_saves_the_refresh_token() {
        use axum::{Json, Router, routing::post};

        let app = Router::new().route(
            "/token",
            post(|body: String| async move {
                assert!(body.contains("code=granted"), "{body}");
                assert!(body.contains("code_verifier="), "{body}");
                Json(serde_json::json!({
                    "access_token": "a", "expires_in": 3600, "refresh_token": "kept"
                }))
            }),
        );
        let server = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let token_url = format!("http://{}/token", server.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(server, app).await });

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("token.json");
        // Plays the browser: follows the consent URL straight back to the redirect.
        let login = drive_login_with(
            "https://accounts.example/auth",
            &token_url,
            "client".into(),
            None,
            path.clone(),
            |consent| {
                let url = reqwest::Url::parse(consent).unwrap();
                let get = |name: &str| {
                    url.query_pairs()
                        .find(|(key, _)| key == name)
                        .unwrap()
                        .1
                        .into_owned()
                };
                assert_eq!(get("scope"), DRIVE_FILE_SCOPE);
                let redirect = format!(
                    "{}/?code=granted&state={}",
                    get("redirect_uri"),
                    get("state")
                );
                tokio::spawn(async move { reqwest::get(redirect).await.unwrap() });
            },
        );
        login.await.unwrap();

        assert_eq!(DriveCredentials::load(&path).unwrap().refresh_token, "kept");
    }
}
