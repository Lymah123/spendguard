use rusqlite::{params, Connection, TransactionBehavior};

use crate::audit::store::{current_state, transition_tx, PaymentStore};
use crate::domain::{Lamports, PaymentProposal, PaymentRef, PaymentState};
use crate::error::{Error, Result};
use crate::policy::{BudgetWindow, Policy, PolicyDecision};

/// Length of a day in seconds.
pub const WINDOW_SECS: i64 = 86_400;

/// First second (inclusive) that still counts toward the daily limit.
/// - `Rolling24h`: a payment counts from its reservation until exactly 24h later.
/// - `UtcDay`: a payment counts until the next 00:00 UTC.
pub fn window_start(now: i64, window: BudgetWindow) -> i64 {
    match window {
        BudgetWindow::Rolling24h => now - WINDOW_SECS + 1,
        BudgetWindow::UtcDay => now - now.rem_euclid(WINDOW_SECS),
    }
}

/// Sum of amounts that currently count against the budget. The set of states
/// comes from `PaymentState::counts_against_budget`, so the lifecycle and the
/// accounting cannot drift apart.
pub(super) fn sum_in_window(conn: &Connection, now: i64, window: BudgetWindow) -> Result<Lamports> {
    let states: Vec<String> = PaymentState::ALL
        .into_iter()
        .filter(|s| s.counts_against_budget())
        .map(|s| format!("'{}'", s.as_str())) // our own constants, never user input
        .collect();
    let sql = format!(
        "SELECT COALESCE(SUM(amount_lamports), 0) FROM payments \
         WHERE state IN ({}) AND reserved_at >= ?1",
        states.join(", ")
    );
    let total: i64 = conn.query_row(&sql, [window_start(now, window)], |r| r.get(0))?;
    let total = u64::try_from(total).map_err(|_| Error::Corrupt("negative budget total".into()))?;
    Ok(Lamports::new(total))
}

impl PaymentStore {
    pub fn spent_in_window(&self, now: i64) -> Result<Lamports> {
        sum_in_window(&self.conn, now, self.window)
    }

    /// Run policy and, if it approves, reserve the amount, atomically.
    ///
    /// `BEGIN IMMEDIATE` takes SQLite's write lock before the budget is read, so
    /// two processes cannot both see the same total and both pass. The policy
    /// check runs inside the transaction, against the total read inside it.
    ///
    /// Proposed -> Reserved (approved) or Proposed -> PolicyRejected (rejected).
    pub fn reserve(
        &mut self,
        policy: &Policy,
        r: &PaymentRef,
        proposal: &PaymentProposal,
        now: i64,
    ) -> Result<PolicyDecision> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;

        let state = current_state(&tx, r)?;
        if state != PaymentState::Proposed {
            return Err(Error::InvalidTransition {
                from: state,
                to: PaymentState::Reserved,
            });
        }

        let spent = sum_in_window(&tx, now, policy.window())?;
        let decision = policy.evaluate(r.clone(), proposal, spent);

        match &decision {
            PolicyDecision::Approved(v) => {
                let amount = i64::try_from(v.amount().as_u64()).map_err(|_| {
                    Error::Corrupt("approved amount does not fit in SQLite INTEGER".into())
                })?;
                transition_tx(&tx, r, PaymentState::Reserved, now, None)?;
                tx.execute(
                    "UPDATE payments SET recipient_address = ?1, recipient_label = ?2, \
                     amount_lamports = ?3, reserved_at = ?4 WHERE payment_ref = ?5",
                    params![
                        v.recipient().to_string(),
                        v.recipient_label(),
                        amount,
                        now,
                        r.as_str()
                    ],
                )?;
            }
            PolicyDecision::Rejected(reason) => {
                let text = reason.to_string();
                transition_tx(&tx, r, PaymentState::PolicyRejected, now, Some(&text))?;
                tx.execute(
                    "UPDATE payments SET reject_code = ?1, reject_reason = ?2 WHERE payment_ref = ?3",
                    params![reason.code(), text, r.as_str()],
                )?;
            }
        }

        tx.commit()?;
        Ok(decision)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use solana_sdk::pubkey::Pubkey;

    use super::*;
    use crate::error::Error;
    use crate::policy::Allowlist;

    const T0: i64 = 1_000_000;

    fn sol(s: &str) -> Lamports {
        Lamports::parse_sol(s).unwrap()
    }

