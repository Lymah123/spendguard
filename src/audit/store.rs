use std::path::Path;
use std::time::Duration;

use rusqlite::types::Type;
use rusqlite::{params, Connection, OptionalExtension, Row, Transaction, TransactionBehavior};

use crate::domain::{Lamports, PaymentProposal, PaymentRef, PaymentState};
use crate::error::{Error, Result};
use crate::policy::BudgetWindow;

const MIGRATION_001: &str = include_str!("../../migrations/001_init.sql");
const SCHEMA_VERSION: i64 = 1;

const COLS: &str = "payment_ref, state, raw_request, recipient_input, amount_input, note, \
    recipient_address, recipient_label, amount_lamports, reject_code, reject_reason, \
    signature, last_valid_block_height, created_at, reserved_at, updated_at";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaymentRecord {
    pub payment_ref: PaymentRef,
    pub state: PaymentState,
    pub raw_request: String,
    pub recipient_input: String,
    pub amount_input: String,
    pub note: Option<String>,
    pub recipient_address: Option<String>,
    pub recipient_label: Option<String>,
    pub amount: Option<Lamports>,
    pub reject_code: Option<String>,
    pub reject_reason: Option<String>,
    pub signature: Option<String>,
    pub last_valid_block_height: Option<u64>,
    pub created_at: i64,
    pub reserved_at: Option<i64>,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    pub from: Option<PaymentState>,
    pub to: PaymentState,
    pub at: i64,
    pub detail: Option<String>,
}

/// SQLite-backed payment store. Synchronous on purpose: every state change is
/// one short IMMEDIATE transaction, which keeps the budget logic easy to reason
/// about. Wrap calls in `spawn_blocking` if an async caller ever needs it.
pub struct PaymentStore {
    pub(super) conn: Connection,
    pub(super) window: BudgetWindow,
}

fn conv_err(idx: usize, ty: Type, msg: String) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(idx, ty, msg.into())
}

fn parse_state(idx: usize, s: &str) -> rusqlite::Result<PaymentState> {
    PaymentState::from_db(s)
        .ok_or_else(|| conv_err(idx, Type::Text, format!("unknown payment state {s:?}")))
}

fn to_i64(v: u64, what: &str) -> Result<i64> {
    i64::try_from(v)
        .map_err(|_| Error::Corrupt(format!("{what} {v} does not fit in SQLite INTEGER")))
}

fn map_row(row: &Row<'_>) -> rusqlite::Result<PaymentRecord> {
    let state_s: String = row.get(1)?;
    let amount: Option<i64> = row.get(8)?;
    let height: Option<i64> = row.get(12)?;
    let non_negative = |idx: usize, v: i64| {
        u64::try_from(v).map_err(|_| conv_err(idx, Type::Integer, format!("negative value {v}")))
    };
    Ok(PaymentRecord {
        payment_ref: PaymentRef::from_stored(row.get(0)?),
        state: parse_state(1, &state_s)?,
        raw_request: row.get(2)?,
        recipient_input: row.get(3)?,
        amount_input: row.get(4)?,
        note: row.get(5)?,
        recipient_address: row.get(6)?,
        recipient_label: row.get(7)?,
        amount: amount
            .map(|a| non_negative(8, a).map(Lamports::new))
            .transpose()?,
        reject_code: row.get(9)?,
        reject_reason: row.get(10)?,
        signature: row.get(11)?,
        last_valid_block_height: height.map(|h| non_negative(12, h)).transpose()?,
        created_at: row.get(13)?,
        reserved_at: row.get(14)?,
        updated_at: row.get(15)?,
    })
}

