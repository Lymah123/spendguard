use std::collections::HashMap;

use solana_sdk::pubkey::Pubkey;

use crate::error::{Error, Result};

pub const MAX_LABEL_LEN: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub pubkey: Pubkey,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// Not a known label and not a valid address.
    UnknownRecipient(String),
    /// A valid address, but it is not on the allowlist.
    NotAllowlisted(Pubkey),
}

/// The approved recipients, loaded from `spendguard.toml`.
///
/// Construction enforces the properties the resolver relies on:
/// labels are short ASCII identifiers, unique ignoring case, and can never be
/// mistaken for an address; each address appears at most once.
#[derive(Debug, Clone)]
pub struct Allowlist {
    /// lowercase label -> (label as configured, address)
    by_label: HashMap<String, (String, Pubkey)>,
    by_key: HashMap<Pubkey, String>,
}

impl Allowlist {
    pub fn new(entries: impl IntoIterator<Item = (String, Pubkey)>) -> Result<Self> {
        let mut by_label = HashMap::new();
        let mut by_key = HashMap::new();

        for (label, key) in entries {
            let invalid =
                |why: &str| Error::InvalidPolicy(format!("allowlist label {label:?}: {why}"));

            if label.is_empty() || label.len() > MAX_LABEL_LEN {
                return Err(invalid("must be 1..=32 characters"));
            }
            if !label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            {
                return Err(invalid(
                    "only ASCII letters, digits, '-' and '_' are allowed",
                ));
            }
            // A label that parses as an address could redirect a payment to a
            // different key than the one the user thinks they are paying.
            if label.parse::<Pubkey>().is_ok() {
                return Err(invalid("must not look like a Solana address"));
            }
            if by_key.insert(key, label.clone()).is_some() {
                return Err(Error::InvalidPolicy(format!(
                    "address {key} appears more than once in the allowlist"
                )));
            }
            if by_label
                .insert(label.to_ascii_lowercase(), (label.clone(), key))
                .is_some()
            {
                return Err(invalid("duplicate label (labels are case-insensitive)"));
            }
        }
        Ok(Self { by_label, by_key })
    }

    pub fn len(&self) -> usize {
        self.by_key.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }

    /// Resolve untrusted model output to an allowlisted address, or say why not.
    /// A raw address is accepted only if it is on the allowlist.
    pub fn resolve(&self, raw: &str) -> std::result::Result<Resolved, ResolveError> {
        let raw = raw.trim();

        if let Some((label, key)) = self.by_label.get(&raw.to_ascii_lowercase()) {
            return Ok(Resolved {
                pubkey: *key,
                label: label.clone(),
            });
        }

        match raw.parse::<Pubkey>() {
            Ok(key) => match self.by_key.get(&key) {
                Some(label) => Ok(Resolved {
                    pubkey: key,
                    label: label.clone(),
                }),
                None => Err(ResolveError::NotAllowlisted(key)),
            },
            Err(_) => Err(ResolveError::UnknownRecipient(raw.to_string())),
        }
    }
}