    /// per-tx 0.5 SOL, daily 1 SOL, allowlist: Ada
    fn policy() -> Policy {
        let allow = Allowlist::new([("Ada".to_string(), Pubkey::new_unique())]).unwrap();
        Policy::new(sol("0.5"), sol("1"), allow).unwrap()
    }

    fn request(
        s: &mut PaymentStore,
        recipient: &str,
        amount: &str,
        now: i64,
    ) -> (PaymentRef, PaymentProposal) {
        let r = PaymentRef::generate();
        let p = PaymentProposal {
            recipient: recipient.into(),
            amount: amount.into(),
            note: None,
        };
        s.propose(&r, &format!("pay {recipient} {amount}"), &p, now)
            .unwrap();
        (r, p)
    }

    fn approved(s: &mut PaymentStore, pol: &Policy, amount: &str, now: i64) -> PaymentRef {
        let (r, p) = request(s, "ada", amount, now);
        assert!(matches!(
            s.reserve(pol, &r, &p, now).unwrap(),
            PolicyDecision::Approved(_)
        ));
        r
    }

    struct TempDb(PathBuf);
    impl TempDb {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!("spendguard-test-{}.db", uuid::Uuid::new_v4())))
        }
    }
    impl Drop for TempDb {
        fn drop(&mut self) {
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{suffix}", self.0.display()));
            }
        }
    }

    #[test]
    fn reserve_approved_records_everything() {
        let (mut s, pol) = (PaymentStore::open_in_memory().unwrap(), policy());
        let r = approved(&mut s, &pol, "0.4", T0);
        let rec = s.get(&r).unwrap();
        assert_eq!(rec.state, PaymentState::Reserved);
        assert_eq!(rec.amount, Some(sol("0.4")));
        assert_eq!(rec.recipient_label.as_deref(), Some("Ada"));
        assert_eq!(rec.reserved_at, Some(T0));
        assert_eq!(s.spent_in_window(T0).unwrap(), sol("0.4"));
    }

    #[test]
    fn rejection_is_recorded_and_costs_no_budget() {
        let (mut s, pol) = (PaymentStore::open_in_memory().unwrap(), policy());
        let (r, p) = request(&mut s, "ada", "0.6", T0);
        assert!(matches!(
            s.reserve(&pol, &r, &p, T0).unwrap(),
            PolicyDecision::Rejected(_)
        ));
        let rec = s.get(&r).unwrap();
        assert_eq!(rec.state, PaymentState::PolicyRejected);
        assert_eq!(rec.reject_code.as_deref(), Some("exceeds_per_tx_limit"));
        assert!(rec.reject_reason.unwrap().contains("per-transaction limit"));
        assert_eq!(rec.amount, None);
        assert_eq!(s.spent_in_window(T0).unwrap(), Lamports::ZERO);
    }

    #[test]
    fn budget_accumulates_across_payments() {
        let (mut s, pol) = (PaymentStore::open_in_memory().unwrap(), policy());
        approved(&mut s, &pol, "0.4", T0);
        approved(&mut s, &pol, "0.4", T0 + 1);
        let (r, p) = request(&mut s, "ada", "0.4", T0 + 2);
        match s.reserve(&pol, &r, &p, T0 + 2).unwrap() {
            PolicyDecision::Rejected(reason) => assert_eq!(reason.code(), "exceeds_daily_limit"),
            other => panic!("{other:?}"),
        }
        assert_eq!(s.spent_in_window(T0 + 2).unwrap(), sol("0.8"));
    }

    #[test]
    fn decline_and_fail_free_the_budget() {
        let (mut s, pol) = (PaymentStore::open_in_memory().unwrap(), policy());
        let a = approved(&mut s, &pol, "0.4", T0);
        let b = approved(&mut s, &pol, "0.4", T0);
        assert_eq!(s.spent_in_window(T0).unwrap(), sol("0.8"));
        s.decline(&a, T0).unwrap();
        assert_eq!(s.spent_in_window(T0).unwrap(), sol("0.4"));
        s.fail(&b, T0, "build error").unwrap();
        assert_eq!(s.spent_in_window(T0).unwrap(), Lamports::ZERO);
    }

    #[test]
    fn pending_and_confirmed_payments_keep_counting() {
        let (mut s, pol) = (PaymentStore::open_in_memory().unwrap(), policy());
        let r = approved(&mut s, &pol, "0.4", T0);
        s.authorize(&r, T0).unwrap();
        assert_eq!(s.spent_in_window(T0).unwrap(), sol("0.4"));
        s.mark_submitted(&r, "sig123", 500, T0).unwrap();
        assert_eq!(s.spent_in_window(T0).unwrap(), sol("0.4"));
        s.confirm(&r, T0).unwrap();
        assert_eq!(s.spent_in_window(T0).unwrap(), sol("0.4"));
    }

    #[test]
    fn rolling_window_expires_after_24_hours() {
        let (mut s, pol) = (PaymentStore::open_in_memory().unwrap(), policy());
        approved(&mut s, &pol, "0.4", T0);
        assert_eq!(s.spent_in_window(T0 + WINDOW_SECS - 1).unwrap(), sol("0.4"));
        assert_eq!(s.spent_in_window(T0 + WINDOW_SECS).unwrap(), Lamports::ZERO);
    }

    #[test]
    fn utc_day_window_resets_at_midnight_utc() {
        // T0 = 1_000_000 is 13:46:40 into the UTC day that began at 950_400
        let (mut s, _) = (PaymentStore::open_in_memory().unwrap(), ());
        let pol = policy().with_window(BudgetWindow::UtcDay);
        s.set_window(pol.window());
        approved(&mut s, &pol, "0.4", T0);
        let next_midnight = 950_400 + WINDOW_SECS; // 1_036_800
        assert_eq!(s.spent_in_window(T0).unwrap(), sol("0.4"));
        assert_eq!(s.spent_in_window(next_midnight - 1).unwrap(), sol("0.4"));
        assert_eq!(s.spent_in_window(next_midnight).unwrap(), Lamports::ZERO);
    }

    #[test]
    fn utc_day_window_counts_a_payment_made_exactly_at_midnight() {
        let (mut s, pol) = (
            PaymentStore::open_in_memory().unwrap(),
            policy().with_window(BudgetWindow::UtcDay),
        );
        s.set_window(pol.window());
        approved(&mut s, &pol, "0.4", 950_400); // 00:00:00 UTC
        assert_eq!(s.spent_in_window(950_400).unwrap(), sol("0.4"));
        assert_eq!(
            s.spent_in_window(950_400 + WINDOW_SECS - 1).unwrap(),
            sol("0.4")
        );
    }

    #[test]
    fn utc_day_reservation_uses_the_policys_window() {
        // yesterday's 0.9 must not block today's payment under UtcDay, but does under Rolling24h
        let day2 = 950_400 + WINDOW_SECS + 60; // one minute after midnight
        let yesterday = day2 - 120; // 23:59 the previous day
        for (window, expect_ok) in [
            (BudgetWindow::UtcDay, true),
            (BudgetWindow::Rolling24h, false),
        ] {
            let (mut s, pol) = (
                PaymentStore::open_in_memory().unwrap(),
                policy().with_window(window),
            );
            s.set_window(window);
            approved(&mut s, &pol, "0.5", yesterday);
            approved(&mut s, &pol, "0.4", yesterday + 1);
            let (r, p) = request(&mut s, "ada", "0.3", day2);
            let ok = matches!(
                s.reserve(&pol, &r, &p, day2).unwrap(),
                PolicyDecision::Approved(_)
            );
            assert_eq!(ok, expect_ok, "{window:?}");
        }
    }

    #[test]
    fn illegal_transitions_are_refused() {
        let (mut s, pol) = (PaymentStore::open_in_memory().unwrap(), policy());
        let (r, p) = request(&mut s, "ada", "0.1", T0);
        // cannot authorize something policy has not approved
        assert!(matches!(
            s.authorize(&r, T0),
            Err(Error::InvalidTransition { .. })
        ));
        s.reserve(&pol, &r, &p, T0).unwrap();
        // cannot reserve twice
        assert!(matches!(
            s.reserve(&pol, &r, &p, T0),
            Err(Error::InvalidTransition { .. })
        ));
        // cannot submit without authorization
        assert!(matches!(
            s.mark_submitted(&r, "sig", 1, T0),
            Err(Error::InvalidTransition { .. })
        ));
        // terminal states stay terminal
        s.decline(&r, T0).unwrap();
        assert!(s.authorize(&r, T0).is_err());
        assert!(s.fail(&r, T0, "x").is_err());
    }

    #[test]
    fn unknown_payment_is_an_error() {
        let (mut s, pol) = (PaymentStore::open_in_memory().unwrap(), policy());
        let ghost = PaymentRef::from_stored("sg_doesnotexist".into());
        let p = PaymentProposal {
            recipient: "ada".into(),
            amount: "0.1".into(),
            note: None,
        };
        assert!(matches!(s.get(&ghost), Err(Error::PaymentNotFound(_))));
        assert!(matches!(
            s.reserve(&pol, &ghost, &p, T0),
            Err(Error::PaymentNotFound(_))
        ));
    }

    #[test]
    fn duplicate_reference_is_refused() {
        let mut s = PaymentStore::open_in_memory().unwrap();
        let (r, p) = request(&mut s, "ada", "0.1", T0);
        assert!(s.propose(&r, "again", &p, T0).is_err());
    }

    #[test]
    fn state_log_records_the_full_path_in_order() {
        let (mut s, pol) = (PaymentStore::open_in_memory().unwrap(), policy());
        let r = approved(&mut s, &pol, "0.1", T0);
        s.authorize(&r, T0 + 1).unwrap();
        s.mark_submitted(&r, "sigABC", 900, T0 + 2).unwrap();
        s.confirm(&r, T0 + 3).unwrap();
        let log = s.state_log(&r).unwrap();
        let path: Vec<_> = log.iter().map(|e| e.to).collect();
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
        assert_eq!(log[0].from, None);
        assert_eq!(log[3].detail.as_deref(), Some("sigABC"));
        assert_eq!(log[4].at, T0 + 3);
    }

    #[test]
    fn history_is_newest_first() {
        let mut s = PaymentStore::open_in_memory().unwrap();
        let (a, _) = request(&mut s, "ada", "0.1", T0);
        let (b, _) = request(&mut s, "ada", "0.2", T0 + 5);
        let h = s.history(10).unwrap();
        assert_eq!(h[0].payment_ref, b);
        assert_eq!(h[1].payment_ref, a);
        assert_eq!(s.history(1).unwrap().len(), 1);
    }

    #[test]
    fn survives_restart_and_exposes_submitted_for_reconcile() {
        let db = TempDb::new();
        let pol = policy();
        let r;
        {
            let mut s = PaymentStore::open(&db.0).unwrap();
            r = approved(&mut s, &pol, "0.4", T0);
            s.authorize(&r, T0).unwrap();
            s.mark_submitted(&r, "sigXYZ", 1234, T0).unwrap();
        } // process "crashes" here: connection dropped before confirmation

        let s = PaymentStore::open(&db.0).unwrap();
        let pending = s.by_state(PaymentState::Submitted).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].payment_ref, r);
        assert_eq!(pending[0].signature.as_deref(), Some("sigXYZ"));
        assert_eq!(pending[0].last_valid_block_height, Some(1234));
        // the in-flight payment still counts, so a restart cannot re-spend its budget
        assert_eq!(s.spent_in_window(T0 + 10).unwrap(), sol("0.4"));
    }

    // The core safety property: concurrent processes cannot overspend.
    #[test]
    fn concurrent_reservations_never_exceed_the_daily_cap() {
        let db = TempDb::new();
        PaymentStore::open(&db.0).unwrap(); // create schema once
        let pol = Arc::new(policy());

        let results: Vec<bool> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let (path, pol) = (db.0.clone(), pol.clone());
                    scope.spawn(move || {
                        let mut s = PaymentStore::open(&path).unwrap();
                        let (r, p) = request(&mut s, "ada", "0.3", T0);
                        matches!(
                            s.reserve(&pol, &r, &p, T0).unwrap(),
                            PolicyDecision::Approved(_)
                        )
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

        // 0.3 x 3 = 0.9 fits under 1.0; a fourth would make 1.2
        assert_eq!(results.iter().filter(|ok| **ok).count(), 3);
        let s = PaymentStore::open(&db.0).unwrap();
        assert_eq!(s.spent_in_window(T0).unwrap(), sol("0.9"));
    }

    // Guards against the SQL CHECK list and the Rust enum drifting apart.
    #[test]
    fn schema_accepts_every_state_the_enum_can_produce() {
        let s = PaymentStore::open_in_memory().unwrap();
        let ddl: String = s
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name = 'payments'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        for st in PaymentState::ALL {
            assert!(
                ddl.contains(&format!("'{}'", st.as_str())),
                "schema is missing state {st:?}"
            );
        }
    }

    #[test]
    fn state_strings_round_trip() {
        for st in PaymentState::ALL {
            assert_eq!(PaymentState::from_db(st.as_str()), Some(st));
        }
        assert_eq!(PaymentState::from_db("nonsense"), None);
    }
}
