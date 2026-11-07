use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::Deserialize;
use solana_sdk::pubkey::Pubkey;

use crate::domain::Lamports;
use crate::error::{Error, Result};
use crate::parser::llm::{validate_base_url, DEFAULT_BASE_URL, DEFAULT_MODEL};
use crate::policy::{Allowlist, BudgetWindow, Policy};

pub const DEFAULT_RPC_URL: &str = "https://api.devnet.solana.com";

/// A string that never prints itself.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    network: RawNetwork,
    policy: RawPolicy,
    #[serde(default)]
    recipients: Vec<RawRecipient>,
    #[serde(default)]
    parser: RawParser,
    #[serde(default)]
    storage: RawStorage,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawNetwork {
    cluster: Option<String>,
    rpc_url: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPolicy {
    max_per_tx_lamports: u64,
    daily_cap_lamports: u64,
    day_boundary: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRecipient {
    label: String,
    address: String,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawParser {
    mode: Option<ParserMode>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawStorage {
    db_path: Option<String>,
}

/// Which parser `pay` uses by default. `--mock-parser` on the command line
/// always forces `Mock`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ParserMode {
    Mock,
    Llm,
}

/// Everything the program needs to start. Limits and recipients come from
/// `spendguard.toml`; secrets and machine-specific paths come from the
/// environment (`.env`), never from the TOML.
#[derive(Debug, Clone)]
pub struct Settings {
    pub policy: Policy,
    pub rpc_url: String,
    pub db_path: PathBuf,
    pub keypair_path: PathBuf,
    /// Key for the OpenAI-compatible LLM API (`SPENDGUARD_LLM_API_KEY`).
    pub api_key: Option<Secret>,
    pub model: String,
    /// Base URL of the OpenAI-compatible LLM API (`SPENDGUARD_LLM_BASE_URL`).
    /// Must be https, except for localhost.
    pub llm_base_url: String,
    pub parser_mode: ParserMode,
}

fn cfg(msg: impl Into<String>) -> Error {
    Error::Config(msg.into())
}

const PLACEHOLDER_PREFIX: &str = "REPLACE_WITH";

impl Settings {
    pub fn load(path: &Path, env: &dyn Fn(&str) -> Option<String>) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| cfg(format!("cannot read {}: {e}", path.display())))?;
        Self::from_parts(&text, env)
    }

    pub fn from_parts(toml_text: &str, env: &dyn Fn(&str) -> Option<String>) -> Result<Self> {
        let raw: RawConfig = toml::from_str(toml_text).map_err(|e| cfg(e.to_string()))?;

        // --- network: Devnet only ---
        if let Some(cluster) = &raw.network.cluster {
            if !cluster.trim().eq_ignore_ascii_case("devnet") {
                return Err(cfg(format!(
                    "network.cluster is {cluster:?}; this MVP only supports \"devnet\""
                )));
            }
        }
        let rpc_url = env("SPENDGUARD_RPC_URL")
            .or(raw.network.rpc_url)
            .unwrap_or_else(|| DEFAULT_RPC_URL.to_string());
        if rpc_url.to_ascii_lowercase().contains("mainnet") {
            return Err(cfg("this MVP is Devnet-only; refusing a mainnet RPC URL"));
        }

        // --- policy ---
        let window = match raw.policy.day_boundary.as_deref().map(str::trim) {
            None => BudgetWindow::Rolling24h,
            Some(v) if v.eq_ignore_ascii_case("utc") => BudgetWindow::UtcDay,
            Some(v) if v.eq_ignore_ascii_case("rolling") => BudgetWindow::Rolling24h,
            Some(v) => {
                return Err(cfg(format!(
                    "policy.day_boundary {v:?} must be \"utc\" or \"rolling\""
                )))
            }
        };

        // --- recipients ---
        let mut entries = Vec::new();
        for r in raw.recipients {
            let addr = r.address.trim();
            if addr.starts_with(PLACEHOLDER_PREFIX) {
                return Err(cfg(format!(
                    "recipient {:?} still has the placeholder address; paste a real Devnet address in spendguard.toml",
                    r.label
                )));
            }
            let key = Pubkey::from_str(addr).map_err(|_| {
                cfg(format!(
                    "recipient {:?}: {addr:?} is not a valid Solana address",
                    r.label
                ))
            })?;
            entries.push((r.label, key));
        }
        let policy = Policy::new(
            Lamports::new(raw.policy.max_per_tx_lamports),
            Lamports::new(raw.policy.daily_cap_lamports),
            Allowlist::new(entries)?,
        )?
        .with_window(window);

        let non_empty = |k: &str| env(k).filter(|v| !v.trim().is_empty());

        // --- LLM: the API key is sent to this URL, so it must be https (or localhost) ---
        let llm_base_url =
            non_empty("SPENDGUARD_LLM_BASE_URL").unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        validate_base_url(&llm_base_url)?;

        Ok(Self {
            policy,
            rpc_url,
            db_path: non_empty("SPENDGUARD_DB")
                .or(raw.storage.db_path)
                .map(PathBuf::from)
                .unwrap_or_else(|| "spendguard.db".into()),
            keypair_path: non_empty("SPENDGUARD_KEYPAIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| "spendguard-devnet.json".into()),
            api_key: non_empty("SPENDGUARD_LLM_API_KEY").map(Secret::new),
            model: non_empty("SPENDGUARD_MODEL").unwrap_or_else(|| DEFAULT_MODEL.to_string()),
            llm_base_url,
            parser_mode: raw.parser.mode.unwrap_or(ParserMode::Llm),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    /// The shape of the project's real spendguard.toml, with valid addresses filled in.
    const VALID: &str = r#"
[network]
cluster = "devnet"
rpc_url = "https://api.devnet.solana.com"

[policy]
max_per_tx_lamports = 100_000_000
daily_cap_lamports  = 300_000_000
day_boundary        = "utc"

[[recipients]]
label   = "ada"
address = "11111111111111111111111111111111"

[[recipients]]
label   = "invoices"
address = "Vote111111111111111111111111111111111111111"

[parser]
mode = "mock"

[storage]
db_path = "spendguard.db"
"#;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| m.get(k).cloned()
    }

    fn load(toml: &str) -> Result<Settings> {
        Settings::from_parts(toml, &env_of(&[]))
    }

    #[test]
    fn loads_the_projects_toml_schema() {
        let s = load(VALID).unwrap();
        assert_eq!(s.policy.max_per_tx(), Lamports::new(100_000_000));
        assert_eq!(s.policy.daily_cap(), Lamports::new(300_000_000));
        assert_eq!(s.policy.window(), BudgetWindow::UtcDay);
        assert_eq!(s.policy.allowlist().len(), 2);
        assert_eq!(s.parser_mode, ParserMode::Mock);
        assert_eq!(s.rpc_url, DEFAULT_RPC_URL);
        assert_eq!(s.db_path, PathBuf::from("spendguard.db"));
        assert_eq!(s.keypair_path, PathBuf::from("spendguard-devnet.json"));
        assert_eq!(s.model, DEFAULT_MODEL);
        assert_eq!(s.llm_base_url, DEFAULT_BASE_URL);
        assert!(s.api_key.is_none());
    }

    #[test]
    fn minimal_file_gets_safe_defaults() {
        let s = load("[policy]\nmax_per_tx_lamports = 1\ndaily_cap_lamports = 2\n").unwrap();
        assert_eq!(s.policy.window(), BudgetWindow::Rolling24h); // stricter default
        assert_eq!(s.parser_mode, ParserMode::Llm);
        assert!(s.policy.allowlist().is_empty());
    }

    #[test]
    fn day_boundary_options() {
        for (v, want) in [
            ("utc", BudgetWindow::UtcDay),
            ("UTC", BudgetWindow::UtcDay),
            ("rolling", BudgetWindow::Rolling24h),
        ] {
            let s = load(&VALID.replace("\"utc\"", &format!("\"{v}\""))).unwrap();
            assert_eq!(s.policy.window(), want, "{v}");
        }
        assert!(matches!(
            load(&VALID.replace("\"utc\"", "\"weekly\"")),
            Err(Error::Config(_))
        ));
    }

    #[test]
    fn only_devnet_is_accepted() {
        for bad in ["mainnet-beta", "testnet", "localnet"] {
            assert!(
                load(&VALID.replace("\"devnet\"", &format!("\"{bad}\""))).is_err(),
                "{bad}"
            );
        }
        let toml = VALID.replace(
            "https://api.devnet.solana.com",
            "https://api.mainnet-beta.solana.com",
        );
        assert!(load(&toml).is_err());
        assert!(Settings::from_parts(
            VALID,
            &env_of(&[("SPENDGUARD_RPC_URL", "https://api.mainnet-beta.solana.com")])
        )
        .is_err());
    }

    #[test]
    fn placeholder_addresses_fail_with_a_helpful_message() {
        let toml = VALID.replace(
            "11111111111111111111111111111111",
            "REPLACE_WITH_DEVNET_ADDRESS",
        );
        match load(&toml) {
            Err(Error::Config(m)) => {
                assert!(m.contains("\"ada\"") && m.contains("placeholder"), "{m}")
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn environment_overrides_and_secret_is_redacted() {
        let env = env_of(&[
            ("SPENDGUARD_LLM_API_KEY", "gsk-abc123"),
            ("SPENDGUARD_DB", "/tmp/x.db"),
            ("SPENDGUARD_KEYPAIR", "/tmp/k.json"),
            ("SPENDGUARD_MODEL", "some-model"),
            ("SPENDGUARD_LLM_BASE_URL", "http://localhost:11434/v1"),
            ("SPENDGUARD_RPC_URL", "https://example.devnet.rpc"),
        ]);
        let s = Settings::from_parts(VALID, &env).unwrap();
        assert_eq!(s.api_key.as_ref().unwrap().expose(), "gsk-abc123");
        assert_eq!(
            (s.model.as_str(), s.rpc_url.as_str()),
            ("some-model", "https://example.devnet.rpc")
        );
        assert_eq!(s.llm_base_url, "http://localhost:11434/v1");
        assert_eq!(s.db_path, PathBuf::from("/tmp/x.db")); // env beats [storage]
        assert!(!format!("{s:?}").contains("gsk-abc123"));
    }

    #[test]
    fn blank_env_values_fall_back() {
        let s = Settings::from_parts(
            VALID,
            &env_of(&[
                ("SPENDGUARD_LLM_API_KEY", "  "),
                ("SPENDGUARD_DB", ""),
                ("SPENDGUARD_LLM_BASE_URL", " "),
            ]),
        )
        .unwrap();
        assert!(s.api_key.is_none());
        assert_eq!(s.db_path, PathBuf::from("spendguard.db")); // falls through to [storage]
        assert_eq!(s.llm_base_url, DEFAULT_BASE_URL);
    }

    #[test]
    fn the_llm_base_url_cannot_send_the_key_over_plain_http() {
        for bad in [
            "http://api.example.com/v1",
            "ftp://example.com",
            "api.groq.com/openai/v1",
        ] {
            let r = Settings::from_parts(VALID, &env_of(&[("SPENDGUARD_LLM_BASE_URL", bad)]));
            assert!(matches!(r, Err(Error::Config(_))), "{bad}");
        }
    }

    #[test]
    fn rejects_bad_configuration() {
        let bad = [
            VALID.replace("100_000_000", "\"0.1 SOL\""), // amounts are integer lamports
            VALID.replace("100_000_000", "0"),           // zero per-tx limit
            VALID.replace("300_000_000", "50_000_000"),  // daily < per-tx
            VALID.replace(
                "Vote111111111111111111111111111111111111111",
                "not-an-address",
            ),
            VALID.replace("\"invoices\"", "\"ADA\""), // duplicate label (case-insensitive)
            VALID.replace("\"mock\"", "\"gpt\""),     // unknown parser mode
            format!("{VALID}\n[extra]\nx = 1\n"),     // unknown section
            VALID.replace("[policy]", "[policy]\nunknown_key = 1"), // unknown field
            "[network]\ncluster = \"devnet\"".to_string(), // missing [policy]
            "this is not toml".to_string(),
        ];
        for toml in bad {
            assert!(
                matches!(
                    load(&toml),
                    Err(Error::Config(_)) | Err(Error::InvalidPolicy(_))
                ),
                "should reject:\n{toml}"
            );
        }
    }

    #[test]
    fn missing_file_is_a_clear_error() {
        let r = Settings::load(Path::new("/nonexistent/spendguard.toml"), &env_of(&[]));
        assert!(matches!(r, Err(Error::Config(m)) if m.contains("cannot read")));
    }
}
