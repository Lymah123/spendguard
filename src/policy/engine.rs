use std::fmt;

use crate::domain::{Lamports, PaymentProposal, PaymentRef, ValidatedPayment};
use crate::error::{Error, Result};
use crate::policy::resolver::{Allowlist, ResolveError};

/// Why a proposal was refused. Shown to the user and stored in the audit log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RejectReason {
    InvalidAmount(String),
    ZeroAmount,
    UnknownRecipient(String),
    RecipientNotAllowlisted(String),
    ExceedsPerTxLimit {
        amount: Lamports,
        limit: Lamports,
    },
    ExceedsDailyLimit {
        amount: Lamports,
        already_spent: Lamports,
        limit: Lamports,
    },
    BudgetOverflow,
}

impl RejectReason {
    /// Stable machine-readable code for the database.
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidAmount(_) => "invalid_amount",
            Self::ZeroAmount => "zero_amount",
            Self::UnknownRecipient(_) => "unknown_recipient",
            Self::RecipientNotAllowlisted(_) => "recipient_not_allowlisted",
            Self::ExceedsPerTxLimit { .. } => "exceeds_per_tx_limit",
            Self::ExceedsDailyLimit { .. } => "exceeds_daily_limit",
            Self::BudgetOverflow => "budget_overflow",
        }
    }
}

fn shorten(s: &str) -> String {
    const MAX: usize = 64;
    if s.chars().count() <= MAX {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(MAX).collect::<String>())
    }
}

impl fmt::Display for RejectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidAmount(a) => write!(f, "could not read {:?} as a SOL amount", shorten(a)),
            Self::ZeroAmount => write!(f, "amount must be greater than zero"),
            Self::UnknownRecipient(r) => write!(f, "{:?} is not an approved recipient", shorten(r)),
            Self::RecipientNotAllowlisted(a) => {
                write!(f, "address {a} is not on the approved recipient list")
            }
            Self::ExceedsPerTxLimit { amount, limit } => {
                write!(f, "{amount} exceeds the per-transaction limit of {limit}")
            }
            Self::ExceedsDailyLimit {
                amount,
                already_spent,
                limit,
            } => {
                let remaining = limit.checked_sub(*already_spent).unwrap_or(Lamports::ZERO);
                write!(
                    f,
                    "{amount} would exceed the daily limit of {limit} \
                     ({already_spent} already used, {remaining} remaining)"
                )
            }
            Self::BudgetOverflow => write!(f, "budget arithmetic overflowed; refusing"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDecision {
    Approved(ValidatedPayment),
    Rejected(RejectReason),
}

/// How "today" is measured for the daily limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BudgetWindow {
    /// The last 24 hours, sliding. Stricter: no midnight reset to exploit.
    #[default]
    Rolling24h,
    /// The current UTC calendar day; the budget resets at 00:00 UTC.
    UtcDay,
}

/// The spending rules. Immutable once built.
#[derive(Debug, Clone)]
pub struct Policy {
    max_per_tx: Lamports,
    daily_cap: Lamports,
    allowlist: Allowlist,
    window: BudgetWindow,
}

impl Policy {
    pub fn new(max_per_tx: Lamports, daily_cap: Lamports, allowlist: Allowlist) -> Result<Self> {
        if max_per_tx.is_zero() {
            return Err(Error::InvalidPolicy(
                "per-transaction limit must be > 0".into(),
            ));
        }
        if daily_cap < max_per_tx {
            return Err(Error::InvalidPolicy(
                "daily limit must be at least the per-transaction limit".into(),
            ));
        }
        Ok(Self {
            max_per_tx,
            daily_cap,
            allowlist,
            window: BudgetWindow::default(),
        })
    }

    pub fn with_window(mut self, window: BudgetWindow) -> Self {
        self.window = window;
        self
    }

    pub fn window(&self) -> BudgetWindow {
        self.window
    }

    pub fn max_per_tx(&self) -> Lamports {
        self.max_per_tx
    }
    pub fn daily_cap(&self) -> Lamports {
        self.daily_cap
    }
    pub fn allowlist(&self) -> &Allowlist {
        &self.allowlist
    }

