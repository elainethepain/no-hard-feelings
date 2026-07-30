use klend_interface::{
    state::{LendingMarket, Obligation, ObligationLiquidity, Reserve},
    Fraction,
};
use solana_pubkey::Pubkey;

use crate::consts::{DEFAULT_MIN_DEBT_USD, FULL_BPS, GAS_COST_USD};
use crate::model::ScoredEntry;

// ---------------------------------------------------------------------------
// Fraction helpers
// ---------------------------------------------------------------------------

/// Convert a kLend scaled fraction (u128) to f64 for display and comparison.
pub fn scaled_to_f64(value: u128) -> f64 {
    Fraction::from_bits(value).to_num()
}

/// Safe division that returns 0.0 instead of NaN/Inf.
fn safe_div(a: f64, b: f64) -> f64 {
    if b == 0.0 || !b.is_finite() || !a.is_finite() {
        0.0
    } else {
        a / b
    }
}

/// Compute the cToken → liquidity exchange rate for a reserve.
///
/// exchange_rate = total_liquidity / ctoken_supply
/// where total_liquidity = available_amount + borrowed_amount
///
/// Returns 1.0 if ctoken supply is 0 (no deposits yet).
fn ctoken_exchange_rate(reserve: &Reserve) -> f64 {
    let available = reserve.liquidity.total_available_amount as f64;
    let borrowed: f64 =
        Fraction::from_bits(u128::from(reserve.liquidity.borrowed_amount_sf)).to_num();
    let total_liquidity = available + borrowed;
    let ctoken_supply = reserve.collateral.mint_total_supply as f64;
    if ctoken_supply <= 0.0 {
        return 1.0;
    }
    safe_div(total_liquidity, ctoken_supply)
}

// ---------------------------------------------------------------------------
// Obligation filtering and stats
// ---------------------------------------------------------------------------

/// Pre-filter using fresh reserve prices instead of stale on-chain obligation values.
///
/// Recalculates deposit and borrow USD values from the obligation's token amounts
/// and the current reserve prices (which should have fresh oracle prices applied).
/// This catches obligations that became liquidatable since the last on-chain refresh,
/// and filters out those that were repaid but still show stale unhealthy values.
pub fn filter_obligation_fresh(
    obligation: &Obligation,
    reserves: &[(Pubkey, &Reserve)],
    min_debt_usd: f64,
) -> bool {
    if obligation.has_debt == 0 {
        return false;
    }

    let (deposited_usd, weighted_threshold) = {
        let mut total_deposit = 0.0f64;
        let mut weighted_thresh = 0.0f64;

        for deposit in &obligation.deposits {
            if deposit.deposit_reserve == Pubkey::default() || deposit.deposited_amount == 0 {
                continue;
            }
            if let Some((_, reserve)) = reserves
                .iter()
                .find(|(pk, _)| *pk == deposit.deposit_reserve)
            {
                let price = scaled_to_f64(u128::from(reserve.liquidity.market_price_sf));
                let decimals = reserve.liquidity.mint_decimals;
                let exchange_rate = ctoken_exchange_rate(reserve);
                let deposit_value = deposit.deposited_amount as f64 * exchange_rate
                    / 10f64.powi(decimals as i32)
                    * price;
                total_deposit += deposit_value;
                weighted_thresh +=
                    deposit_value * reserve.liquidation_threshold_pct() as f64 / 100.0;
            }
        }
        (total_deposit, weighted_thresh)
    };

    if deposited_usd <= 0.0 {
        return false;
    }

    let borrowed_usd = {
        let mut total_borrow = 0.0f64;

        for borrow in &obligation.borrows {
            if borrow.borrow_reserve == Pubkey::default() {
                continue;
            }
            let borrowed_amount: f64 = Fraction::from_bits(borrow.borrowed_amount()).to_num();
            if borrowed_amount <= 0.0 {
                continue;
            }
            if let Some((_, reserve)) = reserves.iter().find(|(pk, _)| *pk == borrow.borrow_reserve)
            {
                let price = scaled_to_f64(u128::from(reserve.liquidity.market_price_sf));
                let decimals = reserve.liquidity.mint_decimals;
                let borrow_value = borrowed_amount / 10f64.powi(decimals as i32) * price;
                let borrow_factor = reserve.borrow_factor_pct() as f64 / 100.0;
                total_borrow += borrow_value * borrow_factor;
            }
        }
        total_borrow
    };

    if borrowed_usd < min_debt_usd {
        return false;
    }

    // Liquidatable when borrow-factor-adjusted debt exceeds the weighted threshold.
    borrowed_usd >= weighted_threshold
}

