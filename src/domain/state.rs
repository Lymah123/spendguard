use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Lifecycle of one payment. Every change goes through `transition`, and each
/// accepted transition should be appended to `state_log` by the store.
///
/// ```text
/// Proposed ──► PolicyRejected
///    └──────► Reserved ──► UserDeclined
///                 ├──────► Authorized ──► Submitted ──► Confirmed
///                 │             └──────────┬──────────► Failed
///                 └──────► Failed ◄────────┘
/// ```
///
/// Ordering rule for the signer/sender: record `Submitted` (with the signature,
/// which is known right after signing) BEFORE the network send. If the send
/// call times out, the payment is already `Submitted` and `reconcile` can ask
/// the chain whether it landed. Only mark `Submitted -> Failed` once the
/// blockhash has expired or the chain reports the transaction failed;
/// otherwise a payment that later lands would escape the daily budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaymentState {
    /// Parsed and shaped, not yet judged by policy.
    Proposed,
    /// Policy said no. Terminal.
    PolicyRejected,
    /// Policy said yes and the amount is reserved against the budget.
    Reserved,
    /// The user refused at the approval prompt. Terminal; frees the budget.
    UserDeclined,
    /// The user approved the exact built message. Signing may happen now.
    Authorized,
    /// Signature recorded and transaction sent (or about to be).
    Submitted,
    /// Confirmed on chain. Terminal.
    Confirmed,
    /// Did not and will not land. Terminal; frees the budget.
    Failed,
}

impl PaymentState {
    /// Every variant, in lifecycle order. Keep this next to the enum:
    /// a variant missing here is silently ignored by the budget query
    /// and by `from_db`.
    pub const ALL: [PaymentState; 8] = [
        PaymentState::Proposed,
        PaymentState::PolicyRejected,
        PaymentState::Reserved,
        PaymentState::UserDeclined,
        PaymentState::Authorized,
        PaymentState::Submitted,
        PaymentState::Confirmed,
        PaymentState::Failed,
    ];

    pub fn can_transition_to(self, to: PaymentState) -> bool {
        use PaymentState::*;
        matches!(
            (self, to),
            (Proposed, PolicyRejected)
                | (Proposed, Reserved)
                | (Reserved, UserDeclined)
                | (Reserved, Authorized)
                | (Reserved, Failed) // e.g. message build failed
                | (Authorized, Submitted)
                | (Authorized, Failed) // pre-send failures only (signing error)
                | (Submitted, Confirmed)
                | (Submitted, Failed)
        )
    }

    pub fn transition(self, to: PaymentState) -> Result<PaymentState> {
        if self.can_transition_to(to) {
            Ok(to)
        } else {
            Err(Error::InvalidTransition { from: self, to })
        }
    }

    pub fn is_terminal(self) -> bool {
        use PaymentState::*;
        matches!(self, PolicyRejected | UserDeclined | Confirmed | Failed)
    }

    /// Whether a payment in this state counts toward the daily limit.
    /// Pending states count too, so a crash can never let a restart
    /// re-spend the same budget.
    pub fn counts_against_budget(self) -> bool {
        use PaymentState::*;
        matches!(self, Reserved | Authorized | Submitted | Confirmed)
    }

    /// Stable string for the SQLite `state` column.
    pub fn as_str(self) -> &'static str {
        use PaymentState::*;
        match self {
            Proposed => "proposed",
            PolicyRejected => "policy_rejected",
            Reserved => "reserved",
            UserDeclined => "user_declined",
            Authorized => "authorized",
            Submitted => "submitted",
            Confirmed => "confirmed",
            Failed => "failed",
        }
    }

    /// Inverse of `as_str()`, used when reading rows back from SQLite.
    pub fn from_db(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|st| st.as_str() == s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use PaymentState::*;

    const ALL: [PaymentState; 8] = PaymentState::ALL;

    #[test]
    fn happy_path() {
        let s = Proposed
            .transition(Reserved)
            .unwrap()
            .transition(Authorized)
            .unwrap()
            .transition(Submitted)
            .unwrap()
            .transition(Confirmed)
            .unwrap();
        assert!(s.is_terminal());
    }

    #[test]
    fn terminal_states_have_no_exits() {
        for from in ALL.into_iter().filter(|s| s.is_terminal()) {
            for to in ALL {
                assert!(
                    !from.can_transition_to(to),
                    "{from:?} -> {to:?} must be illegal"
                );
            }
        }
    }

    #[test]
    fn cannot_skip_authorization() {
        assert!(Reserved.transition(Submitted).is_err());
        assert!(Proposed.transition(Authorized).is_err());
        assert!(Proposed.transition(Submitted).is_err());
    }

    #[test]
    fn no_state_transitions_to_itself() {
        for s in ALL {
            assert!(!s.can_transition_to(s));
        }
    }

    #[test]
    fn budget_accounting_matches_lifecycle() {
        assert!(Reserved.counts_against_budget());
        assert!(Authorized.counts_against_budget());
        assert!(Submitted.counts_against_budget());
        assert!(Confirmed.counts_against_budget());
        assert!(!Proposed.counts_against_budget());
        assert!(!PolicyRejected.counts_against_budget());
        assert!(!UserDeclined.counts_against_budget());
        assert!(!Failed.counts_against_budget());
    }

    #[test]
    fn db_strings_round_trip() {
        for st in PaymentState::ALL {
            assert_eq!(PaymentState::from_db(st.as_str()), Some(st));
        }
        assert_eq!(PaymentState::from_db("nonsense"), None);
    }

    #[test]
    fn serde_names_match_db_strings() {
        // as_str() feeds SQLite and serde feeds the dashboard JSON: they must agree.
        for st in PaymentState::ALL {
            assert_eq!(
                serde_json::to_string(&st).unwrap(),
                format!("\"{}\"", st.as_str())
            );
        }
    }
}
