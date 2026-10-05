//! Browser authorization and OS credential storage for HTTP MCP servers.
use crate::{Error, Result};
use rmcp::transport::auth::{
    AuthError, AuthorizationManager, AuthorizationRequest, AuthorizationSession,
    CredentialRefreshGuard, CredentialStore, StoredCredentials,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub client_id: String,
    pub scopes: String,
}

#[derive(Clone)]
struct KeychainStore {
    account: String,
    refresh: Arc<tokio::sync::Mutex<()>>,
}
impl KeychainStore {
    fn new(url: &str, config: &Config) -> Result<Self> {
        let mut url = reqwest::Url::parse(url)
            .map_err(|_| Error::Invalid("Invalid OAuth server URL".into()))?;
        url.set_fragment(None);
        let account = format!("{}|{}", url, config.client_id.trim());
        static LOCKS: OnceLock<Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<()>>>>> =
            OnceLock::new();
        let refresh = LOCKS
            .get_or_init(Default::default)
            .lock()
            .expect("OAuth locks")
            .entry(account.clone())
            .or_default()
            .clone();
        Ok(Self { account, refresh })
    }
    async fn entry<T: Send + 'static>(
        &self,
        operation: impl FnOnce(keyring::Entry) -> std::result::Result<T, AuthError> + Send + 'static,
    ) -> std::result::Result<T, AuthError> {
        let account = self.account.clone();
        tokio::task::spawn_blocking(move || {
            let entry = keyring::Entry::new("local.optchat.mcp.oauth", &account)
                .map_err(|_| store_error())?;
            operation(entry)
        })
        .await
        .map_err(|_| store_error())?
    }
}
fn store_error() -> AuthError {
    AuthError::CredentialStoreError(
        "Cannot access MCP credentials in the OS credential store".into(),
    )
}
#[async_trait::async_trait]
impl CredentialStore for KeychainStore {
    async fn load(&self) -> std::result::Result<Option<StoredCredentials>, AuthError> {
        self.entry(|entry| match entry.get_password() {
            Ok(value) => serde_json::from_str(&value)
                .map(Some)
                .map_err(|_| store_error()),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => Err(store_error()),
        })
        .await
    }
    async fn save(&self, credentials: StoredCredentials) -> std::result::Result<(), AuthError> {
        self.entry(move |entry| {
            let value = serde_json::to_string(&credentials).map_err(|_| store_error())?;
            entry.set_password(&value).map_err(|_| store_error())
        })
        .await
    }
    async fn clear(&self) -> std::result::Result<(), AuthError> {
        self.entry(|entry| match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(_) => Err(store_error()),
        })
        .await
    }
    async fn acquire_refresh_guard(
        &self,
    ) -> std::result::Result<Option<CredentialRefreshGuard>, AuthError> {
        Ok(Some(CredentialRefreshGuard::new(
            self.refresh.clone().lock_owned().await,
        )))
    }
}

fn auth_error(error: AuthError) -> Error {
    // Provider error bodies can include credentials; never forward them to chat/UI.
    let message = match error {
        AuthError::CredentialStoreError(_) => {
            "Cannot access MCP credentials in the OS credential store"
        }
        AuthError::RegistrationFailed(_) => {
            "OAuth client registration failed. Supply a registered public client ID if this server does not support dynamic registration"
        }
        AuthError::PkceUnsupported => "OAuth server does not support PKCE S256",
        AuthError::MetadataError(_) | AuthError::NoAuthorizationSupport => {
            "OAuth discovery failed; check the server URL and authorization metadata"
        }
        AuthError::TokenExchangeFailed(_) => "OAuth code exchange failed; sign in again",
        AuthError::TokenRefreshFailed(_) => "OAuth token refresh failed; retry the connection",
        AuthError::AuthorizationServerMismatch { .. }
        | AuthError::AuthorizationServerMissingIssuer { .. } => {
            "OAuth authorization issuer validation failed"
        }
        _ => "MCP OAuth authorization failed; sign in again",
    };
    Error::Invalid(message.into())
}

pub async fn manager(url: &str, config: &Config) -> Result<AuthorizationManager> {
    validate_url(url)?;
    let mut manager = AuthorizationManager::new(url).await.map_err(auth_error)?;
    manager.set_credential_store(KeychainStore::new(url, config)?);
    Ok(manager)
}

