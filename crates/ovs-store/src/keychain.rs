//! Passwords in the OS credential store (macOS Keychain, Windows Credential Manager), keyed by
//! connection id.
//!
//! Every keychain access can make macOS ask the user for permission, so lookups are cached for
//! the life of the process and unchanged passwords are never rewritten.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock, PoisonError};

use crate::Result;

const SERVICE: &str = "openvisql";

/// What this process already knows about a connection's keychain entry.
#[derive(Clone, PartialEq, Eq)]
enum Known {
    Stored(String),
    /// Looked up (or deleted): there is no entry, so don't ask the keychain again.
    Missing,
}

fn cache() -> &'static Mutex<HashMap<String, Known>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Known>>> = OnceLock::new();
    CACHE.get_or_init(Mutex::default)
}

fn cached(account: &str) -> Option<Known> {
    cache()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(account)
        .cloned()
}

fn remember(account: &str, known: Known) {
    cache()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(account.to_owned(), known);
}

fn entry(account: &str) -> keyring::Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, account)
}

/// `account` is the connection id, with a `:ssh` suffix for SSH secrets.
pub fn get_password(account: &str) -> Result<Option<String>> {
    match cached(account) {
        Some(Known::Stored(p)) => return Ok(Some(p)),
        Some(Known::Missing) => return Ok(None),
        None => {}
    }
    match entry(account)?.get_password() {
        Ok(p) => {
            remember(account, Known::Stored(p.clone()));
            Ok(Some(p))
        }
        Err(keyring::Error::NoEntry) => {
            remember(account, Known::Missing);
            Ok(None)
        }
        Err(e) => Err(e.into()),
    }
}

/// Stores `password`, skipping the keychain write when it already holds that value.
pub fn set_password(account: &str, password: &str) -> Result<()> {
    if matches!(cached(account), Some(Known::Stored(p)) if p == password) {
        return Ok(());
    }
    entry(account)?.set_password(password)?;
    remember(account, Known::Stored(password.to_owned()));
    Ok(())
}

pub fn delete_password(account: &str) -> Result<()> {
    if cached(account) == Some(Known::Missing) {
        return Ok(());
    }
    match entry(account)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => {
            remember(account, Known::Missing);
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}
