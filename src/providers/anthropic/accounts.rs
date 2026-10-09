//! Claude accounts the Anthropic passthrough can send requests with.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::auth::{AuthStorage, FileAuthStore};
use crate::paths;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StoredAnthropicAccount {
    pub name: String,
    pub token: String,
    /// Unix time in milliseconds.
    pub added_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct StoredAnthropicAccounts {
    pub accounts: Vec<StoredAnthropicAccount>,
}

pub struct AnthropicAccountStore<S: AuthStorage<StoredAnthropicAccounts>> {
    store: S,
}

impl<S: AuthStorage<StoredAnthropicAccounts>> AnthropicAccountStore<S> {
    pub fn new(store: S) -> Self {
        Self { store }
    }

    pub fn load(&self) -> anyhow::Result<StoredAnthropicAccounts> {
        Ok(self.store.load()?.unwrap_or_default())
    }

    /// Returns the number of registered accounts after adding this one.
    pub fn add(&self, name: &str, token: &str) -> anyhow::Result<usize> {
        let name = name.trim();
        let token = token.trim();
        if name.is_empty() {
            anyhow::bail!("Claude account name is required");
        }
        if token.is_empty() {
            anyhow::bail!("Claude account token is required");
        }
        let mut stored = self.load()?;
        if stored.accounts.iter().any(|account| account.name == name) {
            anyhow::bail!("Claude account {name} is already registered");
        }
        stored.accounts.push(StoredAnthropicAccount {
            name: name.to_string(),
            token: token.to_string(),
            added_at: now_ms(),
        });
        let account_count = stored.accounts.len();
        self.store.save(stored)?;
        Ok(account_count)
    }

    pub fn remove(&self, name: &str) -> anyhow::Result<()> {
        let mut stored = self.load()?;
        let Some(index) = stored
            .accounts
            .iter()
            .position(|account| account.name == name)
        else {
            anyhow::bail!("Claude account {name} is not registered");
        };
        stored.accounts.remove(index);
        self.store.save(stored)
    }

    pub fn path(&self) -> String {
        self.store.path()
    }
}

pub fn file_store() -> AnthropicAccountStore<impl AuthStorage<StoredAnthropicAccounts>> {
    let file = paths::config_dir()
        .join("anthropic")
        .join("accounts.json")
        .to_string_lossy()
        .to_string();
    AnthropicAccountStore::new(AccountFileStorage(FileAuthStore::new(file.clone(), file)))
}

/// Unlike the shared single-credential store, a malformed registry must not become an
/// empty registry: the next `add` would otherwise overwrite every registered account.
struct AccountFileStorage(FileAuthStore<StoredAnthropicAccounts>);

impl AuthStorage<StoredAnthropicAccounts> for AccountFileStorage {
    fn load(&self) -> anyhow::Result<Option<StoredAnthropicAccounts>> {
        let path = self.0.path();
        let raw = match std::fs::read(&path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err).with_context(|| format!("Cannot read {path}")),
        };
        serde_json::from_slice(&raw)
            .map(Some)
            .with_context(|| format!("Invalid Claude accounts file: {path}"))
    }

    fn save(&self, value: StoredAnthropicAccounts) -> anyhow::Result<()> {
        self.0.save(value)
    }

    fn clear(&self) -> anyhow::Result<()> {
        self.0.clear()
    }

    fn path(&self) -> String {
        self.0.path()
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{InMemoryAuthStore, fixture_store};

    fn memory_store() -> AnthropicAccountStore<InMemoryAuthStore<StoredAnthropicAccounts>> {
        AnthropicAccountStore::new(fixture_store())
    }

    #[test]
    fn add_then_load_returns_trimmed_account() {
        let store = memory_store();

        assert_eq!(store.add(" work ", " sk-ant-oat-work\n").unwrap(), 1);

        let loaded = store.load().unwrap();
        assert_eq!(loaded.accounts.len(), 1);
        assert_eq!(loaded.accounts[0].name, "work");
        assert_eq!(loaded.accounts[0].token, "sk-ant-oat-work");
    }

    #[test]
    fn add_rejects_duplicate_name() {
        let store = memory_store();
        store.add("work", "sk-ant-oat-work").unwrap();

        assert!(store.add("work", "sk-ant-oat-other").is_err());
        assert_eq!(store.load().unwrap().accounts.len(), 1);
    }

    #[test]
    fn add_rejects_empty_token() {
        let store = memory_store();

        assert!(store.add("work", " \n").is_err());
        assert!(store.load().unwrap().accounts.is_empty());
    }

    #[test]
    fn remove_deletes_registered_account() {
        let store = memory_store();
        store.add("work", "sk-ant-oat-work").unwrap();

        store.remove("work").unwrap();

        assert!(store.load().unwrap().accounts.is_empty());
    }

    #[test]
    fn remove_rejects_unknown_name() {
        let store = memory_store();

        assert!(store.remove("missing").is_err());
    }
}
