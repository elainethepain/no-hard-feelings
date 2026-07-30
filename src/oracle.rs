//! Fresh oracle price fetching via Scope on-chain accounts.
//!
//! Scope is Kamino's oracle aggregator. Each lending market references a Scope
//! `OraclePrices` account that contains up to 512 price entries. Each reserve's
//! `scope_configuration.price_chain` specifies which entry index holds its price.
//!
//! By fetching the Scope account directly from RPC and parsing the price at the
//! correct index, we get the same fresh prices the on-chain program would see
//! after a refresh — without actually submitting refresh instructions.
//!
//! The Scope types below are vendored from `scope-types` v2.0.0
//! (https://github.com/Kamino-Finance/scope, release/0.37.0,
//! programs/scope-types/src/states/). The crate itself is incompatible with
//! Solana SDK v2.3 due to Anchor/borsh version conflicts. These struct layouts
//! are stable — changing them would break the on-chain program.

use std::collections::HashMap;

use anyhow::{Context, Result};
use bytemuck::{Pod, Zeroable};
use klend_interface::{state::Reserve, Fraction};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_pubkey::Pubkey;
use tracing::{debug, warn};

use crate::consts::KLEND_NULL_PUBKEY;

// ---------------------------------------------------------------------------
// Vendored Scope types (from scope-types v2.0.0)
// ---------------------------------------------------------------------------

/// Scope oracle price. Integer + exponent representation.
/// Decimal price = value / 10^exp.
/// Example: BTC at $64,622.369 → value=6462236900000, exp=8.
#[derive(Debug, Clone, Copy, Default, Pod, Zeroable)]
#[repr(C)]
pub struct Price {
    pub value: u64,
    pub exp: u64,
}

impl Price {
    pub fn to_f64(self) -> f64 {
        if self.value == 0 {
            return 0.0;
        }
        self.value as f64 / 10f64.powi(self.exp as i32)
    }
}

/// A timestamped price entry in the Scope OraclePrices account.
#[derive(Debug, Clone, Copy, Default, Pod, Zeroable)]
#[repr(C)]
pub struct DatedPrice {
    pub price: Price,
    pub last_updated_slot: u64,
    pub unix_timestamp: u64,
    pub _padding: [u8; 24],
}

const MAX_ENTRIES: usize = 512;
const ANCHOR_DISCRIMINATOR_SIZE: usize = 8;
const PRICE_CHAIN_NONE: u16 = 0xFFFF;

/// Parsed Scope OraclePrices (just the price entries we need).
pub struct OraclePrices {
    pub prices: [DatedPrice; MAX_ENTRIES],
}

impl OraclePrices {
    /// Parse from raw account data (skips the 8-byte Anchor discriminator).
    pub fn from_account_data(data: &[u8]) -> Option<Self> {
        let body = data.get(ANCHOR_DISCRIMINATOR_SIZE..)?;
        // Skip 32-byte oracle_mappings pubkey.
        let prices_data = body.get(32..)?;
        let entry_size = std::mem::size_of::<DatedPrice>();
        if prices_data.len() < MAX_ENTRIES * entry_size {
            return None;
        }
        let mut prices = [DatedPrice::default(); MAX_ENTRIES];
        for i in 0..MAX_ENTRIES {
            let offset = i * entry_size;
            prices[i] = *bytemuck::from_bytes(&prices_data[offset..offset + entry_size]);
        }
        Some(Self { prices })
    }

