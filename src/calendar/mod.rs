//! Calendar data is provider-neutral; authentication and transport stay here.
mod auth;
mod credentials;
mod graph;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

pub type Result<T> = std::result::Result<T, String>;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Person {
    pub name: String,
    pub email: String,
}

impl Person {
    pub fn label(&self) -> &str {
        if self.name.is_empty() {
            &self.email
        } else {
            &self.name
        }
    }
}

/// A snapshot of the invitation, not an assertion that every invitee attended.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Meeting {
    pub provider: String,
    pub id: String,
    pub title: String,
    pub starts_at: i64,
    pub ends_at: i64,
    pub participants: Vec<Person>,
}

#[derive(Clone, Default, Deserialize, Serialize)]
pub struct Config {
    pub enabled: bool,
    pub email: String,
    pub client_id: String,
    #[serde(default)]
    pub tenant: String,
}

impl Config {
    pub fn new(email: &str, client_id: &str) -> Result<Self> {
        let email = email.trim();
        let (_, domain) = email
            .rsplit_once('@')
            .ok_or("Enter your work or school email address.")?;
        if email.len() > 254
            || email.contains(char::is_whitespace)
            || email.matches('@').count() != 1
            || email.starts_with('@')
            || !valid_domain(domain)
        {
            return Err("Enter a valid work or school email address.".into());
        }
        let client_id = client_id.trim();
        if !is_guid(client_id) {
            return Err("Enter the Application (client) ID from Entra, in UUID format.".into());
        }
        Ok(Self {
            enabled: true,
            email: email.to_owned(),
            client_id: client_id.to_ascii_lowercase(),
            tenant: String::new(),
        })
    }

    pub fn load() -> Self {
        std::fs::read(config_path())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    fn save(&self) -> Result<()> {
        let path = config_path();
        std::fs::create_dir_all(path.parent().unwrap())
            .map_err(|_| "Could not create calendar settings folder.")?;
        let bytes =
            serde_json::to_vec_pretty(self).map_err(|_| "Could not encode calendar settings.")?;
        crate::files::atomic_write(&path, &bytes)
            .map_err(|_| "Could not save calendar settings.".into())
    }
}

fn config_path() -> PathBuf {
    gtk::glib::user_config_dir()
        .join(crate::APP_NAME)
        .join("calendar.json")
}

fn valid_domain(domain: &str) -> bool {
    domain.len() <= 253
        && domain.contains('.')
        && domain.split('.').all(|part| {
            !part.is_empty()
                && part.len() <= 63
                && !part.starts_with('-')
                && !part.ends_with('-')
                && part.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
        })
}

fn is_guid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(i, c)| {
            if [8, 13, 18, 23].contains(&i) {
                c == b'-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
}

pub fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err("Calendar operation cancelled.".into())
    } else {
        Ok(())
    }
}

/// Accessed only by serialized background tasks. No calendar network calls when disabled.
pub struct Service {
    pub config: Config,
    tokens: Option<auth::Tokens>,
    /// A keyring failure must be retried before the next calendar request.
    tokens_need_save: bool,
}

impl Service {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            tokens: None,
            tokens_need_save: false,
        }
    }

    pub fn connect(
        &mut self,
        mut config: Config,
        cancel: &AtomicBool,
        open_browser: impl FnOnce(String) -> Result<()>,
    ) -> Result<()> {
        let changed_account =
            self.config.email != config.email || self.config.client_id != config.client_id;
        if changed_account {
            if self.config.enabled {
                return Err(
                    "Disconnect the current calendar before changing accounts or client IDs."
                        .into(),
                );
            }
            credentials::clear(&self.config)?;
        }
        config.tenant = auth::discover(&config)?;
        check_cancel(cancel)?;
        let tokens = auth::sign_in(&config, cancel, open_browser)?;
        check_cancel(cancel)?;
        graph::verify_account(&tokens.access_token, &config.email)?;
        check_cancel(cancel)?;
        credentials::save(&config, &tokens)?;
        if let Err(error) = check_cancel(cancel).and_then(|_| config.save()) {
            let _ = credentials::clear(&config);
            return Err(error);
        }
        self.config = config;
        self.tokens = Some(tokens);
        self.tokens_need_save = false;
        Ok(())
    }

    pub fn disconnect(&mut self) -> Result<()> {
        // Disable network access even if the keyring is temporarily locked.
        let mut disabled = self.config.clone();
        disabled.enabled = false;
        disabled.save()?;
        self.config = disabled;
        self.tokens = None;
        self.tokens_need_save = false;
        credentials::clear(&self.config)
    }

    pub fn upcoming(&mut self, cancel: &AtomicBool) -> Result<Vec<Meeting>> {
        if !self.config.enabled {
            return Ok(Vec::new());
        }
        check_cancel(cancel)?;
        if self.tokens.is_none() {
            self.tokens = Some(credentials::load(&self.config)?);
        }
        self.save_pending_tokens()?;
        if self.tokens.as_ref().unwrap().expires_at <= crate::ipc::now() + 120 {
            self.refresh_tokens(cancel)?;
        }
        match graph::upcoming(&self.tokens.as_ref().unwrap().access_token, cancel) {
            Err(graph::Error::Unauthorized) => {
                self.refresh_tokens(cancel)?;
                graph::upcoming(&self.tokens.as_ref().unwrap().access_token, cancel)
                    .map_err(|e| e.message())
            }
            result => result.map_err(|e| e.message()),
        }
    }

    fn refresh_tokens(&mut self, cancel: &AtomicBool) -> Result<()> {
        check_cancel(cancel)?;
        let tokens = auth::refresh(&self.config, self.tokens.as_ref().unwrap())?;
        // Once redeemed, retain and save the replacement before honoring cancellation.
        self.tokens = Some(tokens);
        self.tokens_need_save = true;
        self.save_pending_tokens()?;
        check_cancel(cancel)
    }

    fn save_pending_tokens(&mut self) -> Result<()> {
        if self.tokens_need_save {
            credentials::save(&self.config, self.tokens.as_ref().unwrap())?;
            self.tokens_need_save = false;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn disabled_calendar_never_needs_credentials_or_network() {
        let mut service = Service::new(Config::default());
        assert!(
            service
                .upcoming(&AtomicBool::new(false))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn rejects_email_domains_and_client_ids_that_could_change_endpoints() {
        let id = "01234567-89ab-cdef-0123-456789abcdef";
        assert!(Config::new(" User@example.com ", id).is_ok());
        for email in [
            "foo",
            "@example.com",
            "a@@example.com",
            "a@example.com/path",
            "a@bad..com",
            "a@-bad.com",
            "a@localhost",
        ] {
            assert!(Config::new(email, id).is_err(), "{email}");
        }
        assert!(Config::new("a@example.com", "common/evil").is_err());
        assert!(!Config::default().enabled);
    }
}
