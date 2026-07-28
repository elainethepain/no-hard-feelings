use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_pubkey::Pubkey;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    signature::{read_keypair_file, Keypair},
    signer::Signer,
};
use tracing::info;

/// HTTP timeout for external API calls (kswap, Kamino LUT API).
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// Fully resolved bot configuration.
pub struct BotConfig {
    pub rpc: RpcClient,
    pub signer: Keypair,
    pub owner: Pubkey,
    pub markets: Option<Vec<Pubkey>>,
    /// Shared HTTP client with connection pooling and timeouts.
    pub http: reqwest::Client,
}

/// Build a `BotConfig` from CLI / env values.
pub fn load_config(
    rpc_url: &str,
    keypair_path: Option<&PathBuf>,
    markets: Option<&Vec<Pubkey>>,
) -> Result<BotConfig> {
    let rpc = RpcClient::new_with_timeout_and_commitment(
        rpc_url.to_string(),
        Duration::from_secs(120),
        CommitmentConfig::confirmed(),
    );

    let home = std::env::var("HOME").unwrap_or_default();
    let default_path = PathBuf::from(format!("{home}/.config/solana/id.json"));
    let kp_path = keypair_path.unwrap_or(&default_path);

    let signer = read_keypair_file(kp_path)
        .map_err(|e| anyhow::anyhow!("Failed to load keypair from {}: {e}", kp_path.display()))?;
    let owner = signer.pubkey();

    let http = reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .pool_max_idle_per_host(4)
        .build()
        .map_err(|e| anyhow::anyhow!("Failed to build HTTP client: {e}"))?;

    info!(wallet = %owner, rpc = %rpc_url, "Loaded config");

    Ok(BotConfig {
        rpc,
        signer,
        owner,
        markets: markets.cloned(),
        http,
    })
}
