use std::str::FromStr;

use solana_sdk::hash::Hash;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::message::Message;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::{system_instruction, system_program};

use crate::domain::{Lamports, ValidatedPayment};
use crate::error::{Error, Result};

/// SPL Memo program (v2).
const MEMO_PROGRAM_ID: &str = "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr";

/// A legacy transaction holding exactly two instructions plus the fee payer
/// and one signature is the only shape SpendGuard builds or signs.
const MAX_ACCOUNT_KEYS: usize = 4; // payer, recipient, system program, memo program

pub fn memo_program_id() -> Pubkey {
    Pubkey::from_str(MEMO_PROGRAM_ID).expect("memo program id constant is valid")
}

/// What a built message actually does, read back out of its bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedPayment {
    pub from: Pubkey,
    pub to: Pubkey,
    pub lamports: Lamports,
    pub memo: String,
    pub recent_blockhash: Hash,
}

/// ValidatedPayment -> unsigned message: one System transfer, then one memo.
/// Taking `ValidatedPayment` means only policy-approved payments can be built.
pub fn build_message(
    payment: &ValidatedPayment,
    payer: &Pubkey,
    recent_blockhash: Hash,
) -> Message {
    let transfer =
        system_instruction::transfer(payer, payment.recipient(), payment.amount().as_u64());
    let memo = Instruction {
        program_id: memo_program_id(),
        // The payer co-signs the memo so the memo program can attribute it.
        accounts: vec![AccountMeta::new_readonly(*payer, true)],
        data: payment.memo_text().into_bytes(),
    };
    Message::new_with_blockhash(&[transfer, memo], Some(payer), &recent_blockhash)
}

fn mismatch(why: &str) -> Error {
    Error::MessageMismatch(why.to_string())
}

fn key_at(msg: &Message, idx: usize) -> Result<&Pubkey> {
    msg.account_keys
        .get(idx)
        .ok_or_else(|| mismatch("an instruction references a missing account"))
}

/// Strictly decode a message back into "who pays whom how much, with what memo".
///
/// This is what the approval prompt renders and what the signer re-checks, so
/// the user approves what is actually signed. It fails closed: anything other
/// than one plain transfer followed by one memo is an error.
pub fn decode_message(msg: &Message) -> Result<DecodedPayment> {
    if msg.header.num_required_signatures != 1 {
        return Err(mismatch("expected exactly one signer"));
    }
    if msg.instructions.len() != 2 {
        return Err(mismatch(
            "expected exactly two instructions (transfer + memo)",
        ));
    }
    if msg.account_keys.len() > MAX_ACCOUNT_KEYS {
        return Err(mismatch("message references unexpected extra accounts"));
    }
    let payer = *key_at(msg, 0)?;

    // --- instruction 0: System Program transfer ---
    let transfer = &msg.instructions[0];
    if *key_at(msg, transfer.program_id_index as usize)? != system_program::id() {
        return Err(mismatch("first instruction is not a System Program call"));
    }
    // System `Transfer` wire format: u32 LE discriminant (2) then u64 LE lamports.
    if transfer.data.len() != 12 || transfer.data[..4] != [2, 0, 0, 0] {
        return Err(mismatch("first instruction is not a plain transfer"));
    }
    if transfer.accounts.len() != 2 {
        return Err(mismatch(
            "transfer must reference exactly a source and a destination",
        ));
    }
    let from = *key_at(msg, transfer.accounts[0] as usize)?;
    let to = *key_at(msg, transfer.accounts[1] as usize)?;
    if from != payer {
        return Err(mismatch(
            "funds would leave an account other than the fee payer",
        ));
    }
    let raw: [u8; 8] = transfer.data[4..12]
        .try_into()
        .map_err(|_| mismatch("bad transfer amount"))?;
    let lamports = Lamports::new(u64::from_le_bytes(raw));

    // --- instruction 1: memo ---
    let memo_ix = &msg.instructions[1];
    if *key_at(msg, memo_ix.program_id_index as usize)? != memo_program_id() {
        return Err(mismatch("second instruction is not a memo"));
    }
    if memo_ix.accounts.len() > 1 {
        return Err(mismatch("memo references too many accounts"));
    }
    for &a in &memo_ix.accounts {
        if *key_at(msg, a as usize)? != payer {
            return Err(mismatch("memo references an unexpected account"));
        }
    }
    let memo =
        String::from_utf8(memo_ix.data.clone()).map_err(|_| mismatch("memo is not valid UTF-8"))?;

    Ok(DecodedPayment {
        from,
        to,
        lamports,
        memo,
        recent_blockhash: msg.recent_blockhash,
    })
}

