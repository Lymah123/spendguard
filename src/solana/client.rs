use std::collections::HashMap;
use std::sync::Mutex;

use solana_client::rpc_client::RpcClient;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::hash::Hash;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use solana_sdk::transaction::Transaction;

use crate::domain::Lamports;
use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxStatus {
    /// The cluster has no record of this signature (yet, or ever).
    NotFound,
    /// Seen, but not yet at `confirmed` commitment.
    Pending,
    /// At `confirmed` commitment with no error.
    Confirmed,
    /// Landed and failed on chain.
    Failed(String),
}

/// Everything SpendGuard needs from the chain. No key access here.
///
/// A `send` error is AMBIGUOUS: a timeout can still have delivered the
/// transaction. Callers must have recorded the signature as `Submitted`
/// before sending, and use `status` + `current_block_height` to decide.
pub trait ChainClient {
    /// A recent blockhash and the last block height at which it is valid.
    fn latest_blockhash(&self) -> Result<(Hash, u64)>;
    fn current_block_height(&self) -> Result<u64>;
    fn send(&self, tx: &Transaction) -> Result<()>;
    fn status(&self, sig: &Signature) -> Result<TxStatus>;
    fn balance(&self, key: &Pubkey) -> Result<Lamports>;
}

impl<T: ChainClient + ?Sized> ChainClient for std::sync::Arc<T> {
    fn latest_blockhash(&self) -> Result<(Hash, u64)> {
        (**self).latest_blockhash()
    }
    fn current_block_height(&self) -> Result<u64> {
        (**self).current_block_height()
    }
    fn send(&self, tx: &Transaction) -> Result<()> {
        (**self).send(tx)
    }
    fn status(&self, sig: &Signature) -> Result<TxStatus> {
        (**self).status(sig)
    }
    fn balance(&self, key: &Pubkey) -> Result<Lamports> {
        (**self).balance(key)
    }
}

fn chain_err(e: impl std::fmt::Display) -> Error {
    Error::Chain(e.to_string())
}

/// Blocking JSON-RPC client (Devnet by default; see `.env.example`).
pub struct RpcChain {
    rpc: RpcClient,
}

impl RpcChain {
    pub fn new(url: &str) -> Self {
        Self {
            rpc: RpcClient::new_with_commitment(url.to_string(), CommitmentConfig::confirmed()),
        }
    }
}

impl ChainClient for RpcChain {
    fn latest_blockhash(&self) -> Result<(Hash, u64)> {
        self.rpc
            .get_latest_blockhash_with_commitment(CommitmentConfig::confirmed())
            .map_err(chain_err)
    }

    fn current_block_height(&self) -> Result<u64> {
        self.rpc.get_block_height().map_err(chain_err)
    }

    fn send(&self, tx: &Transaction) -> Result<()> {
        self.rpc.send_transaction(tx).map(|_| ()).map_err(chain_err)
    }

    fn status(&self, sig: &Signature) -> Result<TxStatus> {
        let resp = self
            .rpc
            .get_signature_statuses_with_history(&[*sig])
            .map_err(chain_err)?;
        Ok(match resp.value.into_iter().next().flatten() {
            None => TxStatus::NotFound,
            Some(st) => {
                if let Some(err) = st.err {
                    TxStatus::Failed(err.to_string())
                } else if st.satisfies_commitment(CommitmentConfig::confirmed()) {
                    TxStatus::Confirmed
                } else {
                    TxStatus::Pending
                }
            }
        })
    }

    fn balance(&self, key: &Pubkey) -> Result<Lamports> {
        self.rpc
            .get_balance(key)
            .map(Lamports::new)
            .map_err(chain_err)
    }
}

/// Scriptable in-memory chain for tests (including `tests/`). Not for production use.
pub struct MockChain {
    inner: Mutex<MockState>,
}

struct MockState {
    blockhash: Hash,
    last_valid_block_height: u64,
    height: u64,
    send_error: Option<String>,
    sent: Vec<Transaction>,
    statuses: HashMap<Signature, TxStatus>,
    balance: Lamports,
    auto_confirm: bool,
}

impl Default for MockChain {
    fn default() -> Self {
        Self::new()
    }
}

