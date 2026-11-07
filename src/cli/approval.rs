use std::cell::RefCell;
use std::io::{BufRead, Write};

use crate::service::{ApprovalSummary, Approver};

/// What the user reads before signing. Pure so it can be tested. Full addresses
/// are shown on purpose: the user must be able to verify them.
pub fn render_summary(s: &ApprovalSummary) -> String {
    let to = match &s.recipient_label {
        Some(label) => format!("{label}  ({})", s.to),
        None => s.to.to_string(),
    };
    let mut out = String::new();
    out.push_str(&format!(
        "\n── Payment {} ──────────────────────────────\n",
        s.payment_ref
    ));
    out.push_str(&format!("  To:       {to}\n"));
    out.push_str(&format!("  Amount:   {}\n", s.amount));
    out.push_str(&format!("  From:     {}\n", s.from));
    out.push_str(&format!("  Memo:     {}\n", s.memo));
    if let Some(note) = &s.note {
        out.push_str(&format!(
            "  Note:     {note}   (kept locally, not on-chain)\n"
        ));
    }
    out.push_str(&format!(
        "  Budget:   {} left of today's limit after this payment\n",
        s.budget_remaining_after
    ));
    out
}

/// Asks on a terminal. Only an explicit "y" or "yes" approves; an empty line,
/// anything else, end-of-input, or an I/O error all mean no.
pub struct PromptApprover<R: BufRead, W: Write> {
    input: RefCell<R>,
    output: RefCell<W>,
}

impl<R: BufRead, W: Write> PromptApprover<R, W> {
    pub fn new(input: R, output: W) -> Self {
        Self {
            input: RefCell::new(input),
            output: RefCell::new(output),
        }
    }
}

impl<R: BufRead, W: Write> Approver for PromptApprover<R, W> {
    fn approve(&self, summary: &ApprovalSummary) -> bool {
        let mut out = self.output.borrow_mut();
        if write!(
            out,
            "{}\nSign and send this payment? [y/N] ",
            render_summary(summary)
        )
        .is_err()
            || out.flush().is_err()
        {
            return false;
        }
        let mut line = String::new();
        match self.input.borrow_mut().read_line(&mut line) {
            Ok(n) if n > 0 => matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes"),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use solana_sdk::pubkey::Pubkey;

    use super::*;
    use crate::domain::{Lamports, PaymentRef};

    fn summary() -> ApprovalSummary {
        ApprovalSummary {
            payment_ref: PaymentRef::from_stored("sg_abc123def456".into()),
            from: Pubkey::new_unique(),
            to: Pubkey::new_unique(),
            recipient_label: Some("Ada".into()),
            amount: Lamports::parse_sol("0.1").unwrap(),
            memo: "spendguard:sg_abc123def456".into(),
            note: Some("API invoice".into()),
            budget_remaining_after: Lamports::parse_sol("0.9").unwrap(),
        }
    }

    fn ask(answer: &str) -> (bool, String) {
        let mut out = Vec::new();
        let ok = {
            let a = PromptApprover::new(answer.as_bytes(), &mut out);
            a.approve(&summary())
        };
        (ok, String::from_utf8(out).unwrap())
    }

    #[test]
    fn summary_shows_everything_needed_to_decide() {
        let s = summary();
        let text = render_summary(&s);
        for needle in [
            "Ada",
            &s.to.to_string(),
            &s.from.to_string(),
            "0.1 SOL",
            "spendguard:sg_abc123def456",
            "API invoice",
            "0.9 SOL",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
    }

    #[test]
    fn only_an_explicit_yes_approves() {
        for yes in ["y\n", "Y\n", "yes\n", " YES \n"] {
            assert!(ask(yes).0, "{yes:?} should approve");
        }
        for no in [
            "\n", "n\n", "no\n", "yep\n", "sure\n", "y e s\n", "", "ok\n",
        ] {
            assert!(!ask(no).0, "{no:?} should decline");
        }
    }

    #[test]
    fn the_prompt_is_printed_before_reading() {
        let (_, shown) = ask("n\n");
        assert!(shown.contains("Sign and send this payment? [y/N]"));
    }

    #[test]
    fn a_broken_terminal_means_no() {
        struct BrokenOut;
        impl Write for BrokenOut {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "gone"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        assert!(!PromptApprover::new("y\n".as_bytes(), BrokenOut).approve(&summary()));
    }
}
