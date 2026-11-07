use std::str::FromStr;
use std::time::Duration;

use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;

use crate::audit::{unix_now, PaymentStore};
use crate::domain::{Lamports, PaymentProposal, PaymentRef, ValidatedPayment};
use crate::error::{Error, Result};
use crate::policy::{Policy, PolicyDecision, RejectReason};
use crate::solana::builder::{build_message, verify_matches, DecodedPayment};
use crate::solana::client::{ChainClient, TxStatus};
use crate::wallet::signer::FileSigner;

/// Kept back from the balance check to cover the network fee.
pub const FEE_BUFFER: Lamports = Lamports::new(10_000);

/// A transaction is only declared dead once the chain has moved this many
/// blocks past its last valid height. The margin absorbs RPC nodes that lag.
pub const EXPIRY_MARGIN_BLOCKS: u64 = 20;

/// `Reserved`/`Authorized` payments older than this are assumed abandoned by a
/// crashed process. Younger ones may belong to a live `pay` in another process.
pub const STALE_PENDING_SECS: i64 = 300;

/// What the user sees before signing. Built from the DECODED message, i.e. from
/// the bytes that will actually be signed, not from the proposal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalSummary {
    pub payment_ref: PaymentRef,
    pub from: Pubkey,
    pub to: Pubkey,
    pub recipient_label: Option<String>,
    pub amount: Lamports,
    pub memo: String,
    pub note: Option<String>,
    /// Daily budget left once this payment is counted.
    pub budget_remaining_after: Lamports,
}

