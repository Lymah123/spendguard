use std::io::Write;
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{write_keypair_file, Keypair, Signer};

use crate::audit::PaymentRecord;
use crate::domain::Lamports;
use crate::error::{Error, Result};
use crate::parser::IntentParser;
use crate::policy::BudgetWindow;
use crate::service::{Approver, PayOutcome, PaymentService, ReconcileReport, ReconcileResult};
use crate::solana::client::ChainClient;

#[derive(Parser, Debug)]
#[command(
    name = "spendguard",
    version,
    about = "AI-assisted Solana payments under deterministic spending policy"
)]
pub struct Cli {
    /// Path to the policy file.
    #[arg(long, global = true, default_value = "spendguard.toml")]
    pub config: PathBuf,

    /// Use the built-in deterministic parser instead of the LLM (offline demos).
    #[arg(long, global = true)]
    pub mock_parser: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Make a payment from a plain-language request.
    Pay {
        #[arg(required = true, num_args = 1..)]
        request: Vec<String>,
    },
    /// Show recent payments, including rejected ones.
    History {
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// Show today's spending against the limits.
    Budget,
    /// Settle payments left in flight, for example after a crash.
    Reconcile,
    /// Print this wallet's public address.
    Address,
    /// Create a new Devnet keypair file.
    Keygen {
        #[arg(long, default_value = "spendguard-devnet.json")]
        path: PathBuf,
    },
}

/// Create a keypair file readable only by the owner. Refuses to overwrite.
pub fn keygen(path: &Path) -> Result<Pubkey> {
    if path.exists() {
        return Err(Error::Signer(format!(
            "{} already exists; not overwriting",
            path.display()
        )));
    }
    let kp = Keypair::new();
    write_keypair_file(&kp, path)
        .map_err(|e| Error::Signer(format!("cannot write {}: {e}", path.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(kp.pubkey())
}

pub fn explorer_url(signature: &str) -> String {
    format!("https://explorer.solana.com/tx/{signature}?cluster=devnet")
}

/// Unix seconds -> "YYYY-MM-DD HH:MM:SS" (UTC), without a date library.
pub fn fmt_time(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Howard Hinnant's civil-from-days
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

pub fn format_outcome(o: &PayOutcome) -> String {
    match o {
        PayOutcome::Rejected { payment_ref, reason } => format!(
            "✗ Blocked by policy: {reason}\n  Nothing was signed or sent.  (ref {payment_ref})"
        ),
        PayOutcome::Declined { payment_ref } => {
            format!("Declined. Nothing was signed or sent.  (ref {payment_ref})")
        }
        PayOutcome::Confirmed { payment_ref, signature } => format!(
            "✓ Confirmed on Devnet\n  Reference: {payment_ref}\n  Signature: {signature}\n  Explorer:  {}",
            explorer_url(&signature.to_string())
        ),
        PayOutcome::Pending { payment_ref, signature, detail } => format!(
            "… Not confirmed yet: {detail}\n  Reference: {payment_ref}\n  Signature: {signature}\n  Explorer:  {}",
            explorer_url(&signature.to_string())
        ),
        PayOutcome::Failed { payment_ref, reason } => format!("✗ Failed: {reason}\n  (ref {payment_ref})"),
    }
}

pub fn format_history(records: &[PaymentRecord]) -> String {
    if records.is_empty() {
        return "No payments yet.".to_string();
    }
    let mut out = String::new();
    for r in records {
        let who = r.recipient_label.as_deref().unwrap_or(&r.recipient_input);
        let amount = r
            .amount
            .map_or_else(|| format!("({})", r.amount_input), |a| a.to_string());
        let detail = r
            .reject_reason
            .clone()
            .or_else(|| r.signature.as_ref().map(|s| explorer_url(s)))
            .unwrap_or_default();
        out.push_str(&format!(
            "{}  {}  {:<15}  {:>16}  → {}\n",
            fmt_time(r.created_at),
            r.payment_ref,
            r.state.as_str(),
            amount,
            who
        ));
        if !detail.is_empty() {
            out.push_str(&format!("    {detail}\n"));
        }
    }
    out.trim_end().to_string()
}

pub fn format_reconcile(reports: &[ReconcileReport]) -> String {
    if reports.is_empty() {
        return "Nothing in flight.".to_string();
    }
    reports
        .iter()
        .map(|r| {
            let what = match &r.result {
                ReconcileResult::Confirmed => "confirmed".to_string(),
                ReconcileResult::Failed(why) => format!("failed: {why}"),
                ReconcileResult::StillPending => {
                    "still pending (blockhash not yet expired)".to_string()
                }
                ReconcileResult::Abandoned => "never sent; released".to_string(),
            };
            format!("{}  {what}", r.payment_ref)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Run one command against a service. Keygen is handled in `main` (it needs no wallet).
pub fn execute<C: ChainClient>(
    cmd: &Command,
    svc: &mut PaymentService<C>,
    parser: &dyn IntentParser,
    approver: &dyn Approver,
    out: &mut dyn Write,
    now: i64,
) -> Result<()> {
    match cmd {
        Command::Pay { request } => {
            let request = request.join(" ");
            let proposal = match parser.parse(&request) {
                Ok(p) => p,
                Err(e) => {
                    writeln!(
                        out,
                        "Could not turn that into a payment: {e}\n  Nothing was signed or sent."
                    )?;
                    return Ok(());
                }
            };
            let outcome = svc.pay(&request, &proposal, approver)?;
            writeln!(out, "{}", format_outcome(&outcome))?;
        }
        Command::History { limit } => {
            writeln!(out, "{}", format_history(&svc.store().history(*limit)?))?;
        }
        Command::Budget => {
            let spent = svc.store().spent_in_window(now)?;
            let cap = svc.policy().daily_cap();
            let left = cap.checked_sub(spent).unwrap_or(Lamports::ZERO);
            let period = match svc.policy().window() {
                BudgetWindow::UtcDay => "Today (UTC)",
                BudgetWindow::Rolling24h => "Last 24h",
            };
            writeln!(
                out,
                "{period}: {spent} of {cap} used, {left} left.\nPer-payment limit: {}\nApproved recipients: {}",
                svc.policy().max_per_tx(),
                svc.policy().allowlist().len()
            )?;
        }
        Command::Reconcile => {
            writeln!(out, "{}", format_reconcile(&svc.reconcile()?))?;
        }
        Command::Address => writeln!(out, "{}", svc.wallet_pubkey())?,
        Command::Keygen { .. } => {
            return Err(Error::Config(
                "keygen is handled before the wallet is loaded".into(),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use crate::audit::PaymentStore;
    use crate::parser::MockParser;
    use crate::policy::{Allowlist, Policy};
    use crate::service::{ApprovalSummary, ConfirmSettings};
    use crate::solana::client::MockChain;
    use crate::wallet::signer::FileSigner;

    struct Always(bool);
    impl Approver for Always {
        fn approve(&self, _: &ApprovalSummary) -> bool {
            self.0
        }
    }

    fn sol(s: &str) -> Lamports {
        Lamports::parse_sol(s).unwrap()
    }

    fn service() -> (PaymentService<Arc<MockChain>>, Arc<MockChain>) {
        let chain = Arc::new(MockChain::new());
        chain.set_auto_confirm(true);
        let allow = Allowlist::new([("Ada".to_string(), Pubkey::new_unique())]).unwrap();
        let policy = Policy::new(sol("0.5"), sol("1"), allow).unwrap();
        let svc = PaymentService::new(
            PaymentStore::open_in_memory().unwrap(),
            policy,
            chain.clone(),
            FileSigner::from_keypair(Keypair::new()),
        )
        .with_confirm(ConfirmSettings {
            attempts: 2,
            interval: Duration::ZERO,
        });
        (svc, chain)
    }

    fn run(cmd: Command, svc: &mut PaymentService<Arc<MockChain>>, yes: bool) -> String {
        let mut out = Vec::new();
        execute(
            &cmd,
            svc,
            &MockParser,
            &Always(yes),
            &mut out,
            crate::audit::unix_now(),
        )
        .unwrap();
        String::from_utf8(out).unwrap()
    }

    fn pay(text: &str) -> Command {
        Command::Pay {
            request: text.split_whitespace().map(String::from).collect(),
        }
    }

    // The two-act demo: a normal payment, then one the AI understands but policy blocks.
    #[test]
    fn demo_flow_end_to_end() {
        let (mut svc, chain) = service();

        let ok = run(pay("send 0.1 SOL to Ada for API invoice"), &mut svc, true);
        assert!(
            ok.contains("✓ Confirmed on Devnet")
                && ok.contains("explorer.solana.com/tx/")
                && ok.contains("cluster=devnet"),
            "{ok}"
        );

        let blocked = run(pay("send 0.9 SOL to Ada"), &mut svc, true);
        assert!(
            blocked.contains("Blocked by policy") && blocked.contains("per-transaction limit"),
            "{blocked}"
        );

        let stranger = run(
            pay(&format!("send 0.1 SOL to {}", Pubkey::new_unique())),
            &mut svc,
            true,
        );
        assert!(
            stranger.contains("not on the approved recipient list"),
            "{stranger}"
        );

        assert_eq!(
            chain.sent().len(),
            1,
            "only the legitimate payment reached the chain"
        );

        let history = run(Command::History { limit: 10 }, &mut svc, true);
        assert!(
            history.contains("confirmed") && history.contains("policy_rejected"),
            "{history}"
        );
        assert!(
            history.contains("exceeds the per-transaction limit")
                || history.contains("per-transaction limit")
        );

        let budget = run(Command::Budget, &mut svc, true);
        assert!(
            budget.contains("0.1 SOL of 1 SOL used, 0.9 SOL left"),
            "{budget}"
        );
    }

    #[test]
    fn declining_is_reported_and_sends_nothing() {
        let (mut svc, chain) = service();
        let out = run(pay("send 0.1 SOL to Ada"), &mut svc, false);
        assert!(out.contains("Declined"), "{out}");
        assert!(chain.sent().is_empty());
    }

    #[test]
    fn unintelligible_requests_are_explained_and_recorded_nowhere() {
        let (mut svc, chain) = service();
        let out = run(pay("what is the weather"), &mut svc, true);
        assert!(out.contains("Could not turn that into a payment"), "{out}");
        assert!(chain.sent().is_empty());
        assert_eq!(
            run(Command::History { limit: 5 }, &mut svc, true).trim(),
            "No payments yet."
        );
    }

    #[test]
    fn address_and_reconcile_commands() {
        let (mut svc, _) = service();
        assert_eq!(
            run(Command::Address, &mut svc, true).trim(),
            svc.wallet_pubkey().to_string()
        );
        assert_eq!(
            run(Command::Reconcile, &mut svc, true).trim(),
            "Nothing in flight."
        );
    }

    #[test]
    fn keygen_creates_a_private_file_and_never_overwrites() {
        let path = std::env::temp_dir().join(format!("sg-keygen-{}.json", uuid::Uuid::new_v4()));
        let pk = keygen(&path).unwrap();
        assert_eq!(FileSigner::from_file(&path).unwrap().pubkey(), pk); // loads, so permissions are private
        assert!(matches!(keygen(&path), Err(Error::Signer(_))));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn time_formatting() {
        assert_eq!(fmt_time(0), "1970-01-01 00:00:00");
        assert_eq!(fmt_time(1_000_000_000), "2001-09-09 01:46:40");
        assert_eq!(fmt_time(1_700_000_000), "2023-11-14 22:13:20");
        assert_eq!(fmt_time(951_782_400 + 86_399), "2000-02-29 23:59:59"); // leap day
    }
}
