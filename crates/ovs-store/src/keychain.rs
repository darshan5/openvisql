//! Passwords in the OS credential store (macOS Keychain, Windows Credential Manager), keyed by
//! connection id.
//!
//! Every keychain access can make macOS ask the user for permission, so lookups are cached for
//! the life of the process and unchanged passwords are never rewritten.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock, PoisonError};

use uuid::Uuid;

use crate::Result;

const SERVICE: &str = "openvisql";

/// What this process already knows about a connection's keychain entry.
#[derive(Clone, PartialEq, Eq)]
enum Known {
    Stored(String),
    /// Looked up (or deleted): there is no entry, so don't ask the keychain again.
    Missing,
}

fn cache() -> &'static Mutex<HashMap<Uuid, Known>> {
    static CACHE: OnceLock<Mutex<HashMap<Uuid, Known>>> = OnceLock::new();
    CACHE.get_or_init(Mutex::default)
}

fn cached(id: Uuid) -> Option<Known> {
    cache()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&id)
        .cloned()
}

fn remember(id: Uuid, known: Known) {
    cache()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(id, known);
}

fn entry(id: Uuid) -> keyring::Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, &id.to_string())
}

pub fn get_password(id: Uuid) -> Result<Option<String>> {
    match cached(id) {
        Some(Known::Stored(p)) => return Ok(Some(p)),
        Some(Known::Missing) => return Ok(None),
        None => {}
    }
    match entry(id)?.get_password() {
        Ok(p) => {
            remember(id, Known::Stored(p.clone()));
            Ok(Some(p))
        }
        Err(keyring::Error::NoEntry) => {
            remember(id, Known::Missing);
            Ok(None)
        }
        Err(e) => Err(e.into()),
    }
}

/// Stores `password`, skipping the keychain write when it already holds that value.
pub fn set_password(id: Uuid, password: &str) -> Result<()> {
    if matches!(cached(id), Some(Known::Stored(p)) if p == password) {
        return Ok(());
    }
    entry(id)?.set_password(password)?;
    remember(id, Known::Stored(password.to_owned()));
    Ok(())
}

pub fn delete_password(id: Uuid) -> Result<()> {
    if cached(id) == Some(Known::Missing) {
        return Ok(());
    }
    match entry(id)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => {
            remember(id, Known::Missing);
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}