pub trait Approver {
    /// `true` = sign and send this exact payment. Anything else = decline.
    fn approve(&self, summary: &ApprovalSummary) -> bool;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PayOutcome {
    Rejected {
        payment_ref: PaymentRef,
        reason: RejectReason,
    },
    Declined {
        payment_ref: PaymentRef,
    },
    Confirmed {
        payment_ref: PaymentRef,
        signature: Signature,
    },
    /// Sent (or possibly sent) but not yet confirmed. Run `reconcile` later.
    Pending {
        payment_ref: PaymentRef,
        signature: Signature,
        detail: String,
    },
    Failed {
        payment_ref: PaymentRef,
        reason: String,
    },
}

#[derive(Debug, Clone)]
pub struct ConfirmSettings {
    pub attempts: u32,
    pub interval: Duration,
}

impl Default for ConfirmSettings {
    fn default() -> Self {
        Self {
            attempts: 30,
            interval: Duration::from_secs(2),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileResult {
    Confirmed,
    Failed(String),
    StillPending,
    /// Never reached the chain; released.
    Abandoned,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileReport {
    pub payment_ref: PaymentRef,
    pub result: ReconcileResult,
}

enum Resolution {
    Confirmed,
    Failed(String),
    Pending,
}

pub struct PaymentService<C: ChainClient> {
    store: PaymentStore,
    policy: Policy,
    chain: C,
    signer: FileSigner,
    clock: Box<dyn Fn() -> i64>,
    confirm: ConfirmSettings,
}

impl<C: ChainClient> PaymentService<C> {
    pub fn new(mut store: PaymentStore, policy: Policy, chain: C, signer: FileSigner) -> Self {
        // One source of truth for "what counts as today".
        store.set_window(policy.window());
        Self {
            store,
            policy,
            chain,
            signer,
            clock: Box::new(unix_now),
            confirm: ConfirmSettings::default(),
        }
    }

    pub fn with_clock(mut self, clock: Box<dyn Fn() -> i64>) -> Self {
        self.clock = clock;
        self
    }

    pub fn with_confirm(mut self, confirm: ConfirmSettings) -> Self {
        self.confirm = confirm;
        self
    }

    pub fn store(&self) -> &PaymentStore {
        &self.store
    }
    pub fn store_mut(&mut self) -> &mut PaymentStore {
        &mut self.store
    }
    pub fn policy(&self) -> &Policy {
        &self.policy
    }
    pub fn wallet_pubkey(&self) -> Pubkey {
        self.signer.pubkey()
    }

    /// The whole lifecycle for one request:
    /// propose -> policy + reserve -> build -> approve -> authorize -> sign ->
    /// record `Submitted` -> send -> confirm.
    pub fn pay(
        &mut self,
        raw_request: &str,
        proposal: &PaymentProposal,
        approver: &dyn Approver,
    ) -> Result<PayOutcome> {
        let r = PaymentRef::generate();
        let now = (self.clock)();
        self.store.propose(&r, raw_request, proposal, now)?;

        match self.store.reserve(&self.policy, &r, proposal, now)? {
            PolicyDecision::Rejected(reason) => Ok(PayOutcome::Rejected {
                payment_ref: r,
                reason,
            }),
            PolicyDecision::Approved(validated) => self.execute(r, validated, approver),
        }
    }

    fn fail(&mut self, r: PaymentRef, reason: String) -> Result<PayOutcome> {
        self.store.fail(&r, (self.clock)(), &reason)?;
        Ok(PayOutcome::Failed {
            payment_ref: r,
            reason,
        })
    }

    fn execute(
        &mut self,
        r: PaymentRef,
        v: ValidatedPayment,
        approver: &dyn Approver,
    ) -> Result<PayOutcome> {
        let payer = self.signer.pubkey();

        // 1. Can the wallet cover it? Fail early, before bothering the user.
        match self.chain.balance(&payer) {
            Ok(balance) => {
                let enough = v
                    .amount()
                    .checked_add(FEE_BUFFER)
                    .map_or(false, |need| balance >= need);
                if !enough {
                    return self.fail(
                        r,
                        format!(
                            "insufficient balance: wallet has {balance}, needs {} plus fees",
                            v.amount()
                        ),
                    );
                }
            }
            Err(e) => return self.fail(r, format!("could not check wallet balance: {e}")),
        }

        // 2. Build the message the user will review, and decode it back.
        let review_hash = match self.chain.latest_blockhash() {
            Ok((hash, _)) => hash,
            Err(e) => return self.fail(r, format!("could not get a blockhash: {e}")),
        };
        let review_msg = build_message(&v, &payer, review_hash);
        let decoded = match verify_matches(&review_msg, &v) {
            Ok(d) => d,
            Err(e) => return self.fail(r, format!("internal consistency check failed: {e}")),
        };

        // 3. Explicit approval, rendered from the decoded message.
        let summary = self.summary(&r, &v, &decoded)?;
        if !approver.approve(&summary) {
            self.store.decline(&r, (self.clock)())?;
            return Ok(PayOutcome::Declined { payment_ref: r });
        }

        // 4. The user may have deliberated for a while: rebuild with a fresh
        //    blockhash and re-verify it is still exactly the approved payment.
        let (fresh_hash, last_valid) = match self.chain.latest_blockhash() {
            Ok(x) => x,
            Err(e) => return self.fail(r, format!("could not get a fresh blockhash: {e}")),
        };
        let msg = build_message(&v, &payer, fresh_hash);
        if let Err(e) = verify_matches(&msg, &v) {
            return self.fail(r, format!("internal consistency check failed: {e}"));
        }

        // 5. Authorize, then sign. Nothing has left the machine yet.
        self.store.authorize(&r, (self.clock)())?;
        let signed = match self.signer.sign(&msg) {
            Ok(s) => s,
            Err(e) => return self.fail(r, format!("signing refused: {e}")),
        };
        let signature = signed.signature;

        // 6. Record `Submitted` + signature BEFORE the network send. If we crash
        //    or time out from here on, `reconcile` can still find this payment.
        self.store
            .mark_submitted(&r, &signature.to_string(), last_valid, (self.clock)())?;

        // 7. Send. An error here is ambiguous, so we keep going and look at the chain.
        let send_error = self
            .chain
            .send(&signed.transaction)
            .err()
            .map(|e| e.to_string());

        // 8. Wait for the outcome.
        for attempt in 0..self.confirm.attempts {
            match self.check_once(&r, &signature, last_valid)? {
                Resolution::Confirmed => {
                    return Ok(PayOutcome::Confirmed {
                        payment_ref: r,
                        signature,
                    })
                }
                Resolution::Failed(reason) => {
                    return Ok(PayOutcome::Failed {
                        payment_ref: r,
                        reason,
                    })
                }
                Resolution::Pending => {
                    if attempt + 1 < self.confirm.attempts && !self.confirm.interval.is_zero() {
                        std::thread::sleep(self.confirm.interval);
                    }
                }
            }
        }
        let detail = match send_error {
            Some(e) => {
                format!("send reported an error ({e}); it may still have landed. Run `reconcile`.")
            }
            None => "sent, not yet confirmed. Run `reconcile`.".to_string(),
        };
        Ok(PayOutcome::Pending {
            payment_ref: r,
            signature,
            detail,
        })
    }

    fn summary(
        &self,
        r: &PaymentRef,
        v: &ValidatedPayment,
        d: &DecodedPayment,
    ) -> Result<ApprovalSummary> {
        // Includes this payment's own reservation.
        let spent = self.store.spent_in_window((self.clock)())?;
        Ok(ApprovalSummary {
            payment_ref: r.clone(),
            from: d.from,
            to: d.to,
            recipient_label: v.recipient_label().map(str::to_string),
            amount: d.lamports,
            memo: d.memo.clone(),
            note: v.note().map(str::to_string),
            budget_remaining_after: self
                .policy
                .daily_cap()
                .checked_sub(spent)
                .unwrap_or(Lamports::ZERO),
        })
    }

    /// Look at the chain once and settle the payment if the answer is final.
    fn check_once(
        &mut self,
        r: &PaymentRef,
        sig: &Signature,
        last_valid: u64,
    ) -> Result<Resolution> {
        let first = match self.chain.status(sig) {
            Ok(s) => s,
            Err(_) => return Ok(Resolution::Pending), // cannot tell; stay Submitted
        };
        if let Some(done) = self.settle(r, &first)? {
            return Ok(done);
        }

        // Not settled. Only give up once the blockhash is well and truly dead.
        let expired = matches!(self.chain.current_block_height(), Ok(h) if h > last_valid.saturating_add(EXPIRY_MARGIN_BLOCKS));
        if !expired {
            return Ok(Resolution::Pending);
        }
        // One last look, so a transaction that landed at the buzzer is not written off.
        match self.chain.status(sig) {
            Ok(TxStatus::NotFound) => {
                let reason = "transaction never landed and its blockhash expired".to_string();
                self.store.fail(r, (self.clock)(), &reason)?;
                Ok(Resolution::Failed(reason))
            }
            Ok(other) => Ok(self.settle(r, &other)?.unwrap_or(Resolution::Pending)),
            Err(_) => Ok(Resolution::Pending),
        }
    }

    /// Apply a final on-chain status. `None` means "not final yet".
    fn settle(&mut self, r: &PaymentRef, status: &TxStatus) -> Result<Option<Resolution>> {
        let now = (self.clock)();
        match status {
            TxStatus::Confirmed => {
                self.store.confirm(r, now)?;
                Ok(Some(Resolution::Confirmed))
            }
            TxStatus::Failed(e) => {
                let reason = format!("failed on chain: {e}");
                self.store.fail(r, now, &reason)?;
                Ok(Some(Resolution::Failed(reason)))
            }
            // `Pending` (seen, not yet confirmed) must NOT be written off, even after
            // expiry: it was included in a block and may still confirm.
            TxStatus::Pending | TxStatus::NotFound => Ok(None),
        }
    }

    /// Settle everything left in flight, e.g. after a crash.
    /// - `Submitted`: ask the chain; fail only once the blockhash has expired.
    /// - `Authorized` / `Reserved` and stale: never sent, so release the budget.
    pub fn reconcile(&mut self) -> Result<Vec<ReconcileReport>> {
        use crate::domain::PaymentState::{Authorized, Reserved, Submitted};
        let mut reports = Vec::new();

        for rec in self.store.by_state(Submitted)? {
            let sig = rec
                .signature
                .as_deref()
                .ok_or_else(|| {
                    Error::Corrupt(format!(
                        "{} is submitted but has no signature",
                        rec.payment_ref
                    ))
                })
                .and_then(|s| {
                    Signature::from_str(s)
                        .map_err(|e| Error::Corrupt(format!("bad stored signature: {e}")))
                })?;
            let last_valid = rec.last_valid_block_height.ok_or_else(|| {
                Error::Corrupt(format!(
                    "{} is submitted but has no block height",
                    rec.payment_ref
                ))
            })?;
            let result = match self.check_once(&rec.payment_ref, &sig, last_valid)? {
                Resolution::Confirmed => ReconcileResult::Confirmed,
                Resolution::Failed(reason) => ReconcileResult::Failed(reason),
                Resolution::Pending => ReconcileResult::StillPending,
            };
            reports.push(ReconcileReport {
                payment_ref: rec.payment_ref,
                result,
            });
        }

        let now = (self.clock)();
        for (state, why) in [
            (
                Authorized,
                "interrupted after approval, before the transaction was sent",
            ),
            (Reserved, "abandoned before approval"),
        ] {
            for rec in self.store.by_state(state)? {
                if now - rec.updated_at >= STALE_PENDING_SECS {
                    self.store.fail(&rec.payment_ref, now, why)?;
                    reports.push(ReconcileReport {
                        payment_ref: rec.payment_ref,
                        result: ReconcileResult::Abandoned,
                    });
                }
            }
        }
        Ok(reports)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::sync::Arc;

    use solana_sdk::signature::Keypair;

    use super::*;
    use crate::domain::PaymentState;
    use crate::policy::Allowlist;
    use crate::solana::builder::decode_message;
    use crate::solana::client::MockChain;

    const T0: i64 = 2_000_000;

    fn sol(s: &str) -> Lamports {
        Lamports::parse_sol(s).unwrap()
    }

    struct Scripted {
        answer: bool,
        calls: Cell<u32>,
        last: RefCell<Option<ApprovalSummary>>,
    }
    impl Scripted {
        fn new(answer: bool) -> Self {
            Self {
                answer,
                calls: Cell::new(0),
                last: RefCell::new(None),
            }
        }
    }
    impl Approver for Scripted {
        fn approve(&self, s: &ApprovalSummary) -> bool {
            self.calls.set(self.calls.get() + 1);
            *self.last.borrow_mut() = Some(s.clone());
            self.answer
        }
    }

    struct H {
        svc: PaymentService<Arc<MockChain>>,
        chain: Arc<MockChain>,
        clock: Arc<AtomicI64>,
        ada: Pubkey,
    }

    /// per-tx 0.5, daily 1.0, allowlist: Ada. Mock chain auto-confirms.
    fn harness() -> H {
        let chain = Arc::new(MockChain::new());
        chain.set_auto_confirm(true);
        let clock = Arc::new(AtomicI64::new(T0));
        let ada = Pubkey::new_unique();
        let policy = Policy::new(
            sol("0.5"),
            sol("1"),
            Allowlist::new([("Ada".to_string(), ada)]).unwrap(),
        )
        .unwrap();
        let c = clock.clone();
        let svc = PaymentService::new(
            PaymentStore::open_in_memory().unwrap(),
            policy,
            chain.clone(),
            FileSigner::from_keypair(Keypair::new()),
        )
        .with_clock(Box::new(move || c.load(Ordering::SeqCst)))
        .with_confirm(ConfirmSettings {
            attempts: 3,
            interval: Duration::ZERO,
        });
        H {
            svc,
            chain,
            clock,
            ada,
        }
    }

    fn req(recipient: &str, amount: &str) -> PaymentProposal {
        PaymentProposal {
            recipient: recipient.into(),
            amount: amount.into(),
            note: Some("invoice".into()),
        }
    }

    fn state_of(h: &H, r: &PaymentRef) -> PaymentState {
        h.svc.store().get(r).unwrap().state
    }

    fn pending_of(o: PayOutcome) -> (PaymentRef, Signature) {
        match o {
            PayOutcome::Pending {
                payment_ref,
                signature,
                ..
            } => (payment_ref, signature),
            other => panic!("expected Pending, got {other:?}"),
        }
    }

    #[test]
    fn happy_path_signs_exactly_the_approved_payment() {
        let mut h = harness();
        let out = h
            .svc
            .pay("pay ada 0.1", &req("ada", "0.1 SOL"), &Scripted::new(true))
            .unwrap();
        let (r, sig) = match out {
            PayOutcome::Confirmed {
                payment_ref,
                signature,
            } => (payment_ref, signature),
            other => panic!("{other:?}"),
        };
        assert_eq!(state_of(&h, &r), PaymentState::Confirmed);

        let sent = h.chain.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].signatures[0], sig);
        assert!(sent[0].verify_with_results().iter().all(|ok| *ok));
        let d = decode_message(&sent[0].message).unwrap();
        assert_eq!(d.from, h.svc.wallet_pubkey());
        assert_eq!(d.to, h.ada);
        assert_eq!(d.lamports, sol("0.1"));
        assert_eq!(d.memo, format!("spendguard:{r}"));

        let rec = h.svc.store().get(&r).unwrap();
        assert_eq!(rec.signature.as_deref(), Some(sig.to_string().as_str()));
        let path: Vec<_> = h
            .svc
            .store()
            .state_log(&r)
            .unwrap()
            .iter()
            .map(|e| e.to)
            .collect();
        assert_eq!(
            path,
            vec![
                PaymentState::Proposed,
                PaymentState::Reserved,
                PaymentState::Authorized,
                PaymentState::Submitted,
                PaymentState::Confirmed
            ]
        );
    }

    #[test]
    fn approval_summary_comes_from_the_built_message() {
        let mut h = harness();
        let approver = Scripted::new(true);
        h.svc
            .pay("pay ada 0.1", &req("ADA", "0.1"), &approver)
            .unwrap();
        let s = approver.last.borrow().clone().unwrap();
        assert_eq!(s.from, h.svc.wallet_pubkey());
        assert_eq!(s.to, h.ada);
        assert_eq!(s.recipient_label.as_deref(), Some("Ada"));
        assert_eq!(s.amount, sol("0.1"));
        assert_eq!(s.memo, format!("spendguard:{}", s.payment_ref));
        assert_eq!(s.note.as_deref(), Some("invoice"));
        assert_eq!(s.budget_remaining_after, sol("0.9"));
    }

    // The headline demo.
    #[test]
    fn policy_rejection_never_reaches_approval_or_the_chain() {
        let mut h = harness();
        let approver = Scripted::new(true);
        let out = h
            .svc
            .pay("pay ada 0.6", &req("ada", "0.6"), &approver)
            .unwrap();
        match out {
            PayOutcome::Rejected {
                payment_ref,
                reason,
            } => {
                assert_eq!(reason.code(), "exceeds_per_tx_limit");
                assert_eq!(state_of(&h, &payment_ref), PaymentState::PolicyRejected);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(approver.calls.get(), 0);
        assert!(h.chain.sent().is_empty());
        assert_eq!(h.svc.store().spent_in_window(T0).unwrap(), Lamports::ZERO);
    }

    #[test]
    fn unknown_recipient_is_rejected() {
        let mut h = harness();
        let out = h
            .svc
            .pay("pay mallory", &req("mallory", "0.1"), &Scripted::new(true))
            .unwrap();
        assert!(
            matches!(out, PayOutcome::Rejected { ref reason, .. } if reason.code() == "unknown_recipient")
        );
        assert!(h.chain.sent().is_empty());
    }

    #[test]
    fn daily_limit_is_enforced_across_payments() {
        let mut h = harness();
        for _ in 0..2 {
            let o = h
                .svc
                .pay("x", &req("ada", "0.5"), &Scripted::new(true))
                .unwrap();
            assert!(matches!(o, PayOutcome::Confirmed { .. }));
        }
        let o = h
            .svc
            .pay("x", &req("ada", "0.1"), &Scripted::new(true))
            .unwrap();
        assert!(
            matches!(o, PayOutcome::Rejected { ref reason, .. } if reason.code() == "exceeds_daily_limit")
        );
        assert_eq!(h.chain.sent().len(), 2);
    }

    #[test]
    fn declining_sends_nothing_and_frees_the_budget() {
        let mut h = harness();
        let out = h
            .svc
            .pay("pay ada", &req("ada", "0.4"), &Scripted::new(false))
            .unwrap();
        match out {
            PayOutcome::Declined { payment_ref } => {
                assert_eq!(state_of(&h, &payment_ref), PaymentState::UserDeclined)
            }
            other => panic!("{other:?}"),
        }
        assert!(h.chain.sent().is_empty());
        assert_eq!(h.svc.store().spent_in_window(T0).unwrap(), Lamports::ZERO);
    }

    #[test]
    fn insufficient_balance_fails_before_asking_the_user() {
        let mut h = harness();
        h.chain.set_balance(sol("0.05"));
        let approver = Scripted::new(true);
        let out = h.svc.pay("pay ada", &req("ada", "0.1"), &approver).unwrap();
        match out {
            PayOutcome::Failed {
                payment_ref,
                reason,
            } => {
                assert!(reason.contains("insufficient balance"));
                assert_eq!(state_of(&h, &payment_ref), PaymentState::Failed);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(approver.calls.get(), 0);
        assert_eq!(h.svc.store().spent_in_window(T0).unwrap(), Lamports::ZERO);
    }

    #[test]
    fn send_timeout_leaves_a_recoverable_submitted_payment() {
        let mut h = harness();
        h.chain.fail_sends_with("timeout");
        let (r, sig) = pending_of(
            h.svc
                .pay("pay ada", &req("ada", "0.4"), &Scripted::new(true))
                .unwrap(),
        );

        // recorded as Submitted with its signature, and still counted against the budget
        let rec = h.svc.store().get(&r).unwrap();
        assert_eq!(rec.state, PaymentState::Submitted);
        assert_eq!(rec.signature.as_deref(), Some(sig.to_string().as_str()));
        assert_eq!(h.svc.store().spent_in_window(T0).unwrap(), sol("0.4"));

        // chain knows nothing and the blockhash is still valid: keep waiting
        let reports = h.svc.reconcile().unwrap();
        assert_eq!(
            reports,
            vec![ReconcileReport {
                payment_ref: r.clone(),
                result: ReconcileResult::StillPending
            }]
        );
        assert_eq!(state_of(&h, &r), PaymentState::Submitted);

        // it turns out the transaction DID land despite the send error
        h.chain.set_status(sig, TxStatus::Confirmed);
        let reports = h.svc.reconcile().unwrap();
        assert_eq!(reports[0].result, ReconcileResult::Confirmed);
        assert_eq!(state_of(&h, &r), PaymentState::Confirmed);
        assert_eq!(h.svc.store().spent_in_window(T0).unwrap(), sol("0.4"));
    }

    #[test]
    fn expired_unlanded_transaction_is_failed_and_frees_the_budget() {
        let mut h = harness();
        h.chain.fail_sends_with("timeout");
        let (r, _) = pending_of(
            h.svc
                .pay("pay ada", &req("ada", "0.4"), &Scripted::new(true))
                .unwrap(),
        );

        let (_, last_valid) = h.chain.latest_blockhash().unwrap();
        h.chain.set_height(last_valid + EXPIRY_MARGIN_BLOCKS); // exactly at the margin: not dead yet
        assert_eq!(
            h.svc.reconcile().unwrap()[0].result,
            ReconcileResult::StillPending
        );

        h.chain.set_height(last_valid + EXPIRY_MARGIN_BLOCKS + 1);
        let reports = h.svc.reconcile().unwrap();
        assert!(
            matches!(reports[0].result, ReconcileResult::Failed(ref why) if why.contains("expired"))
        );
        assert_eq!(state_of(&h, &r), PaymentState::Failed);
        assert_eq!(h.svc.store().spent_in_window(T0).unwrap(), Lamports::ZERO);
    }

    #[test]
    fn a_transaction_that_lands_at_the_buzzer_is_not_written_off() {
        let mut h = harness();
        h.chain.fail_sends_with("timeout");
        let (r, sig) = pending_of(
            h.svc
                .pay("pay ada", &req("ada", "0.4"), &Scripted::new(true))
                .unwrap(),
        );
        let (_, last_valid) = h.chain.latest_blockhash().unwrap();
        h.chain.set_height(last_valid + EXPIRY_MARGIN_BLOCKS + 100);
        h.chain.set_status(sig, TxStatus::Confirmed);
        assert_eq!(
            h.svc.reconcile().unwrap()[0].result,
            ReconcileResult::Confirmed
        );
        assert_eq!(state_of(&h, &r), PaymentState::Confirmed);
    }

    #[test]
    fn seen_but_unconfirmed_is_never_failed_even_after_expiry() {
        let mut h = harness();
        h.chain.set_auto_confirm(false); // send succeeds, status stays Pending
        let (r, _) = pending_of(
            h.svc
                .pay("pay ada", &req("ada", "0.4"), &Scripted::new(true))
                .unwrap(),
        );
        let (_, last_valid) = h.chain.latest_blockhash().unwrap();
        h.chain.set_height(last_valid + EXPIRY_MARGIN_BLOCKS + 100);
        assert_eq!(
            h.svc.reconcile().unwrap()[0].result,
            ReconcileResult::StillPending
        );
        assert_eq!(state_of(&h, &r), PaymentState::Submitted);
        assert_eq!(h.svc.store().spent_in_window(T0).unwrap(), sol("0.4"));
    }

    #[test]
    fn on_chain_failure_is_recorded_and_frees_the_budget() {
        let mut h = harness();
        h.chain.set_auto_confirm(false);
        let (r, sig) = pending_of(
            h.svc
                .pay("pay ada", &req("ada", "0.4"), &Scripted::new(true))
                .unwrap(),
        );
        h.chain
            .set_status(sig, TxStatus::Failed("InsufficientFundsForFee".into()));
        let reports = h.svc.reconcile().unwrap();
        assert!(
            matches!(reports[0].result, ReconcileResult::Failed(ref why) if why.contains("InsufficientFundsForFee"))
        );
        assert_eq!(state_of(&h, &r), PaymentState::Failed);
        assert_eq!(h.svc.store().spent_in_window(T0).unwrap(), Lamports::ZERO);
    }

    #[test]
    fn stale_unsent_payments_are_released_but_fresh_ones_are_left_alone() {
        let mut h = harness();
        let policy = h.svc.policy().clone();
        let p = req("ada", "0.2");

        let reserved = PaymentRef::generate();
        h.svc.store_mut().propose(&reserved, "x", &p, T0).unwrap();
        h.svc
            .store_mut()
            .reserve(&policy, &reserved, &p, T0)
            .unwrap();

        let authorized = PaymentRef::generate();
        h.svc.store_mut().propose(&authorized, "x", &p, T0).unwrap();
        h.svc
            .store_mut()
            .reserve(&policy, &authorized, &p, T0)
            .unwrap();
        h.svc.store_mut().authorize(&authorized, T0).unwrap();

        // too fresh: might belong to a live `pay` in another process
        h.clock.store(T0 + 10, Ordering::SeqCst);
        assert!(h.svc.reconcile().unwrap().is_empty());
        assert_eq!(h.svc.store().spent_in_window(T0 + 10).unwrap(), sol("0.4"));

        h.clock.store(T0 + STALE_PENDING_SECS, Ordering::SeqCst);
        let reports = h.svc.reconcile().unwrap();
        assert_eq!(reports.len(), 2);
        assert!(reports
            .iter()
            .all(|r| r.result == ReconcileResult::Abandoned));
        assert_eq!(state_of(&h, &reserved), PaymentState::Failed);
        assert_eq!(state_of(&h, &authorized), PaymentState::Failed);
        assert_eq!(
            h.svc
                .store()
                .spent_in_window(T0 + STALE_PENDING_SECS)
                .unwrap(),
            Lamports::ZERO
        );
    }

    #[test]
    fn the_service_applies_the_policys_day_boundary() {
        use crate::policy::BudgetWindow;
        let chain = Arc::new(MockChain::new());
        chain.set_auto_confirm(true);
        let clock = Arc::new(AtomicI64::new(950_400 + 3_600)); // 01:00 UTC
        let policy = Policy::new(
            sol("0.5"),
            sol("1"),
            Allowlist::new([("Ada".to_string(), Pubkey::new_unique())]).unwrap(),
        )
        .unwrap()
        .with_window(BudgetWindow::UtcDay);
        let c = clock.clone();
        let mut svc = PaymentService::new(
            PaymentStore::open_in_memory().unwrap(),
            policy,
            chain,
            FileSigner::from_keypair(Keypair::new()),
        )
        .with_clock(Box::new(move || c.load(Ordering::SeqCst)))
        .with_confirm(ConfirmSettings {
            attempts: 2,
            interval: Duration::ZERO,
        });

        assert!(matches!(
            svc.pay("x", &req("ada", "0.5"), &Scripted::new(true))
                .unwrap(),
            PayOutcome::Confirmed { .. }
        ));
        assert_eq!(
            svc.store().spent_in_window(950_400 + 86_399).unwrap(),
            sol("0.5")
        );
        clock.store(950_400 + 86_400, Ordering::SeqCst); // midnight UTC
        assert_eq!(
            svc.store().spent_in_window(950_400 + 86_400).unwrap(),
            Lamports::ZERO
        );
        assert!(matches!(
            svc.pay("x", &req("ada", "0.5"), &Scripted::new(true))
                .unwrap(),
            PayOutcome::Confirmed { .. }
        ));
    }

    #[test]
    fn reconcile_with_nothing_in_flight_is_a_no_op() {
        let mut h = harness();
        h.svc
            .pay("x", &req("ada", "0.1"), &Scripted::new(true))
            .unwrap();
        assert!(h.svc.reconcile().unwrap().is_empty());
    }
}
