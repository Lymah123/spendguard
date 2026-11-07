//! Kill the process after a payment is signed and recorded, restart, reconcile.
//! Uses only the public API, against a real on-disk SQLite file.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signature};

use spendguard::audit::PaymentStore;
use spendguard::domain::{Lamports, PaymentProposal, PaymentRef, PaymentState};
use spendguard::policy::{Allowlist, Policy};
use spendguard::service::{
    ApprovalSummary, Approver, ConfirmSettings, PayOutcome, PaymentService, ReconcileResult,
    EXPIRY_MARGIN_BLOCKS,
};
use spendguard::solana::client::{ChainClient, MockChain, TxStatus};
use spendguard::wallet::signer::FileSigner;

struct Yes;
impl Approver for Yes {
    fn approve(&self, _: &ApprovalSummary) -> bool {
        true
    }
}

struct TempDb(PathBuf);
impl Drop for TempDb {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.0.display()));
        }
    }
}

fn sol(s: &str) -> Lamports {
    Lamports::parse_sol(s).unwrap()
}

fn policy(ada: Pubkey) -> Policy {
    Policy::new(
        sol("0.5"),
        sol("1"),
        Allowlist::new([("Ada".to_string(), ada)]).unwrap(),
    )
    .unwrap()
}

fn service(db: &TempDb, ada: Pubkey, chain: Arc<MockChain>) -> PaymentService<Arc<MockChain>> {
    PaymentService::new(
        PaymentStore::open(&db.0).unwrap(),
        policy(ada),
        chain,
        FileSigner::from_keypair(Keypair::new()),
    )
    .with_confirm(ConfirmSettings {
        attempts: 2,
        interval: Duration::ZERO,
    })
}

fn req(amount: &str) -> PaymentProposal {
    PaymentProposal {
        recipient: "ada".into(),
        amount: amount.into(),
        note: None,
    }
}

/// Process 1: sign, record, and "crash" on a send timeout. Returns what the DB holds.
fn first_run(db: &TempDb, ada: Pubkey) -> (PaymentRef, Signature, u64) {
    let chain = Arc::new(MockChain::new());
    chain.fail_sends_with("connection reset");
    let (_, last_valid) = chain.latest_blockhash().unwrap();
    let mut svc = service(db, ada, chain);
    match svc.pay("pay ada 0.5", &req("0.5"), &Yes).unwrap() {
        PayOutcome::Pending {
            payment_ref,
            signature,
            ..
        } => (payment_ref, signature, last_valid),
        other => panic!("expected Pending, got {other:?}"),
    }
    // `svc` is dropped here: the process is gone.
}

#[test]
fn restart_then_reconcile_confirms_a_payment_that_landed() {
    let db = TempDb(std::env::temp_dir().join(format!("sg-recovery-{}.db", uuid::Uuid::new_v4())));
    let ada = Pubkey::new_unique();
    let (r, sig, _) = first_run(&db, ada);

    // Process 2: a fresh store handle on the same file, and a chain that knows the tx landed.
    let chain = Arc::new(MockChain::new());
    chain.set_status(sig, TxStatus::Confirmed);
    let mut svc = service(&db, ada, chain);

    assert_eq!(svc.store().get(&r).unwrap().state, PaymentState::Submitted);
    let reports = svc.reconcile().unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].result, ReconcileResult::Confirmed);
    assert_eq!(svc.store().get(&r).unwrap().state, PaymentState::Confirmed);
}

#[test]
fn restart_cannot_be_used_to_overspend_the_daily_limit() {
    let db = TempDb(std::env::temp_dir().join(format!("sg-recovery-{}.db", uuid::Uuid::new_v4())));
    let ada = Pubkey::new_unique();
    first_run(&db, ada); // 0.5 SOL is in flight and unresolved

    let chain = Arc::new(MockChain::new());
    chain.set_auto_confirm(true);
    let mut svc = service(&db, ada, chain.clone());

    // 0.5 more fits (total 1.0), but the in-flight 0.5 still counts, so 0.1 on top is refused
    assert!(matches!(
        svc.pay("x", &req("0.5"), &Yes).unwrap(),
        PayOutcome::Confirmed { .. }
    ));
    match svc.pay("x", &req("0.1"), &Yes).unwrap() {
        PayOutcome::Rejected { reason, .. } => assert_eq!(reason.code(), "exceeds_daily_limit"),
        other => panic!("{other:?}"),
    }
    assert_eq!(chain.sent().len(), 1);
}

#[test]
fn restart_then_reconcile_releases_a_payment_that_never_landed() {
    let db = TempDb(std::env::temp_dir().join(format!("sg-recovery-{}.db", uuid::Uuid::new_v4())));
    let ada = Pubkey::new_unique();
    let (r, _, last_valid) = first_run(&db, ada);

    let chain = Arc::new(MockChain::new()); // knows nothing about the signature
    let mut svc = service(&db, ada, chain.clone());

    // still within the blockhash's life: the payment stays pending and keeps its budget
    chain.set_height(last_valid);
    assert_eq!(
        svc.reconcile().unwrap()[0].result,
        ReconcileResult::StillPending
    );
    assert!(
        svc.store()
            .spent_in_window(spendguard::audit::unix_now())
            .unwrap()
            > Lamports::ZERO
    );

    // once the blockhash is dead and nothing landed, it is failed and the budget comes back
    chain.set_height(last_valid + EXPIRY_MARGIN_BLOCKS + 1);
    assert!(matches!(
        svc.reconcile().unwrap()[0].result,
        ReconcileResult::Failed(_)
    ));
    assert_eq!(svc.store().get(&r).unwrap().state, PaymentState::Failed);
    assert_eq!(
        svc.store()
            .spent_in_window(spendguard::audit::unix_now())
            .unwrap(),
        Lamports::ZERO
    );
}
