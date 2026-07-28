use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_instruction::Instruction;
use solana_pubkey::Pubkey;
use solana_sdk::address_lookup_table::{state::AddressLookupTable, AddressLookupTableAccount};
use tracing::{debug, info};

const KAMINO_API: &str = "https://api.kamino.finance";

// ---------------------------------------------------------------------------
// Kamino LUT API types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FindLutsRequest {
    addresses: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    verify: Option<bool>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    user_accounts: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FindLutsResponse {
    lut_addresses: Vec<String>,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Collect all account pubkeys referenced by a set of instructions.
pub fn collect_instruction_keys(instructions: &[Instruction]) -> Vec<Pubkey> {
    let mut keys = std::collections::HashSet::new();
    for ix in instructions {
        keys.insert(ix.program_id);
        for meta in &ix.accounts {
            keys.insert(meta.pubkey);
        }
    }
    keys.into_iter().collect()
}

/// Query Kamino's LUT API for the smallest set of lookup tables that cover
/// the given addresses, then fetch those LUT accounts from the RPC.
pub async fn resolve_luts(
    http: &reqwest::Client,
    rpc: &RpcClient,
    addresses: &[Pubkey],
    user_accounts: &[Pubkey],
) -> Result<Vec<AddressLookupTableAccount>> {
    if addresses.is_empty() {
        return Ok(vec![]);
    }

    let lut_pubkeys = find_luts_from_api(http, addresses, user_accounts).await?;
    if lut_pubkeys.is_empty() {
        debug!("No LUTs found for addresses");
        return Ok(vec![]);
    }

    info!(count = lut_pubkeys.len(), "Resolved LUTs from Kamino API");

    let mut lut_accounts = Vec::new();
    for lut_pk in &lut_pubkeys {
        match fetch_lut_account(rpc, lut_pk).await {
            Ok(lut) => lut_accounts.push(lut),
            Err(e) => debug!(lut = %lut_pk, error = %e, "Failed to fetch LUT, skipping"),
        }
    }

    Ok(lut_accounts)
}

// ---------------------------------------------------------------------------
// Private
// ---------------------------------------------------------------------------

/// Call Kamino's POST /luts/find-minimal to find covering LUTs.
async fn find_luts_from_api(
    http: &reqwest::Client,
    addresses: &[Pubkey],
    user_accounts: &[Pubkey],
) -> Result<Vec<Pubkey>> {
    let request = FindLutsRequest {
        addresses: addresses.iter().map(|pk| pk.to_string()).collect(),
        verify: Some(true),
        user_accounts: user_accounts.iter().map(|pk| pk.to_string()).collect(),
    };

    let resp = http
        .post(format!("{KAMINO_API}/luts/find-minimal"))
        .json(&request)
        .send()
        .await
        .context("Kamino LUT API request failed")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Kamino LUT API error {status}: {body}");
    }

    let response: FindLutsResponse = resp
        .json()
        .await
        .context("Failed to parse Kamino LUT response")?;

    let pubkeys: Vec<Pubkey> = response
        .lut_addresses
        .iter()
        .filter_map(|s| s.parse::<Pubkey>().ok())
        .collect();

    Ok(pubkeys)
}

/// Fetch and deserialize a single AddressLookupTable account from the RPC.
async fn fetch_lut_account(
    rpc: &RpcClient,
    lut_pubkey: &Pubkey,
) -> Result<AddressLookupTableAccount> {
    let account = rpc
        .get_account(lut_pubkey)
        .await
        .with_context(|| format!("Failed to fetch LUT account {lut_pubkey}"))?;

    let lookup_table = AddressLookupTable::deserialize(&account.data)
        .map_err(|e| anyhow::anyhow!("Failed to deserialize LUT {lut_pubkey}: {e}"))?;

    Ok(AddressLookupTableAccount {
        key: *lut_pubkey,
        addresses: lookup_table.addresses.to_vec(),
    })
}