    pub fn get_price(&self, entry_index: usize) -> Option<&DatedPrice> {
        let entry = self.prices.get(entry_index)?;
        if entry.price.value == 0 {
            return None;
        }
        Some(entry)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Get all valid price chain indices from a reserve's scope configuration.
///
/// The price chain is an array of up to 4 Scope entry indices. For single-hop
/// tokens (most), only the first entry is valid. For multi-hop tokens (e.g.
/// mSOL → SOL → USD), multiple entries are valid. The final price is the
/// product of all prices along the chain.
fn scope_price_chain(reserve: &Reserve) -> Vec<usize> {
    reserve
        .config
        .token_info
        .scope_configuration
        .price_chain
        .iter()
        .take_while(|&&idx| idx != PRICE_CHAIN_NONE)
        .map(|&idx| idx as usize)
        .collect()
}

/// Resolve a chained price from a Scope OraclePrices account.
/// Multiplies prices along the chain to produce the final USD price.
fn resolve_price_chain(oracle_prices: &OraclePrices, chain: &[usize]) -> Option<f64> {
    if chain.is_empty() {
        return None;
    }
    let mut result = 1.0f64;
    for &index in chain {
        let dated_price = oracle_prices.get_price(index)?;
        let price = dated_price.price.to_f64();
        if price <= 0.0 {
            return None;
        }
        result *= price;
    }
    Some(result)
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Fetch fresh oracle prices for all reserves from their Scope price accounts.
///
/// Returns a map from reserve pubkey to USD price (f64). Reserves without a
/// valid Scope configuration or whose prices can't be parsed are skipped.
pub async fn fetch_fresh_prices(
    rpc: &RpcClient,
    reserves: &[(Pubkey, Reserve)],
) -> Result<HashMap<Pubkey, f64>> {
    // Collect unique Scope price feed accounts and which reserves use them.
    let mut feed_to_reserves: HashMap<Pubkey, Vec<(Pubkey, Vec<usize>)>> = HashMap::new();

    for (reserve_pk, reserve) in reserves {
        let feed = reserve.config.token_info.scope_configuration.price_feed;
        if feed == Pubkey::default() || feed == KLEND_NULL_PUBKEY {
            continue;
        }
        let chain = scope_price_chain(reserve);
        if chain.is_empty() {
            continue;
        }
        feed_to_reserves
            .entry(feed)
            .or_default()
            .push((*reserve_pk, chain));
    }

    if feed_to_reserves.is_empty() {
        debug!("No Scope-configured reserves found");
        return Ok(HashMap::new());
    }

    // Fetch all unique Scope accounts in one RPC call.
    let feed_pks: Vec<Pubkey> = feed_to_reserves.keys().copied().collect();
    let feed_accounts = rpc
        .get_multiple_accounts(&feed_pks)
        .await
        .context("Failed to fetch Scope oracle accounts")?;

    // Parse prices.
    let mut prices = HashMap::new();

    for (feed_pk, acc_opt) in feed_pks.iter().zip(feed_accounts.iter()) {
        let acc = match acc_opt {
            Some(a) => a,
            None => {
                warn!(feed = %feed_pk, "Scope account not found");
                continue;
            }
        };

        let oracle_prices = match OraclePrices::from_account_data(&acc.data) {
            Some(op) => op,
            None => {
                warn!(feed = %feed_pk, "Failed to parse Scope OraclePrices");
                continue;
            }
        };

        let reserve_entries = match feed_to_reserves.get(feed_pk) {
            Some(entries) => entries,
            None => continue,
        };

        for (reserve_pk, chain) in reserve_entries {
            match resolve_price_chain(&oracle_prices, chain) {
                Some(usd) => {
                    debug!(reserve = %reserve_pk, chain = ?chain, price = usd, "Scope price");
                    prices.insert(*reserve_pk, usd);
                }
                None => {
                    debug!(reserve = %reserve_pk, chain = ?chain, "Failed to resolve Scope price chain");
                }
            }
        }
    }

    debug!(count = prices.len(), "Fetched fresh oracle prices");
    Ok(prices)
}

/// Apply fresh oracle prices to reserves by overwriting `market_price_sf`.
///
/// This gives `process_obligations()` accurate price data for scoring,
/// eliminating false positives from stale on-chain obligation values.
pub fn apply_fresh_prices(reserves: &mut [(Pubkey, Reserve)], fresh_prices: &HashMap<Pubkey, f64>) {
    for (reserve_pk, reserve) in reserves.iter_mut() {
        if let Some(&usd_price) = fresh_prices.get(reserve_pk) {
            let price_sf: u128 = Fraction::from_num(usd_price).to_bits();
            reserve.liquidity.market_price_sf = klend_interface::state::PodU128::from(price_sf);
            debug!(reserve = %reserve_pk, price = usd_price, "Applied fresh price");
        }
    }
}