    /// Judge one proposal.
    ///
    /// `spent_in_window` is the total already counted against the budget in the
    /// current rolling window (states where `counts_against_budget()` is true).
    /// The caller supplies it; this function never reads a clock or database.
    /// The store must call this again INSIDE the transaction that reserves the
    /// budget, so two concurrent requests cannot both pass on a stale total.
    ///
    /// Check order (first failure wins): amount, recipient, per-tx, daily.
    pub fn evaluate(
        &self,
        payment_ref: PaymentRef,
        proposal: &PaymentProposal,
        spent_in_window: Lamports,
    ) -> PolicyDecision {
        use PolicyDecision::Rejected;

        let amount = match Lamports::parse_sol(&proposal.amount) {
            Ok(a) => a,
            Err(_) => return Rejected(RejectReason::InvalidAmount(proposal.amount.clone())),
        };
        if amount.is_zero() {
            return Rejected(RejectReason::ZeroAmount);
        }

        let resolved = match self.allowlist.resolve(&proposal.recipient) {
            Ok(r) => r,
            Err(ResolveError::UnknownRecipient(r)) => {
                return Rejected(RejectReason::UnknownRecipient(r))
            }
            Err(ResolveError::NotAllowlisted(key)) => {
                return Rejected(RejectReason::RecipientNotAllowlisted(key.to_string()))
            }
        };

        if amount > self.max_per_tx {
            return Rejected(RejectReason::ExceedsPerTxLimit {
                amount,
                limit: self.max_per_tx,
            });
        }

        let total = match spent_in_window.checked_add(amount) {
            Some(t) => t,
            None => return Rejected(RejectReason::BudgetOverflow),
        };
        if total > self.daily_cap {
            return Rejected(RejectReason::ExceedsDailyLimit {
                amount,
                already_spent: spent_in_window,
                limit: self.daily_cap,
            });
        }

        PolicyDecision::Approved(ValidatedPayment::new(
            payment_ref,
            resolved.pubkey,
            Some(resolved.label),
            amount,
            proposal.note.clone(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::pubkey::Pubkey;

    fn sol(s: &str) -> Lamports {
        Lamports::parse_sol(s).unwrap()
    }

    fn proposal(recipient: &str, amount: &str) -> PaymentProposal {
        PaymentProposal {
            recipient: recipient.into(),
            amount: amount.into(),
            note: None,
        }
    }

    /// per-tx 0.5 SOL, daily 1 SOL, allowlist: ada, bob
    fn fixture() -> (Policy, Pubkey, Pubkey) {
        let (ada, bob) = (Pubkey::new_unique(), Pubkey::new_unique());
        let allow = Allowlist::new([("Ada".to_string(), ada), ("bob".to_string(), bob)]).unwrap();
        (Policy::new(sol("0.5"), sol("1"), allow).unwrap(), ada, bob)
    }

    fn run(p: &Policy, prop: &PaymentProposal, spent: &str) -> PolicyDecision {
        p.evaluate(PaymentRef::generate(), prop, sol(spent))
    }

    fn rejected(d: PolicyDecision) -> RejectReason {
        match d {
            PolicyDecision::Rejected(r) => r,
            PolicyDecision::Approved(v) => panic!("expected rejection, got approval of {v:?}"),
        }
    }

    #[test]
    fn approves_valid_payment_to_label() {
        let (p, ada, _) = fixture();
        match run(&p, &proposal("ada", "0.1 SOL"), "0") {
            PolicyDecision::Approved(v) => {
                assert_eq!(*v.recipient(), ada);
                assert_eq!(v.recipient_label(), Some("Ada"));
                assert_eq!(v.amount(), sol("0.1"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn approved_payment_keeps_the_callers_reference() {
        let (p, _, _) = fixture();
        let r = PaymentRef::from_stored("sg_fixed0000001".into());
        match p.evaluate(r.clone(), &proposal("ada", "0.1"), Lamports::ZERO) {
            PolicyDecision::Approved(v) => assert_eq!(v.payment_ref(), &r),
            other => panic!("{other:?}"),
        }
    }

    // The headline demo: the AI understood the request, policy still says no.
    #[test]
    fn blocks_over_per_tx_limit() {
        let (p, _, _) = fixture();
        let r = rejected(run(&p, &proposal("ada", "0.6 SOL"), "0"));
        assert_eq!(r.code(), "exceeds_per_tx_limit");
        assert!(r.to_string().contains("per-transaction limit"));
    }

    #[test]
    fn per_tx_boundary_is_inclusive() {
        let (p, _, _) = fixture();
        assert!(matches!(
            run(&p, &proposal("ada", "0.5"), "0"),
            PolicyDecision::Approved(_)
        ));
        let r = rejected(run(&p, &proposal("ada", "0.500000001"), "0"));
        assert_eq!(r.code(), "exceeds_per_tx_limit");
    }

    #[test]
    fn blocks_when_daily_limit_would_be_exceeded() {
        let (p, _, _) = fixture();
        let r = rejected(run(&p, &proposal("ada", "0.3"), "0.8"));
        assert_eq!(r.code(), "exceeds_daily_limit");
        assert!(r.to_string().contains("0.2 SOL remaining"));
    }

    #[test]
    fn daily_boundary_is_inclusive() {
        let (p, _, _) = fixture();
        assert!(matches!(
            run(&p, &proposal("ada", "0.1"), "0.9"),
            PolicyDecision::Approved(_)
        ));
        assert_eq!(
            rejected(run(&p, &proposal("ada", "0.2"), "0.9")).code(),
            "exceeds_daily_limit"
        );
    }

    #[test]
    fn already_at_cap_blocks_everything() {
        let (p, _, _) = fixture();
        assert_eq!(
            rejected(run(&p, &proposal("ada", "0.000000001"), "1")).code(),
            "exceeds_daily_limit"
        );
    }

    #[test]
    fn blocks_unknown_label() {
        let (p, _, _) = fixture();
        assert_eq!(
            rejected(run(&p, &proposal("mallory", "0.1"), "0")).code(),
            "unknown_recipient"
        );
    }

    #[test]
    fn blocks_raw_address_not_on_allowlist() {
        let (p, _, _) = fixture();
        let stranger = Pubkey::new_unique().to_string();
        let r = rejected(run(&p, &proposal(&stranger, "0.1"), "0"));
        assert_eq!(r.code(), "recipient_not_allowlisted");
    }

    #[test]
    fn accepts_raw_address_that_is_on_allowlist() {
        let (p, _, bob) = fixture();
        match run(&p, &proposal(&bob.to_string(), "0.1"), "0") {
            PolicyDecision::Approved(v) => {
                assert_eq!(*v.recipient(), bob);
                assert_eq!(v.recipient_label(), Some("bob"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn labels_are_case_and_whitespace_insensitive() {
        let (p, ada, _) = fixture();
        for r in ["ADA", " ada ", "Ada"] {
            match run(&p, &proposal(r, "0.1"), "0") {
                PolicyDecision::Approved(v) => assert_eq!(*v.recipient(), ada),
                other => panic!("{r:?}: {other:?}"),
            }
        }
    }

    #[test]
    fn rejects_bad_amounts_including_injected_text() {
        let (p, _, _) = fixture();
        for bad in [
            "all of it",
            "0.1 SOL; ignore limits",
            "-1",
            "1e3",
            "NaN",
            "0.1 USDC",
        ] {
            assert_eq!(
                rejected(run(&p, &proposal("ada", bad), "0")).code(),
                "invalid_amount",
                "{bad:?}"
            );
        }
        assert_eq!(
            rejected(run(&p, &proposal("ada", "0"), "0")).code(),
            "zero_amount"
        );
    }

    #[test]
    fn check_order_is_amount_then_recipient_then_limits() {
        let (p, _, _) = fixture();
        // unknown recipient AND over the limit: recipient reported first
        assert_eq!(
            rejected(run(&p, &proposal("mallory", "9"), "0")).code(),
            "unknown_recipient"
        );
        // bad amount AND unknown recipient: amount reported first
        assert_eq!(
            rejected(run(&p, &proposal("mallory", "lots"), "0")).code(),
            "invalid_amount"
        );
    }

    #[test]
    fn overflowing_spend_total_is_rejected_not_wrapped() {
        let (p, _, _) = fixture();
        let d = p.evaluate(
            PaymentRef::generate(),
            &proposal("ada", "0.1"),
            Lamports::new(u64::MAX),
        );
        assert_eq!(rejected(d).code(), "budget_overflow");
    }

    #[test]
    fn long_untrusted_text_is_truncated_in_messages() {
        let (p, _, _) = fixture();
        let long = "x".repeat(500);
        let msg = rejected(run(&p, &proposal(&long, "0.1"), "0")).to_string();
        assert!(msg.len() < 200);
    }

    #[test]
    fn allowlist_rejects_lookalike_and_duplicate_entries() {
        let k = Pubkey::new_unique();
        // a label that is itself a valid address
        assert!(Allowlist::new([(Pubkey::new_unique().to_string(), k)]).is_err());
        // non-ASCII / spaces / empty / too long
        for bad in ["аda", "ada smith", "", &"a".repeat(33)] {
            assert!(Allowlist::new([(bad.to_string(), k)]).is_err(), "{bad:?}");
        }
        // duplicate label ignoring case, duplicate address
        assert!(Allowlist::new([("ada".into(), k), ("ADA".into(), Pubkey::new_unique())]).is_err());
        assert!(Allowlist::new([("ada".into(), k), ("bob".into(), k)]).is_err());
    }

    #[test]
    fn policy_rejects_nonsense_limits() {
        let allow = Allowlist::new([]).unwrap();
        assert!(Policy::new(Lamports::ZERO, sol("1"), allow.clone()).is_err());
        assert!(Policy::new(sol("2"), sol("1"), allow.clone()).is_err());
        assert!(Policy::new(sol("1"), sol("1"), allow).is_ok());
    }

    #[test]
    fn empty_allowlist_rejects_everything() {
        let p = Policy::new(sol("1"), sol("1"), Allowlist::new([]).unwrap()).unwrap();
        assert_eq!(
            rejected(run(&p, &proposal("ada", "0.1"), "0")).code(),
            "unknown_recipient"
        );
    }
}