impl MockChain {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(MockState {
                blockhash: Hash::new_unique(),
                last_valid_block_height: 1_000,
                height: 900,
                send_error: None,
                sent: Vec::new(),
                statuses: HashMap::new(),
                balance: Lamports::new(5_000_000_000),
                auto_confirm: false,
            }),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, MockState> {
        self.inner.lock().expect("mock chain lock poisoned")
    }

    pub fn set_height(&self, h: u64) {
        self.state().height = h;
    }
    pub fn set_status(&self, sig: Signature, st: TxStatus) {
        self.state().statuses.insert(sig, st);
    }
    /// Make `send` return an error (simulating a timeout) without recording the tx.
    pub fn fail_sends_with(&self, msg: &str) {
        self.state().send_error = Some(msg.to_string());
    }
    pub fn set_balance(&self, lamports: Lamports) {
        self.state().balance = lamports;
    }
    /// When true, every accepted `send` is immediately `Confirmed`.
    pub fn set_auto_confirm(&self, on: bool) {
        self.state().auto_confirm = on;
    }
    pub fn sent(&self) -> Vec<Transaction> {
        self.state().sent.clone()
    }
}

impl ChainClient for MockChain {
    fn latest_blockhash(&self) -> Result<(Hash, u64)> {
        let s = self.state();
        Ok((s.blockhash, s.last_valid_block_height))
    }
    fn current_block_height(&self) -> Result<u64> {
        Ok(self.state().height)
    }
    fn send(&self, tx: &Transaction) -> Result<()> {
        let mut s = self.state();
        if let Some(e) = &s.send_error {
            return Err(Error::Chain(e.clone()));
        }
        s.sent.push(tx.clone());
        let sig = tx.signatures[0];
        if s.auto_confirm {
            s.statuses.insert(sig, TxStatus::Confirmed);
        } else {
            s.statuses.entry(sig).or_insert(TxStatus::Pending);
        }
        Ok(())
    }
    fn status(&self, sig: &Signature) -> Result<TxStatus> {
        Ok(self
            .state()
            .statuses
            .get(sig)
            .cloned()
            .unwrap_or(TxStatus::NotFound))
    }
    fn balance(&self, _key: &Pubkey) -> Result<Lamports> {
        Ok(self.state().balance)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::message::Message;
    use solana_sdk::signature::{Keypair, Signer};
    use solana_sdk::system_instruction;

    fn some_tx() -> Transaction {
        let kp = Keypair::new();
        let ix = system_instruction::transfer(&kp.pubkey(), &Pubkey::new_unique(), 1);
        let msg = Message::new(&[ix], Some(&kp.pubkey()));
        Transaction::new(&[&kp], msg, Hash::new_unique())
    }

    #[test]
    fn unknown_signature_is_not_found() {
        let c = MockChain::new();
        assert_eq!(c.status(&Signature::default()).unwrap(), TxStatus::NotFound);
    }

    #[test]
    fn sent_transactions_become_pending_then_follow_the_script() {
        let c = MockChain::new();
        let tx = some_tx();
        c.send(&tx).unwrap();
        assert_eq!(c.sent().len(), 1);
        assert_eq!(c.status(&tx.signatures[0]).unwrap(), TxStatus::Pending);
        c.set_status(tx.signatures[0], TxStatus::Confirmed);
        assert_eq!(c.status(&tx.signatures[0]).unwrap(), TxStatus::Confirmed);
    }

    #[test]
    fn a_failed_send_records_nothing() {
        let c = MockChain::new();
        c.fail_sends_with("timeout");
        let tx = some_tx();
        assert!(matches!(c.send(&tx), Err(Error::Chain(_))));
        assert!(c.sent().is_empty());
        assert_eq!(c.status(&tx.signatures[0]).unwrap(), TxStatus::NotFound);
    }

    #[test]
    fn blockhash_and_height_are_scriptable() {
        let c = MockChain::new();
        let (_, last_valid) = c.latest_blockhash().unwrap();
        assert!(c.current_block_height().unwrap() < last_valid);
        c.set_height(last_valid + 1);
        assert!(c.current_block_height().unwrap() > last_valid);
    }
}
