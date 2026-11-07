use std::fmt;

use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;

use crate::domain::lamports::Lamports;
use crate::error::{Error, Result};

pub const MAX_NOTE_LEN: usize = 140;

/// What the LLM is allowed to produce, and nothing more. Everything is an
/// untrusted string: `recipient` is a label or address the resolver must
/// verify, and `amount` is text that OUR parser converts, so the model never
/// does arithmetic. Unknown fields are rejected outright.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaymentProposal {
    pub recipient: String,
    pub amount: String,
    #[serde(default)]
    pub note: Option<String>,
}

impl PaymentProposal {
    /// Strictly parse model output. Fails closed on bad JSON, unknown fields,
    /// empty fields, or an over-long note.
    pub fn from_json(raw: &str) -> Result<Self> {
        // serde would also accept a JSON array here (fields by position), so insist on an object.
        let value: serde_json::Value = serde_json::from_str(raw)?;
        if !value.is_object() {
            return Err(Error::MalformedProposal("expected a JSON object".into()));
        }
        let p: Self = serde_json::from_value(value)?;
        p.check()?;
        Ok(p)
    }

    /// Shape checks that do not need the policy: non-empty fields, bounded note.
    pub fn check(&self) -> Result<()> {
        if self.recipient.trim().is_empty() {
            return Err(Error::MalformedProposal("recipient is empty".into()));
        }
        if self.amount.trim().is_empty() {
            return Err(Error::MalformedProposal("amount is empty".into()));
        }
        if let Some(note) = &self.note {
            if note.chars().count() > MAX_NOTE_LEN {
                return Err(Error::MalformedProposal(format!(
                    "note exceeds {MAX_NOTE_LEN} characters"
                )));
            }
        }
        Ok(())
    }
}

/// Human-readable payment reference that goes in the on-chain memo.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PaymentRef(String);

impl PaymentRef {
    pub fn generate() -> Self {
        let id = uuid::Uuid::new_v4().simple().to_string();
        Self(format!("sg_{}", &id[..12]))
    }

    /// Rehydrate a reference read back from the database.
    pub fn from_stored(s: String) -> Self {
        Self(s)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PaymentRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A payment the policy engine has approved. Fields are private and the
/// constructor is `pub(crate)`, so the only code that can produce one is code
/// inside this crate (the policy engine). The transaction builder accepts only
/// this type, which means "unvalidated money movement" does not type-check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedPayment {
    payment_ref: PaymentRef,
    recipient: Pubkey,
    recipient_label: Option<String>,
    amount: Lamports,
    note: Option<String>,
}

impl ValidatedPayment {
    pub(crate) fn new(
        payment_ref: PaymentRef,
        recipient: Pubkey,
        recipient_label: Option<String>,
        amount: Lamports,
        note: Option<String>,
    ) -> Self {
        Self {
            payment_ref,
            recipient,
            recipient_label,
            amount,
            note,
        }
    }

    pub fn payment_ref(&self) -> &PaymentRef {
        &self.payment_ref
    }
    pub fn recipient(&self) -> &Pubkey {
        &self.recipient
    }
    pub fn recipient_label(&self) -> Option<&str> {
        self.recipient_label.as_deref()
    }
    pub fn amount(&self) -> Lamports {
        self.amount
    }
    pub fn note(&self) -> Option<&str> {
        self.note.as_deref()
    }

    /// On-chain memo: the reference only. The free-text note stays local.
    pub fn memo_text(&self) -> String {
        format!("spendguard:{}", self.payment_ref)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_proposal() {
        let p = PaymentProposal::from_json(
            r#"{"recipient":"alice","amount":"0.1 SOL","note":"lunch"}"#,
        )
        .unwrap();
        assert_eq!(p.recipient, "alice");
        assert_eq!(p.note.as_deref(), Some("lunch"));
    }

    #[test]
    fn note_is_optional() {
        assert!(PaymentProposal::from_json(r#"{"recipient":"a","amount":"1"}"#).is_ok());
    }

    #[test]
    fn rejects_unknown_fields() {
        // e.g. an injected "override_policy" field must never be silently ignored
        let r =
            PaymentProposal::from_json(r#"{"recipient":"a","amount":"1","override_policy":true}"#);
        assert!(matches!(r, Err(Error::Json(_))));
    }

    #[test]
    fn rejects_empty_and_oversized_fields() {
        assert!(PaymentProposal::from_json(r#"{"recipient":" ","amount":"1"}"#).is_err());
        assert!(PaymentProposal::from_json(r#"{"recipient":"a","amount":""}"#).is_err());
        let long = "x".repeat(MAX_NOTE_LEN + 1);
        let raw = format!(r#"{{"recipient":"a","amount":"1","note":"{long}"}}"#);
        assert!(PaymentProposal::from_json(&raw).is_err());
    }

    #[test]
    fn rejects_arrays_and_other_non_objects() {
        for bad in [
            r#"["ada","1"]"#,
            r#"["ada","1",null]"#,
            "\"ada\"",
            "null",
            "42",
        ] {
            assert!(PaymentProposal::from_json(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn rejects_non_json_and_wrong_types() {
        assert!(PaymentProposal::from_json("send alice 1 sol").is_err());
        assert!(PaymentProposal::from_json(r#"{"recipient":"a","amount":1}"#).is_err());
    }

    #[test]
    fn payment_refs_are_unique_and_prefixed() {
        let a = PaymentRef::generate();
        let b = PaymentRef::generate();
        assert_ne!(a, b);
        assert!(a.as_str().starts_with("sg_"));
        assert_eq!(a.as_str().len(), 3 + 12);
    }

    #[test]
    fn memo_contains_reference_not_note() {
        let v = ValidatedPayment::new(
            PaymentRef::from_stored("sg_abc123def456".into()),
            Pubkey::new_unique(),
            Some("alice".into()),
            Lamports::new(1),
            Some("private note".into()),
        );
        assert_eq!(v.memo_text(), "spendguard:sg_abc123def456");
        assert!(!v.memo_text().contains("private"));
    }
}
