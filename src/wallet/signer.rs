use std::fmt;
use std::path::Path;

use solana_sdk::message::Message;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{read_keypair_file, Keypair, Signature, Signer as _};
use solana_sdk::transaction::Transaction;

use crate::error::{Error, Result};
use crate::solana::builder::decode_message;

pub struct SignedPayment {
    pub transaction: Transaction,
    pub signature: Signature,
}

/// Holds the keypair. Nothing else in the crate can reach the secret: the field
/// is private, `Debug` prints only the public key, and the only operation is
/// `sign`, which refuses any message that is not one plain transfer + memo
/// paid by this key. Even a bug elsewhere cannot get this to sign something else.
pub struct FileSigner {
    keypair: Keypair,
}

impl fmt::Debug for FileSigner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FileSigner({})", self.keypair.pubkey())
    }
}

#[cfg(unix)]
fn check_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path)
        .map_err(|e| Error::Signer(format!("cannot read {}: {e}", path.display())))?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        return Err(Error::Signer(format!(
            "{} is readable by other users (mode {:o}); run `chmod 600` on it",
            path.display(),
            mode & 0o777
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

impl FileSigner {
    /// Load a Solana CLI-format keypair file (a JSON array of 64 bytes).
    pub fn from_file(path: &Path) -> Result<Self> {
        check_permissions(path)?;
        let keypair = read_keypair_file(path)
            .map_err(|e| Error::Signer(format!("cannot load keypair {}: {e}", path.display())))?;
        Ok(Self { keypair })
    }

    pub fn from_keypair(keypair: Keypair) -> Self {
        Self { keypair }
    }

    pub fn pubkey(&self) -> Pubkey {
        self.keypair.pubkey()
    }

    pub fn sign(&self, message: &Message) -> Result<SignedPayment> {
        let decoded = decode_message(message)?;
        if decoded.from != self.keypair.pubkey() {
            return Err(Error::MessageMismatch(
                "message is not paid by this wallet".into(),
            ));
        }
        let transaction =
            Transaction::new(&[&self.keypair], message.clone(), message.recent_blockhash);
        let signature = transaction.signatures[0];
        Ok(SignedPayment {
            transaction,
            signature,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Lamports, PaymentRef, ValidatedPayment};
    use crate::solana::builder::build_message;
    use solana_sdk::hash::Hash;
    use solana_sdk::signature::write_keypair_file;
    use solana_sdk::system_instruction;

    fn payment() -> ValidatedPayment {
        ValidatedPayment::new(
            PaymentRef::from_stored("sg_test00000001".into()),
            Pubkey::new_unique(),
            None,
            Lamports::new(1_000),
            None,
        )
    }

    #[test]
    fn signs_a_valid_payment_and_the_signature_verifies() {
        let signer = FileSigner::from_keypair(Keypair::new());
        let msg = build_message(&payment(), &signer.pubkey(), Hash::new_unique());
        let signed = signer.sign(&msg).unwrap();
        assert!(signed
            .transaction
            .verify_with_results()
            .iter()
            .all(|ok| *ok));
        assert_eq!(signed.signature, signed.transaction.signatures[0]);
    }

    #[test]
    fn signing_is_deterministic_so_the_signature_is_known_before_sending() {
        let signer = FileSigner::from_keypair(Keypair::new());
        let msg = build_message(&payment(), &signer.pubkey(), Hash::new_unique());
        assert_eq!(
            signer.sign(&msg).unwrap().signature,
            signer.sign(&msg).unwrap().signature
        );
    }

    #[test]
    fn refuses_a_message_paid_by_someone_else() {
        let signer = FileSigner::from_keypair(Keypair::new());
        let other = Keypair::new().pubkey();
        let msg = build_message(&payment(), &other, Hash::new_unique());
        assert!(matches!(signer.sign(&msg), Err(Error::MessageMismatch(_))));
    }

    #[test]
    fn refuses_arbitrary_instructions() {
        let signer = FileSigner::from_keypair(Keypair::new());
        let me = signer.pubkey();
        // e.g. a drain that skips the builder entirely
        let ixs = [
            system_instruction::transfer(&me, &Pubkey::new_unique(), 5),
            system_instruction::transfer(&me, &Pubkey::new_unique(), 5),
        ];
        let msg = Message::new_with_blockhash(&ixs, Some(&me), &Hash::new_unique());
        assert!(signer.sign(&msg).is_err());
    }

    #[test]
    fn debug_output_never_contains_secret_bytes() {
        let kp = Keypair::new();
        let secret = kp.to_base58_string();
        let signer = FileSigner::from_keypair(kp);
        let shown = format!("{signer:?}");
        assert!(shown.contains(&signer.pubkey().to_string()));
        assert!(!shown.contains(&secret));
    }

    #[cfg(unix)]
    #[test]
    fn loads_a_private_keyfile_and_refuses_a_world_readable_one() {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!("sg-key-{}.json", uuid::Uuid::new_v4()));
        let kp = Keypair::new();
        write_keypair_file(&kp, &path).unwrap();

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(FileSigner::from_file(&path).unwrap().pubkey(), kp.pubkey());

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            FileSigner::from_file(&path),
            Err(Error::Signer(_))
        ));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_keyfile_is_a_clean_error() {
        let r = FileSigner::from_file(Path::new("/nonexistent/spendguard-key.json"));
        assert!(matches!(r, Err(Error::Signer(_))));
    }
}
