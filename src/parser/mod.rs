//! The untrusted side: turns free text into a `PaymentProposal`.
//! Nothing here can approve, sign, or send anything.

pub mod llm;
pub mod mock;

use crate::domain::PaymentProposal;
use crate::error::Result;

pub trait IntentParser {
    fn parse(&self, request: &str) -> Result<PaymentProposal>;
}

pub use llm::LlmParser;
pub use mock::MockParser;
