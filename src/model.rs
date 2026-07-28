use serde::{Deserialize, Serialize};
use solana_pubkey::Pubkey;

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