/// Summary statistics for an obligation.
#[derive(Debug, Clone)]
pub struct ObligationStats {
    pub deposited_usd: f64,
    pub adjusted_debt_usd: f64,
    pub actual_debt_usd: f64,
    pub unhealthy_limit_usd: f64,
    pub ltv_actual: f64,
}

/// Extract display stats from a parsed obligation.
pub fn obligation_stats(obligation: &Obligation) -> ObligationStats {
    let deposited = scaled_to_f64(obligation.deposited_value());
    let adjusted_debt = scaled_to_f64(obligation.borrow_factor_adjusted_debt_value());
    let actual_debt = scaled_to_f64(obligation.borrowed_assets_market_value());
    let unhealthy = scaled_to_f64(obligation.unhealthy_borrow_value());
    let ltv_actual = if deposited > 0.0 {
        actual_debt / deposited
    } else {
        0.0
    };

    ObligationStats {
        deposited_usd: deposited,
        adjusted_debt_usd: adjusted_debt,
        actual_debt_usd: actual_debt,
        unhealthy_limit_usd: unhealthy,
        ltv_actual,
    }
}

// ---------------------------------------------------------------------------
// Pair selection
// ---------------------------------------------------------------------------

/// Select the best collateral-to-seize / debt-to-repay pair.
///
/// kLend enforces on-chain:
/// - Seize the deposit with the lowest `liquidation_threshold_pct` (weakest collateral)
/// - Repay the borrow with the highest `borrow_factor_pct` (riskiest debt)
/// - Deposits with `loan_to_value_pct == 0` cannot be seized (deposit-only)
pub fn select_pair<'a>(
    obligation: &Obligation,
    reserves: &'a [(Pubkey, &'a Reserve)],
) -> Option<(Pubkey, &'a Reserve, Pubkey, &'a Reserve)> {
    let mut deposit_pairs: Vec<(Pubkey, &Reserve)> = obligation
        .deposits
        .iter()
        .filter(|d| d.deposit_reserve != Pubkey::default())
        .filter_map(|d| {
            reserves
                .iter()
                .find(|(pk, _)| *pk == d.deposit_reserve)
                .map(|(pk, r)| (*pk, *r))
        })
        .collect();
    deposit_pairs.sort_by_key(|(_, r)| r.liquidation_threshold_pct());

    let mut borrow_pairs: Vec<(Pubkey, &Reserve)> = obligation
        .borrows
        .iter()
        .filter(|b| b.borrow_reserve != Pubkey::default())
        .filter_map(|b| {
            reserves
                .iter()
                .find(|(pk, _)| *pk == b.borrow_reserve)
                .map(|(pk, r)| (*pk, *r))
        })
        .collect();
    borrow_pairs.sort_by(|(_, a), (_, b)| b.borrow_factor_pct().cmp(&a.borrow_factor_pct()));

    let withdraw = deposit_pairs
        .iter()
        .find(|(_, r)| r.loan_to_value_pct() > 0)?;
    let repay = borrow_pairs.first()?;

    Some((withdraw.0, withdraw.1, repay.0, repay.1))
}

/// Locate a specific borrow position within an obligation.
pub fn find_borrow_position<'a>(
    obligation: &'a Obligation,
    reserve: &Pubkey,
) -> Option<&'a ObligationLiquidity> {
    obligation
        .borrows
        .iter()
        .find(|b| b.borrow_reserve == *reserve && b.borrow_reserve != Pubkey::default())
}

// ---------------------------------------------------------------------------
// Liquidation amounts
// ---------------------------------------------------------------------------

