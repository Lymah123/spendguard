-- SpendGuard schema v1. Applied once; PRAGMA user_version tracks it.
-- Amounts are integer lamports (SQLite INTEGER is i64; policy caps keep us far below that).
-- Timestamps are unix seconds, supplied by the caller (never by SQLite), so tests are deterministic.

CREATE TABLE payments (
    payment_ref             TEXT PRIMARY KEY,
    state                   TEXT NOT NULL CHECK (state IN (
        'proposed', 'policy_rejected', 'reserved', 'user_declined',
        'authorized', 'submitted', 'confirmed', 'failed'
    )),

    -- exactly what came in, kept for the audit trail
    raw_request             TEXT NOT NULL,
    recipient_input         TEXT NOT NULL,
    amount_input            TEXT NOT NULL,
    note                    TEXT,

    -- set only when policy approves
    recipient_address       TEXT,
    recipient_label         TEXT,
    amount_lamports         INTEGER CHECK (amount_lamports IS NULL OR amount_lamports > 0),
    reserved_at             INTEGER,

    -- set only when policy rejects
    reject_code             TEXT,
    reject_reason           TEXT,

    -- set when the transaction is signed and sent
    signature               TEXT,
    last_valid_block_height INTEGER,

    created_at              INTEGER NOT NULL,
    updated_at              INTEGER NOT NULL
);

CREATE INDEX idx_payments_state ON payments (state);
CREATE INDEX idx_payments_reserved_at ON payments (reserved_at);

-- Append-only record of every state change.
CREATE TABLE state_log (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    payment_ref TEXT NOT NULL REFERENCES payments (payment_ref),
    from_state  TEXT,
    to_state    TEXT NOT NULL,
    at          INTEGER NOT NULL,
    detail      TEXT
);

CREATE INDEX idx_state_log_ref ON state_log (payment_ref, id);