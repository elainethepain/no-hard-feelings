use std::collections::HashSet;
use std::sync::LazyLock;

use anyhow::{Context, Result};
use serde::Deserialize;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_pubkey::Pubkey;
use solana_sdk::{
    signature::Signature,
    signer::Signer,
    transaction::VersionedTransaction,
};
use tracing::debug;

use crate::consts::KSWAP_API;

/// Known swap program IDs that are expected in kswap transactions.
/// Any instruction targeting a program not in this list is rejected.
const ALLOWED_PROGRAMS: &[&str] = &[
    // Jupiter v6
    "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4",
    // Jupiter DCA
    "DCAK36VfExkPdAkYUQg6ewgxyinvcEyPLyHjRbmveKFw",
    // Associated Token Account program
    "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL",
    // Token program
    "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
    // Token-2022
    "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb",
    // System program
    "11111111111111111111111111111111",
    // Compute budget
    "ComputeBudget111111111111111111111111111111",
    // Kamino kswap router
    "KSwapuSniperzQDVCi8ENJBqt9LuPHBRLbqy1bDMJnk",
    // Raydium AMM
    "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8",
    // Raydium CLMM
    "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK",
    // Orca Whirlpool
    "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc",
    // Meteora DLMM
    "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo",
    // Phoenix
    "PhoeNiXZ8ByJGLkxNfZRnkUfjvmuYqLR89jjFHGqdXY",
    // Openbook v2
    "opnb2LAfJYbRMAHHvqjCwQxanZn7ReEHp1k81EQMQvR",
    // Memo program (used by Jupiter)
    "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr",
    // Sysvar instructions
    "Sysvar1nstructions1111111111111111111111111",
];

/// Parsed allowlist — computed once on first use.
static ALLOWED_PROGRAM_SET: LazyLock<HashSet<Pubkey>> = LazyLock::new(|| {
    ALLOWED_PROGRAMS
        .iter()
        .filter_map(|s| s.parse::<Pubkey>().ok())
        .collect()
});

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct KswapResponse {
    data: KswapData,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KswapData {
    pub transaction: String,
    pub expected_amount_out: String,
    pub min_amount_out: String,
    pub router_type: String,
}

// ---------------------------------------------------------------------------
// Quote
// ---------------------------------------------------------------------------

/// Fetch a swap quote from the kswap API.
pub async fn get_swap_quote(
    http: &reqwest::Client,
    input_mint: &Pubkey,
    output_mint: &Pubkey,
    amount_in: u64,
    wallet: &Pubkey,
    slippage_bps: u16,
) -> Result<KswapData> {
    let resp = http
        .get(KSWAP_API)
        .query(&[
            ("tokenIn", input_mint.to_string()),
            ("tokenOut", output_mint.to_string()),
            ("amountIn", amount_in.to_string()),
            ("maxSlippageBps", slippage_bps.to_string()),
            ("wallet", wallet.to_string()),
            ("wrapAndUnwrapSol", "true".to_string()),
            ("includeSetupIxs", "true".to_string()),
        ])
        .send()
        .await
        .context("kswap API request failed")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("kswap API error {status}: {body}");
    }

    let kswap: KswapResponse = resp.json().await.context("Failed to parse kswap response")?;
    Ok(kswap.data)
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Validate that a kswap pre-built transaction only contains instructions from
/// known, trusted programs. Rejects transactions with unexpected program IDs
/// that could drain the wallet.
fn validate_swap_transaction(
    tx: &VersionedTransaction,
    wallet: &Pubkey,
) -> Result<()> {
    let account_keys = tx.message.static_account_keys();

    for ix in tx.message.instructions() {
        let program_id = account_keys
            .get(ix.program_id_index as usize)
            .ok_or_else(|| anyhow::anyhow!("Invalid program_id_index in swap tx"))?;

        if !ALLOWED_PROGRAM_SET.contains(program_id) {
            anyhow::bail!(
                "Swap transaction contains untrusted program: {program_id}. \
                 Refusing to sign — possible malicious transaction."
            );
        }
    }

    // Verify the wallet is the fee payer (first account).
    if let Some(first_key) = account_keys.first() {
        if first_key != wallet {
            anyhow::bail!(
                "Swap transaction fee payer {first_key} does not match wallet {wallet}"
            );
        }
    }

    debug!(
        instruction_count = tx.message.instructions().len(),
        "Swap transaction validated"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Send
// ---------------------------------------------------------------------------

/// Decode a kswap pre-built transaction, validate its instructions,
/// update its blockhash, re-sign, and send.
pub async fn send_swap_transaction(
    rpc: &RpcClient,
    signer: &dyn Signer,
    quote: &KswapData,
) -> Result<Signature> {
    use base64::Engine;

    let tx_bytes = base64::engine::general_purpose::STANDARD
        .decode(&quote.transaction)
        .or_else(|_| {
            solana_sdk::bs58::decode(&quote.transaction)
                .into_vec()
                .map_err(|e| anyhow::anyhow!("Failed to decode swap tx: {e}"))
        })
        .context("Failed to decode kswap transaction bytes")?;

    let swap_tx: VersionedTransaction =
        bincode::deserialize(&tx_bytes).context("Failed to deserialize swap transaction")?;

    // Validate before signing — reject transactions with untrusted programs.
    validate_swap_transaction(&swap_tx, &signer.pubkey())?;

    let blockhash = rpc
        .get_latest_blockhash()
        .await
        .context("Failed to get blockhash for swap")?;

    let mut msg = swap_tx.message;
    msg.set_recent_blockhash(blockhash);

    let signed =
        VersionedTransaction::try_new(msg, &[signer]).context("Failed to sign swap tx")?;

    let sig = rpc
        .send_and_confirm_transaction(&signed)
        .await
        .context("Swap transaction failed")?;

    Ok(sig)
}