/// Calculate the maximum repayable amount (native token units) for a single liquidation.
///
/// Accounts for: close factor, insolvency override, dust threshold,
/// and max liquidatable debt market value.
///
/// `debt_price` and `debt_decimals` should come from the reserve with fresh
/// oracle prices applied. Used to compute the debt market value instead of
/// reading the stale `borrow_position.market_value()`.
pub fn max_liquidatable_amount(
    obligation: &Obligation,
    lending_market: &LendingMarket,
    borrow_position: &ObligationLiquidity,
    debt_price: f64,
    debt_decimals: u64,
) -> u64 {
    let borrowed_amount: u64 = Fraction::from_bits(borrow_position.borrowed_amount()).to_num();
    if borrowed_amount == 0 || debt_price <= 0.0 {
        return 0;
    }

    // Compute fresh debt market value from current price.
    let debt_market_value = borrowed_amount as f64 / 10f64.powi(debt_decimals as i32) * debt_price;

    let deposited = scaled_to_f64(obligation.deposited_value());
    let actual_debt = scaled_to_f64(obligation.borrowed_assets_market_value());
    let actual_ltv = if deposited > 0.0 {
        safe_div(actual_debt, deposited)
    } else {
        1.0
    };

    let insolvency_threshold = lending_market.insolvency_risk_unhealthy_ltv_pct as f64 / 100.0;
    let close_factor_pct = if actual_ltv >= insolvency_threshold {
        100u8
    } else {
        lending_market.liquidation_max_debt_close_factor_pct
    };

    let mut max_repay = (borrowed_amount as f64 * close_factor_pct as f64 / 100.0) as u64;

    // Dust threshold: below min_full_liquidation_value_threshold, allow full liquidation.
    let min_full_liq = lending_market.min_full_liquidation_value_threshold as f64;
    if debt_market_value < min_full_liq {
        max_repay = borrowed_amount;
    }

    // Cap by max liquidatable debt market value at once.
    let max_value_at_once = lending_market.max_liquidatable_debt_market_value_at_once as f64;
    if safe_div(max_repay as f64, borrowed_amount as f64) * debt_market_value > max_value_at_once {
        let ratio = safe_div(max_value_at_once, debt_market_value);
        max_repay = (borrowed_amount as f64 * ratio) as u64;
    }

    max_repay
}

// ---------------------------------------------------------------------------
// Liquidation bonus
// ---------------------------------------------------------------------------

/// Calculate the dynamic liquidation bonus in basis points.
///
/// Three-step formula from Kamino docs:
/// 1. bonus = max(minBonus, currentLTV - liquidationThreshold)
/// 2. bonus = min(bonus, maxBonus)
/// 3. bonus = min(bonus, 1.0 - actualLTV)  (solvency cap)
pub fn calculate_liquidation_bonus(obligation: &Obligation, withdraw_reserve: &Reserve) -> f64 {
    let deposited = scaled_to_f64(obligation.deposited_value());
    if deposited <= 0.0 {
        return 0.0;
    }

    let adjusted_debt = scaled_to_f64(obligation.borrow_factor_adjusted_debt_value());
    let actual_debt = scaled_to_f64(obligation.borrowed_assets_market_value());
    let current_ltv = safe_div(adjusted_debt, deposited);
    let actual_ltv = safe_div(actual_debt, deposited);

    let threshold = withdraw_reserve.liquidation_threshold_pct() as f64 / 100.0;
    let min_bonus_bps = withdraw_reserve.config.min_liquidation_bonus_bps as f64;
    let max_bonus_bps = withdraw_reserve.config.max_liquidation_bonus_bps as f64;

    // Near-insolvency: use bad debt bonus.
    if actual_ltv >= 0.99 {
        let bad_debt_bps = withdraw_reserve.config.bad_debt_liquidation_bonus_bps as f64;
        let headroom_bps = (1.0 - actual_ltv) * FULL_BPS;
        return bad_debt_bps.min(headroom_bps).max(0.0);
    }

    let ltv_excess_bps = (current_ltv - threshold) * FULL_BPS;
    let bonus = min_bonus_bps.max(ltv_excess_bps); // Step 1: floor
    let bonus = bonus.min(max_bonus_bps); // Step 2: cap
    let headroom_bps = (1.0 - actual_ltv) * FULL_BPS;
    bonus.min(headroom_bps).max(0.0) // Step 3: solvency cap
}