pub fn validate_url(url: &str) -> Result<()> {
    let url = reqwest::Url::parse(url).map_err(|_| Error::Invalid("Invalid OAuth URL".into()))?;
    let loopback = url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    if (url.scheme() != "https" && !(url.scheme() == "http" && loopback))
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(Error::Invalid(
            "OAuth requires HTTPS (HTTP is allowed only for loopback servers)".into(),
        ));
    }
    Ok(())
}

pub async fn sign_out(url: &str, config: &Config) -> Result<()> {
    let store = KeychainStore::new(url, config)?;
    let _guard = store.refresh.clone().lock_owned().await;
    store.clear().await.map_err(auth_error)
}

pub async fn sign_in(url: &str, config: &Config, open_browser: impl FnOnce(String)) -> Result<()> {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
    let redirect = format!(
        "http://127.0.0.1:{}/callback",
        listener.local_addr()?.port()
    );
    let mut manager = manager(url, config).await?;
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()?;
    let response = http.get(url).send().await?;
    let challenge = response
        .headers()
        .get(reqwest::header::WWW_AUTHENTICATE)
        .and_then(|v| v.to_str().ok());
    let metadata = manager
        .resolve_metadata_from_challenge(challenge)
        .await
        .map_err(auth_error)?;
    if !metadata.source.is_discovered() {
        return Err(Error::Invalid(
            "Server did not publish OAuth authorization metadata".into(),
        ));
    }
    manager.set_metadata(metadata.metadata);
    let mut request = AuthorizationRequest::new(&redirect)
        .with_client_name("OptChat")
        .with_application_type("native")
        .with_scopes(config.scopes.split_whitespace());
    if !config.client_id.trim().is_empty() {
        request = request.with_preregistered_client(config.client_id.trim());
    }
    let session = AuthorizationSession::new(manager, request)
        .await
        .map_err(|(_, e)| auth_error(e))?;
    open_browser(session.get_authorization_url().into());
    tokio::time::timeout(Duration::from_secs(300), callback(listener, &session))
        .await
        .map_err(|_| Error::Invalid("OAuth sign-in timed out; try again".into()))?
}

async fn callback(listener: TcpListener, session: &AuthorizationSession) -> Result<()> {
    let authorization = reqwest::Url::parse(session.get_authorization_url())
        .map_err(|_| Error::Invalid("Invalid authorization URL".into()))?;
    let expected_state = authorization
        .query_pairs()
        .find(|(k, _)| k == "state")
        .map(|(_, v)| v.into_owned())
        .ok_or_else(|| Error::Invalid("Missing OAuth state".into()))?;
    loop {
        let (mut socket, _) = listener.accept().await?;
        let mut line = String::new();
        let read = tokio::time::timeout(Duration::from_secs(5), async {
            BufReader::new((&mut socket).take(8192))
                .read_line(&mut line)
                .await
        })
        .await;
        if !matches!(read, Ok(Ok(_))) {
            continue;
        }
        let target = line.split_whitespace().collect::<Vec<_>>();
        let parsed =
            if target.len() == 3 && target[0] == "GET" && target[1].starts_with("/callback?") {
                reqwest::Url::parse(&format!("http://localhost{}", target[1])).ok()
            } else {
                None
            };
        let valid = parsed.as_ref().is_some_and(|url| {
            let states: Vec<_> = url
                .query_pairs()
                .filter(|(key, _)| key == "state")
                .collect();
            states.len() == 1 && states[0].1 == expected_state
        });
        if !valid {
            let _ = socket
                .write_all(
                    b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
                )
                .await;
            continue;
        }
        let parsed = parsed.expect("validated callback");
        let result = if parsed.query_pairs().any(|(key, _)| key == "error") {
            Err(Error::Invalid("OAuth authorization was declined".into()))
        } else {
            session
                .handle_callback_url(parsed.as_str())
                .await
                .map(|_| ())
                .map_err(auth_error)
        };
        let message = if result.is_ok() {
            "Signed in. You can close this tab and return to OptChat."
        } else {
            "Sign-in failed. Return to OptChat and try again."
        };
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nCache-Control: no-store\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{message}",
            message.len()
        );
        let _ = socket.write_all(response.as_bytes()).await;
        return result;
    }
}