pub(super) fn current_state(conn: &Connection, r: &PaymentRef) -> Result<PaymentState> {
    let s: Option<String> = conn
        .query_row(
            "SELECT state FROM payments WHERE payment_ref = ?1",
            [r.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    let s = s.ok_or_else(|| Error::PaymentNotFound(r.to_string()))?;
    PaymentState::from_db(&s).ok_or_else(|| Error::Corrupt(format!("unknown state {s:?} for {r}")))
}

fn log(
    tx: &Transaction<'_>,
    r: &PaymentRef,
    from: Option<PaymentState>,
    to: PaymentState,
    now: i64,
    detail: Option<&str>,
) -> Result<()> {
    tx.execute(
        "INSERT INTO state_log (payment_ref, from_state, to_state, at, detail) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![r.as_str(), from.map(PaymentState::as_str), to.as_str(), now, detail],
    )?;
    Ok(())
}

/// The ONLY place a state changes. Validates against the transition table,
/// updates with a compare-and-swap on the old state, and appends to the log.
/// Must be called inside an IMMEDIATE transaction.
pub(super) fn transition_tx(
    tx: &Transaction<'_>,
    r: &PaymentRef,
    to: PaymentState,
    now: i64,
    detail: Option<&str>,
) -> Result<PaymentState> {
    let from = current_state(tx, r)?;
    from.transition(to)?;
    let n = tx.execute(
        "UPDATE payments SET state = ?1, updated_at = ?2 WHERE payment_ref = ?3 AND state = ?4",
        params![to.as_str(), now, r.as_str(), from.as_str()],
    )?;
    if n != 1 {
        return Err(Error::Corrupt(format!("state of {r} changed concurrently")));
    }
    log(tx, r, Some(from), to, now, detail)?;
    Ok(from)
}

impl PaymentStore {
    pub fn open(path: &Path) -> Result<Self> {
        Self::init(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> Result<Self> {
        conn.busy_timeout(Duration::from_secs(5))?;
        // WAL for concurrent readers; FULL so a committed payment record survives power loss.
        conn.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON; PRAGMA synchronous = FULL;",
        )?;

        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(Error::Corrupt(format!(
                "database schema v{version} is newer than this build (v{SCHEMA_VERSION})"
            )));
        }
        if version < 1 {
            let tx = conn.transaction()?;
            tx.execute_batch(MIGRATION_001)?;
            tx.execute_batch("PRAGMA user_version = 1;")?;
            tx.commit()?;
        }
        Ok(Self {
            conn,
            window: BudgetWindow::default(),
        })
    }

    /// Which window `spent_in_window` measures. `PaymentService::new` sets this
    /// from the policy so the two can never disagree.
    pub fn set_window(&mut self, window: BudgetWindow) {
        self.window = window;
    }

    /// Record a new request in state `Proposed`. Call before policy runs, so even
    /// rejected requests leave a trail.
    pub fn propose(
        &mut self,
        r: &PaymentRef,
        raw_request: &str,
        proposal: &PaymentProposal,
        now: i64,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO payments \
             (payment_ref, state, raw_request, recipient_input, amount_input, note, created_at, updated_at) \
             VALUES (?1, 'proposed', ?2, ?3, ?4, ?5, ?6, ?6)",
            params![r.as_str(), raw_request, proposal.recipient, proposal.amount, proposal.note, now],
        )?;
        log(&tx, r, None, PaymentState::Proposed, now, None)?;
        tx.commit()?;
        Ok(())
    }

    fn simple_transition(
        &mut self,
        r: &PaymentRef,
        to: PaymentState,
        now: i64,
        detail: Option<&str>,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transition_tx(&tx, r, to, now, detail)?;
        tx.commit()?;
        Ok(())
    }

    /// The user approved the exact built message. Reserved -> Authorized.
    pub fn authorize(&mut self, r: &PaymentRef, now: i64) -> Result<()> {
        self.simple_transition(r, PaymentState::Authorized, now, None)
    }

    /// The user refused at the prompt. Reserved -> UserDeclined (frees the budget).
    pub fn decline(&mut self, r: &PaymentRef, now: i64) -> Result<()> {
        self.simple_transition(r, PaymentState::UserDeclined, now, None)
    }

    /// Submitted -> Confirmed.
    pub fn confirm(&mut self, r: &PaymentRef, now: i64) -> Result<()> {
        self.simple_transition(r, PaymentState::Confirmed, now, None)
    }

    /// Any legal predecessor -> Failed (frees the budget). Only call for
    /// `Submitted` once the blockhash has expired or the chain reports failure.
    pub fn fail(&mut self, r: &PaymentRef, now: i64, reason: &str) -> Result<()> {
        self.simple_transition(r, PaymentState::Failed, now, Some(reason))
    }

    /// Authorized -> Submitted, saving the signature. Call this BEFORE sending
    /// the transaction so a crash or timeout mid-send is recoverable.
    pub fn mark_submitted(
        &mut self,
        r: &PaymentRef,
        signature: &str,
        last_valid_block_height: u64,
        now: i64,
    ) -> Result<()> {
        let height = to_i64(last_valid_block_height, "block height")?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transition_tx(&tx, r, PaymentState::Submitted, now, Some(signature))?;
        tx.execute(
            "UPDATE payments SET signature = ?1, last_valid_block_height = ?2 WHERE payment_ref = ?3",
            params![signature, height, r.as_str()],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn get(&self, r: &PaymentRef) -> Result<PaymentRecord> {
        self.conn
            .query_row(
                &format!("SELECT {COLS} FROM payments WHERE payment_ref = ?1"),
                [r.as_str()],
                map_row,
            )
            .optional()?
            .ok_or_else(|| Error::PaymentNotFound(r.to_string()))
    }

    /// Newest first.
    pub fn history(&self, limit: u32) -> Result<Vec<PaymentRecord>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLS} FROM payments ORDER BY created_at DESC, rowid DESC LIMIT ?1"
        ))?;
        let rows = stmt
            .query_map([limit], map_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Oldest first. `reconcile` uses this with `Submitted` (and `Authorized`,
    /// for payments interrupted before the send).
    pub fn by_state(&self, state: PaymentState) -> Result<Vec<PaymentRecord>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {COLS} FROM payments WHERE state = ?1 ORDER BY created_at, rowid"
        ))?;
        let rows = stmt
            .query_map([state.as_str()], map_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn state_log(&self, r: &PaymentRef) -> Result<Vec<LogEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT from_state, to_state, at, detail FROM state_log WHERE payment_ref = ?1 ORDER BY id",
        )?;
        let rows = stmt
            .query_map([r.as_str()], |row| {
                let from: Option<String> = row.get(0)?;
                let to: String = row.get(1)?;
                Ok(LogEntry {
                    from: from.map(|f| parse_state(0, &f)).transpose()?,
                    to: parse_state(1, &to)?,
                    at: row.get(2)?,
                    detail: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}