// ---------------------------------------------------------------------------
// Profit estimation
// ---------------------------------------------------------------------------

/// Estimate net profit in USD for a liquidation.
///
/// net = gross_reward - protocol_fee - swap_cost - gas
pub fn estimate_net_profit(
    repay_usd: f64,
    bonus_bps: f64,
    protocol_fee_pct: u8,
    swap_slippage_bps: u16,
) -> f64 {
    let gross_reward = repay_usd * bonus_bps / FULL_BPS;
    let protocol_fee = gross_reward * protocol_fee_pct as f64 / 100.0;
    let swap_cost = repay_usd * swap_slippage_bps as f64 / FULL_BPS;
    gross_reward - protocol_fee - swap_cost - GAS_COST_USD
}

// ---------------------------------------------------------------------------
// Full scoring pipeline
// ---------------------------------------------------------------------------

/// Analyze obligations and return ranked opportunities within a budget.
///
/// Scores each obligation against the given base token and budget,
/// returning profitable opportunities sorted by estimated profit descending.
pub fn process_obligations(
    obligations: &[(Pubkey, &Obligation)],
    reserves: &[(Pubkey, &Reserve)],
    lending_market: &LendingMarket,
    base_token: &Pubkey,
    budget_usd: f64,
    swap_slippage_bps: u16,
) -> Vec<ScoredEntry> {
    let mut results = Vec::new();

    for (obl_pubkey, obligation) in obligations {
        if !filter_obligation_fresh(obligation, reserves, DEFAULT_MIN_DEBT_USD) {
            continue;
        }

        let (withdraw_pk, withdraw_reserve, repay_pk, repay_reserve) =
            match select_pair(obligation, reserves) {
                Some(p) => p,
                None => continue,
            };

        let borrow_position = match find_borrow_position(obligation, &repay_pk) {
            Some(b) => b,
            None => continue,
        };

        let debt_price = scaled_to_f64(u128::from(repay_reserve.liquidity.market_price_sf));
        let debt_decimals = repay_reserve.liquidity.mint_decimals;
        let max_repay = max_liquidatable_amount(
            obligation,
            lending_market,
            borrow_position,
            debt_price,
            debt_decimals,
        );
        if max_repay == 0 {
            continue;
        }

        let bonus_bps = calculate_liquidation_bonus(obligation, withdraw_reserve);
        if bonus_bps <= 0.0 {
            continue;
        }

        if debt_price <= 0.0 {
            continue;
        }
        let max_repay_usd = max_repay as f64 / 10f64.powi(debt_decimals as i32) * debt_price;

        let repay_usd = max_repay_usd.min(budget_usd);
        let repay_amount = if repay_usd < max_repay_usd {
            (safe_div(repay_usd, debt_price) * 10f64.powi(debt_decimals as i32)) as u64
        } else {
            max_repay
        };

        let needs_swap = repay_reserve.liquidity.mint_pubkey != *base_token;
        let profit = estimate_net_profit(
            repay_usd,
            bonus_bps,
            repay_reserve.config.protocol_liquidation_fee_pct,
            if needs_swap { swap_slippage_bps } else { 0 },
        );

        if profit <= 0.0 {
            continue;
        }

        results.push(ScoredEntry {
            obligation: *obl_pubkey,
            withdraw_reserve: withdraw_pk,
            repay_reserve: repay_pk,
            withdraw_mint: withdraw_reserve.liquidity.mint_pubkey,
            repay_mint: repay_reserve.liquidity.mint_pubkey,
            repay_amount,
            estimated_profit_usd: profit,
        });
    }

    results.sort_by(|a, b| {
        b.estimated_profit_usd
            .partial_cmp(&a.estimated_profit_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    results
}
