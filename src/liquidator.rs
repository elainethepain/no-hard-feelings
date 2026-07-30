use std::collections::HashMap;

use anyhow::Result;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_request::TokenAccountsFilter;
use solana_instruction::Instruction;
use solana_pubkey::Pubkey;
use tracing::{debug, info};

use crate::consts::{TOKEN_2022_PROGRAM_ID, TOKEN_PROGRAM_ID, WSOL_MINT};
use crate::math::scaled_to_f64;
use klend_interface::state::Reserve;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A single token holding in the liquidator's wallet.
#[derive(Debug, Clone)]
pub struct Holding {
    pub mint: Pubkey,
    pub balance: u64,
    pub decimals: u8,
    pub usd_value: f64,
}

/// All token holdings for the liquidator wallet.
#[derive(Debug, Clone)]
pub struct Holdings {
    pub holdings: Vec<Holding>,
}

impl Holdings {
    pub fn holding_of(&self, mint: &Pubkey) -> Option<&Holding> {
        self.holdings.iter().find(|h| h.mint == *mint)
    }

    pub fn total_usd(&self) -> f64 {
        self.holdings.iter().map(|h| h.usd_value).sum()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Holding> {
        self.holdings.iter()
    }
}

// ---------------------------------------------------------------------------
// Wallet scanning
// ---------------------------------------------------------------------------

/// Scan the wallet for all token holdings: native SOL + SPL + Token-2022.
pub async fn scan_wallet(rpc: &RpcClient, owner: &Pubkey) -> Result<Holdings> {
    let mut holdings = Vec::new();

    // Native SOL balance.
    let sol_balance = rpc.get_balance(owner).await?;
    holdings.push(Holding {
        mint: WSOL_MINT,
        balance: sol_balance,
        decimals: 9,
        usd_value: 0.0,
    });

    // SPL Token + Token-2022 accounts (fetched in parallel, either can fail independently).
    let (spl_result, t22_result) = tokio::join!(
        rpc.get_token_accounts_by_owner(owner, TokenAccountsFilter::ProgramId(TOKEN_PROGRAM_ID)),
        rpc.get_token_accounts_by_owner(
            owner,
            TokenAccountsFilter::ProgramId(TOKEN_2022_PROGRAM_ID)
        ),
    );
    let mut token_accounts = spl_result.unwrap_or_default();
    if let Ok(t22) = t22_result {
        token_accounts.extend(t22);
    }

    for account in &token_accounts {
        let data = &account.account.data;
        if let solana_account_decoder_client_types::UiAccountData::Json(parsed) = data {
            if let Some(info) = parsed.parsed.get("info") {
                let mint = info
                    .get("mint")
                    .and_then(|m| m.as_str())
                    .and_then(|m| m.parse::<Pubkey>().ok());
                let amount = info
                    .get("tokenAmount")
                    .and_then(|ta| ta.get("amount"))
                    .and_then(|a| a.as_str())
                    .and_then(|a| a.parse::<u64>().ok());
                let decimals = info
                    .get("tokenAmount")
                    .and_then(|ta| ta.get("decimals"))
                    .and_then(|d| d.as_u64())
                    .unwrap_or(0) as u8;

                if let (Some(mint), Some(amount)) = (mint, amount) {
                    if amount > 0 {
                        holdings.push(Holding {
                            mint,
                            balance: amount,
                            decimals,
                            usd_value: 0.0,
                        });
                    }
                }
            }
        }
    }

    for h in &holdings {
        debug!(mint = %h.mint, balance = h.balance, "Found holding");
    }

    Ok(Holdings { holdings })
}

// ---------------------------------------------------------------------------
// Pricing
// ---------------------------------------------------------------------------

/// Price holdings using on-chain reserve oracle data.
pub fn price_holdings(holdings: &mut Holdings, reserves: &[(Pubkey, Reserve)]) {
    for holding in holdings.holdings.iter_mut() {
        if let Some((_, reserve)) = reserves
            .iter()
            .find(|(_, r)| r.liquidity.mint_pubkey == holding.mint)
        {
            let price = scaled_to_f64(u128::from(reserve.liquidity.market_price_sf));
            let ui_balance = holding.balance as f64 / 10f64.powi(holding.decimals as i32);
            holding.usd_value = ui_balance * price;
        }
    }
}

// ---------------------------------------------------------------------------
// Token program cache
// ---------------------------------------------------------------------------

/// Cache of mint → token program mappings. Built once per cycle from reserves,
/// avoiding repeated RPC calls to detect Token vs Token-2022 per candidate.
pub fn build_token_program_cache(reserves: &[(Pubkey, Reserve)]) -> HashMap<Pubkey, Pubkey> {
    let mut cache = HashMap::new();
    for (_, reserve) in reserves {
        // Liquidity mint → its token program (stored in reserve).
        cache.insert(
            reserve.liquidity.mint_pubkey,
            reserve.liquidity.token_program,
        );
        // Collateral (cToken) mints use the standard Token program.
        // kLend always mints cTokens with the standard program.
        cache.insert(reserve.collateral.mint_pubkey, TOKEN_PROGRAM_ID);
    }
    cache
}

/// Look up the token program for a mint, falling back to RPC if not cached.
pub async fn get_token_program(
    rpc: &RpcClient,
    cache: &HashMap<Pubkey, Pubkey>,
    mint: &Pubkey,
) -> Pubkey {
    if let Some(program) = cache.get(mint) {
        return *program;
    }
    crate::client::detect_token_program(rpc, mint).await
}

// ---------------------------------------------------------------------------
// WSOL unwrapping
// ---------------------------------------------------------------------------

/// Build instructions to unwrap WSOL: close the WSOL ATA (converts to native
/// SOL) and immediately recreate it so it exists for future operations.
pub fn build_unwrap_wsol_ixs(owner: &Pubkey) -> Vec<Instruction> {
    let wsol_ata = spl_associated_token_account::get_associated_token_address_with_program_id(
        owner,
        &WSOL_MINT,
        &TOKEN_PROGRAM_ID,
    );

    vec![
        spl_token::instruction::close_account(&TOKEN_PROGRAM_ID, &wsol_ata, owner, owner, &[])
            .expect("close_account instruction"),
        spl_associated_token_account::instruction::create_associated_token_account(
            owner,
            owner,
            &WSOL_MINT,
            &TOKEN_PROGRAM_ID,
        ),
    ]
}

// ---------------------------------------------------------------------------
// Rebalancing
// ---------------------------------------------------------------------------

/// Rebalance parameters.
pub struct RebalanceConfig {
    /// Token to hold as the base currency.
    pub base_token: Pubkey,
    /// Minimum native SOL to keep for gas (in SOL, not lamports).
    pub min_sol_balance: f64,
    /// Don't swap holdings worth less than this (USD).
    pub dust_threshold_usd: f64,
    /// Swap slippage in basis points.
    pub slippage_bps: u16,
}

/// Determine which swaps are needed to rebalance the wallet.
/// Returns a list of (from_mint, amount_in_native_units) to swap into the base token.
pub fn plan_rebalance(holdings: &Holdings, config: &RebalanceConfig) -> Vec<(Pubkey, u64)> {
    let mut swaps = Vec::new();

    for holding in &holdings.holdings {
        // Skip the base token — that's what we're rebalancing into.
        if holding.mint == config.base_token {
            continue;
        }

        // Skip native SOL tracking entry (WSOL mint represents native SOL in our holdings).
        // SOL is handled separately via min_sol_balance.
        if holding.mint == WSOL_MINT && config.base_token != WSOL_MINT {
            continue;
        }

        // Skip dust.
        if holding.usd_value < config.dust_threshold_usd {
            continue;
        }

        if holding.balance > 0 {
            swaps.push((holding.mint, holding.balance));
        }
    }

    swaps
}

/// Check if SOL balance is below the minimum and needs a top-up.
/// Returns the amount of base token (in native units) to swap into SOL.
pub fn sol_topup_needed(
    holdings: &Holdings,
    reserves: &[(Pubkey, Reserve)],
    config: &RebalanceConfig,
) -> Option<u64> {
    let sol_holding = holdings.holding_of(&WSOL_MINT)?;
    let sol_balance_ui = sol_holding.balance as f64 / 1e9;

    if sol_balance_ui >= config.min_sol_balance {
        return None;
    }

    // Need to top up to 2x min so we don't trigger every cycle.
    let target = config.min_sol_balance * 2.0;
    let missing_sol = target - sol_balance_ui;

    let sol_price = reserves
        .iter()
        .find(|(_, r)| r.liquidity.mint_pubkey == WSOL_MINT)
        .map(|(_, r)| scaled_to_f64(u128::from(r.liquidity.market_price_sf)))
        .unwrap_or(100.0);

    let base_reserve = reserves
        .iter()
        .find(|(_, r)| r.liquidity.mint_pubkey == config.base_token);
    let base_price = base_reserve
        .map(|(_, r)| scaled_to_f64(u128::from(r.liquidity.market_price_sf)))
        .unwrap_or(1.0);
    let base_decimals = base_reserve
        .map(|(_, r)| r.liquidity.mint_decimals)
        .unwrap_or(6);

    let base_amount_needed =
        missing_sol * sol_price / base_price * (1.0 + config.slippage_bps as f64 / 10_000.0);
    let native_amount = (base_amount_needed * 10f64.powi(base_decimals as i32)) as u64;

    info!(
        sol_balance = format!("{sol_balance_ui:.4}"),
        target = format!("{target:.4}"),
        missing = format!("{missing_sol:.4}"),
        base_swap = native_amount,
        "SOL top-up needed"
    );

    Some(native_amount)
}