/// Decode, then confirm the message is exactly the payment that was approved.
pub fn verify_matches(msg: &Message, payment: &ValidatedPayment) -> Result<DecodedPayment> {
    let d = decode_message(msg)?;
    if d.to != *payment.recipient() {
        return Err(mismatch("recipient differs from the approved payment"));
    }
    if d.lamports != payment.amount() {
        return Err(mismatch("amount differs from the approved payment"));
    }
    if d.memo != payment.memo_text() {
        return Err(mismatch("memo differs from the approved payment"));
    }
    Ok(d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::PaymentRef;
    use solana_sdk::signature::{Keypair, Signer};

    fn payment(to: Pubkey, amount: &str) -> ValidatedPayment {
        ValidatedPayment::new(
            PaymentRef::from_stored("sg_test00000001".into()),
            to,
            Some("Ada".into()),
            Lamports::parse_sol(amount).unwrap(),
            Some("private note".into()),
        )
    }

    fn built() -> (Message, ValidatedPayment, Pubkey) {
        let payer = Keypair::new().pubkey();
        let p = payment(Pubkey::new_unique(), "0.1");
        (build_message(&p, &payer, Hash::new_unique()), p, payer)
    }

    #[test]
    fn built_message_decodes_to_the_approved_payment() {
        let (msg, p, payer) = built();
        let d = verify_matches(&msg, &p).unwrap();
        assert_eq!(d.from, payer);
        assert_eq!(d.to, *p.recipient());
        assert_eq!(d.lamports, Lamports::parse_sol("0.1").unwrap());
        assert_eq!(d.memo, "spendguard:sg_test00000001");
        assert_eq!(msg.header.num_required_signatures, 1);
        assert_eq!(msg.instructions.len(), 2);
    }

    #[test]
    fn memo_carries_the_reference_but_not_the_note() {
        let (msg, _, _) = built();
        let memo = String::from_utf8(msg.instructions[1].data.clone()).unwrap();
        assert!(memo.contains("sg_test00000001"));
        assert!(!memo.contains("private"));
    }

    #[test]
    fn detects_tampered_amount() {
        let (mut msg, p, _) = built();
        msg.instructions[0].data[4] ^= 0xFF; // flip bits in the lamports field
        assert!(decode_message(&msg).is_ok()); // still a well-formed transfer...
        assert!(matches!(
            verify_matches(&msg, &p),
            Err(Error::MessageMismatch(_))
        )); // ...but not the approved one
    }

    #[test]
    fn detects_swapped_recipient() {
        let (mut msg, p, _) = built();
        msg.account_keys[1] = Pubkey::new_unique();
        assert!(matches!(
            verify_matches(&msg, &p),
            Err(Error::MessageMismatch(_))
        ));
    }

    #[test]
    fn detects_tampered_memo() {
        let (mut msg, p, _) = built();
        msg.instructions[1].data = b"hello".to_vec();
        assert!(matches!(
            verify_matches(&msg, &p),
            Err(Error::MessageMismatch(_))
        ));
    }

    #[test]
    fn rejects_extra_instructions() {
        let payer = Keypair::new().pubkey();
        let to = Pubkey::new_unique();
        let ixs = [
            system_instruction::transfer(&payer, &to, 1),
            system_instruction::transfer(&payer, &to, 2),
            system_instruction::transfer(&payer, &to, 3),
        ];
        let msg = Message::new_with_blockhash(&ixs, Some(&payer), &Hash::new_unique());
        assert!(matches!(
            decode_message(&msg),
            Err(Error::MessageMismatch(_))
        ));
    }

    #[test]
    fn rejects_non_transfer_system_instruction() {
        let payer = Keypair::new().pubkey();
        let ixs = [
            system_instruction::allocate(&payer, 100), // a System call, but not a transfer
            Instruction {
                program_id: memo_program_id(),
                accounts: vec![],
                data: b"x".to_vec(),
            },
        ];
        let msg = Message::new_with_blockhash(&ixs, Some(&payer), &Hash::new_unique());
        assert!(matches!(
            decode_message(&msg),
            Err(Error::MessageMismatch(_))
        ));
    }

    #[test]
    fn rejects_transfer_from_someone_other_than_the_payer() {
        let (payer, victim, to) = (
            Keypair::new().pubkey(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        let ixs = [
            system_instruction::transfer(&victim, &to, 5),
            Instruction {
                program_id: memo_program_id(),
                accounts: vec![],
                data: b"x".to_vec(),
            },
        ];
        let msg = Message::new_with_blockhash(&ixs, Some(&payer), &Hash::new_unique());
        assert!(matches!(
            decode_message(&msg),
            Err(Error::MessageMismatch(_))
        ));
    }

    #[test]
    fn rejects_a_second_required_signer() {
        let (payer, other, to) = (
            Keypair::new().pubkey(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        let ixs = [
            system_instruction::transfer(&payer, &to, 5),
            Instruction {
                program_id: memo_program_id(),
                accounts: vec![AccountMeta::new_readonly(other, true)],
                data: b"x".to_vec(),
            },
        ];
        let msg = Message::new_with_blockhash(&ixs, Some(&payer), &Hash::new_unique());
        assert!(matches!(
            decode_message(&msg),
            Err(Error::MessageMismatch(_))
        ));
    }

    #[test]
    fn rejects_wrong_second_program() {
        let payer = Keypair::new().pubkey();
        let to = Pubkey::new_unique();
        let ixs = [
            system_instruction::transfer(&payer, &to, 5),
            Instruction {
                program_id: Pubkey::new_unique(),
                accounts: vec![],
                data: b"x".to_vec(),
            },
        ];
        let msg = Message::new_with_blockhash(&ixs, Some(&payer), &Hash::new_unique());
        assert!(matches!(
            decode_message(&msg),
            Err(Error::MessageMismatch(_))
        ));
    }

    #[test]
    fn out_of_range_account_index_fails_closed() {
        let (mut msg, _, _) = built();
        msg.instructions[0].accounts[1] = 200;
        assert!(matches!(
            decode_message(&msg),
            Err(Error::MessageMismatch(_))
        ));
    }
}
