use std::collections::HashMap;

use klend_interface::state::{Obligation, Reserve};
use serde::{Deserialize, Serialize};
use solana_pubkey::Pubkey;
use solana_sdk::address_lookup_table::AddressLookupTableAccount;

use crate::config::BotConfig;

/// A reserve paired with its on-chain pubkey.
#[derive(Debug, Clone, Copy)]
pub struct ReserveWithKey<'a> {
    pub pubkey: Pubkey,
    pub reserve: &'a Reserve,
}

/// Shared context for executing liquidation attempts within a cycle.
pub struct ExecutionContext<'a> {
    pub bot: &'a BotConfig,
    pub data: &'a crate::client::MarketData,
    pub obligations: &'a [(Pubkey, Obligation)],
    pub holdings: &'a crate::liquidator::Holdings,
    pub luts: &'a [AddressLookupTableAccount],
    pub token_program_cache: &'a HashMap<Pubkey, Pubkey>,
}

/// Options that control execution behavior.
pub struct ExecutionOptions {
    pub max_attempts: usize,
    pub send: bool,
    pub priority_fee: u64,
    pub budget: f64,
}

/// Scan result written to disk by `nhf scan` and consumed by `nhf execute`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanResult {
    pub pubkey: String,
    pub deposited_usd: f64,
    pub adjusted_debt_usd: f64,
    pub actual_debt_usd: f64,
    pub unhealthy_limit_usd: f64,
}

/// A scored liquidation opportunity.
#[derive(Debug, Clone)]
pub struct ScoredEntry {
    pub obligation: Pubkey,
    pub withdraw_reserve: Pubkey,
    pub repay_reserve: Pubkey,
    pub withdraw_mint: Pubkey,
    pub repay_mint: Pubkey,
    pub repay_amount: u64,
    pub estimated_profit_usd: f64,
}
