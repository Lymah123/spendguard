use std::io::{stdin, stdout, BufReader};
use std::process::ExitCode;

use clap::Parser;

use spendguard::audit::{unix_now, PaymentStore};
use spendguard::cli::{execute, keygen, Cli, Command, PromptApprover};
use spendguard::config::{ParserMode, Settings};
use spendguard::parser::llm::LlmParser;
use spendguard::parser::{IntentParser, MockParser};
use spendguard::service::PaymentService;
use spendguard::solana::client::RpcChain;
use spendguard::wallet::signer::FileSigner;

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let _ = dotenvy::dotenv(); // optional .env
    let cli = Cli::parse();

    if let Command::Keygen { path } = &cli.command {
        let pubkey = keygen(path)?;
        println!("Created {}\nAddress: {pubkey}", path.display());
        println!("Fund it on Devnet with https://faucet.solana.com or: solana airdrop 2 {pubkey} --url devnet");
        return Ok(());
    }

    let settings = Settings::load(&cli.config, &|k| std::env::var(k).ok())?;
    let signer = FileSigner::from_file(&settings.keypair_path)?;
    let store = PaymentStore::open(&settings.db_path)?;
    let mut svc = PaymentService::new(
        store,
        settings.policy.clone(),
        RpcChain::new(&settings.rpc_url),
        signer,
    );

    // Only `pay` talks to a language model, so only `pay` needs an API key.
    let parser: Box<dyn IntentParser> = if cli.mock_parser
        || settings.parser_mode == ParserMode::Mock
        || !matches!(cli.command, Command::Pay { .. })
    {
        Box::new(MockParser)
    } else {
        match settings.api_key.clone() {
            Some(key) => Box::new(LlmParser::openai_compatible(key, settings.model.clone(), &settings.llm_base_url)?),
            None => {
                return Err("parser mode is \"llm\" but SPENDGUARD_LLM_API_KEY is not set. Add it to .env, or set [parser] mode = \"mock\" / pass --mock-parser.".into())
            }
        }
    };

    let approver = PromptApprover::new(BufReader::new(stdin()), stdout());
    execute(
        &cli.command,
        &mut svc,
        parser.as_ref(),
        &approver,
        &mut stdout(),
        unix_now(),
    )?;
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
