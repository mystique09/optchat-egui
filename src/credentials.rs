use crate::{Error, Result, model::Provider};

const SERVICE: &str = "local.optchat.providers";

fn entry(provider: Provider) -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, provider.key_variable()).map_err(|_| unavailable())
}

fn unavailable() -> Error {
    Error::Invalid("Cannot access the OS credential store. Unlock it and try again; no key was changed in OptChat.".into())
}

/// Saved credentials take precedence; environment variables remain a fallback.
pub async fn load(provider: Provider) -> Result<String> {
    tokio::task::spawn_blocking(move || match entry(provider)?.get_password() {
        Ok(key) => Ok(key),
        Err(keyring::Error::NoEntry) => {
            Ok(std::env::var(provider.key_variable()).unwrap_or_default())
        }
        Err(_) => Err(unavailable()),
    })
    .await
    .map_err(|_| unavailable())?
}

pub async fn save(provider: Provider, key: String) -> Result<String> {
    let key = validate(&key)?.to_owned();
    tokio::task::spawn_blocking(move || {
        entry(provider)?
            .set_password(&key)
            .map_err(|_| unavailable())?;
        Ok(key)
    })
    .await
    .map_err(|_| unavailable())?
}

fn validate(key: &str) -> Result<&str> {
    let key = key.trim();
    if key.is_empty() || !key.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(Error::Invalid(
            "Enter an API key without spaces or line breaks.".into(),
        ));
    }
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_without_exposing_secrets() {
        assert_eq!(validate("  test-key\n").unwrap(), "test-key");
        for key in ["", "   ", "secret key", "secret\nkey", "secret\0key"] {
            let error = validate(key).unwrap_err().to_string();
            assert!(!error.contains("secret"));
        }
    }

    #[test]
    #[ignore = "requires an unlocked OS credential store"]
    fn native_store_round_trip() {
        let service = format!("local.optchat.test.{}", std::process::id());
        let a = keyring::Entry::new(&service, "anthropic").unwrap();
        let d = keyring::Entry::new(&service, "deepseek").unwrap();
        a.set_password("fixture-a").unwrap();
        d.set_password("fixture-d").unwrap();
        let result = (a.get_password(), d.get_password());
        a.delete_credential().unwrap();
        d.delete_credential().unwrap();
        assert_eq!(result.0.unwrap(), "fixture-a");
        assert_eq!(result.1.unwrap(), "fixture-d");
        assert!(matches!(a.get_password(), Err(keyring::Error::NoEntry)));
    }
}
