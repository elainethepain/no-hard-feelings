mod client;
mod config;
mod consts;
mod instructions;
mod kswap;
mod liquidator;
mod lookup_table;
mod math;
mod model;
mod oracle;

use std::{
    collections::HashSet,
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::Result;
use clap::{Parser, Subcommand};
use solana_account::ReadableAccount;
use solana_pubkey::Pubkey;
use solana_sdk::{
    address_lookup_table::AddressLookupTableAccount,
    message::{v0, Message, VersionedMessage},
    transaction::{Transaction, VersionedTransaction},
};
use tracing::{debug, error, info};

use crate::config::BotConfig;

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(name = "nhf", about = "kLend liquidation bot")]
struct Cli {
    /// RPC endpoint URL
    #[arg(long, env = "RPC_URL")]
    rpc_url: String,

    /// Path to liquidator keypair
    #[arg(long, env = "LIQUIDATOR_KEYPAIR")]
    keypair: Option<PathBuf>,

    /// Lending market addresses (omit to discover all on-chain)
    #[arg(long, env = "MARKETS", value_delimiter = ',')]
    markets: Option<Vec<Pubkey>>,

    /// Priority fee in micro-lamports per compute unit
    #[arg(long, env = "PRIORITY_FEE", default_value = "1000")]
    priority_fee: u64,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Scan for liquidatable obligations and save to file
    Scan {
        /// Minimum debt threshold in USD
        #[arg(long, default_value = "5.0")]
        min_debt_usd: f64,

        /// Output file path
        #[arg(long, default_value = "scan_results.json")]
        output: PathBuf,
    },

    /// Liquidate a specific obligation
    Liquidate {
        /// Obligation address
        obligation: Pubkey,

        /// Withdraw (collateral) reserve address
        withdraw_reserve: Pubkey,

        /// Repay (debt) reserve address
        repay_reserve: Pubkey,

        /// Actually send the transaction
        #[arg(long)]
        send: bool,
    },

    /// Budget-aware auto-select and execute best opportunity
    Execute {
        /// Budget in USD
        #[arg(long, default_value = "5.0")]
        budget: f64,

        /// Actually send transactions
        #[arg(long)]
        send: bool,

        /// Maximum opportunities to try
        #[arg(long, default_value = "10")]
        max_attempts: usize,
    },

    /// Continuous scan + liquidate loop
    Crank {
        /// Budget per liquidation in USD
        #[arg(long, default_value = "5.0")]
        budget: f64,

        /// Sleep between cycles (seconds)
        #[arg(long, default_value = "10")]
        interval: u64,

        /// Minimum profit to execute (USD)
        #[arg(long, default_value = "0.01")]
        min_profit: f64,

        /// Maximum opportunities to try per cycle
        #[arg(long, default_value = "10")]
        max_attempts: usize,

        /// Shut down after this many consecutive failed cycles
        #[arg(long, default_value = "50")]
        max_failures: u32,
    },

    /// Swap tokens via kswap
    Swap {
        /// Input token mint
        from: Pubkey,

        /// Output token mint
        to: Pubkey,

        /// Amount in native units
        amount: u64,

        /// Slippage in basis points
        #[arg(long, default_value = "50")]
        slippage_bps: u16,
    },

    /// Rebalance wallet: convert all non-base tokens to base currency,
    /// maintain minimum SOL balance, unwrap WSOL
    Rebalance {
        /// Token to hold as base currency (default: USDC)
        #[arg(
            long,
            env = "BASE_TOKEN",
            default_value = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
        )]
        base_token: Pubkey,

        /// Minimum SOL to keep for gas
        #[arg(long, default_value = "0.05")]
        min_sol: f64,

        /// Don't swap tokens worth less than this (USD)
        #[arg(long, default_value = "5.0")]
        dust_threshold: f64,

        /// Swap slippage in basis points
        #[arg(long, default_value = "50")]
        slippage_bps: u16,
    },
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    if PathBuf::from(".env").exists() {
        dotenvy::dotenv().ok();
    }

    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("nhf=info".parse().unwrap()),
        )
        .init();

    let bot = config::load_config(&cli.rpc_url, cli.keypair.as_ref(), cli.markets.as_ref())?;

    match cli.command {
        Commands::Scan {
            min_debt_usd,
            output,
        } => cmd_scan(&bot, min_debt_usd, &output).await,

        Commands::Liquidate {
            obligation,
            withdraw_reserve,
            repay_reserve,
            send,
        } => {
            cmd_liquidate(
                &bot,
                obligation,
                withdraw_reserve,
                repay_reserve,
                send,
                cli.priority_fee,
            )
            .await
        }

        Commands::Execute {
            budget,
            send,
            max_attempts,
        } => cmd_execute(&bot, budget, send, max_attempts, cli.priority_fee).await,

        Commands::Crank {
            budget,
            interval,
            min_profit,
            max_attempts,
            max_failures,
        } => {
            cmd_crank(
                &bot,
                budget,
                Duration::from_secs(interval),
                min_profit,
                max_attempts,
                cli.priority_fee,
                max_failures,
            )
            .await
        }

        Commands::Swap {
            from,
            to,
            amount,
            slippage_bps,
        } => cmd_swap(&bot, from, to, amount, slippage_bps).await,

        Commands::Rebalance {
            base_token,
            min_sol,
            dust_threshold,
            slippage_bps,
        } => cmd_rebalance(&bot, base_token, min_sol, dust_threshold, slippage_bps).await,
    }
}

// ---------------------------------------------------------------------------
// Resolve markets
// ---------------------------------------------------------------------------

async fn resolve_markets(bot: &BotConfig) -> Result<Vec<Pubkey>> {
    match &bot.markets {
        Some(markets) => Ok(markets.clone()),
        None => {
            let all = client::fetch_all_markets(&bot.rpc).await?;
            let pks: Vec<Pubkey> = all.into_iter().map(|(pk, _)| pk).collect();
            info!(count = pks.len(), "Discovered markets on-chain");
            Ok(pks)
        }
    }
}

// ---------------------------------------------------------------------------
// Scan
// ---------------------------------------------------------------------------

async fn cmd_scan(bot: &BotConfig, min_debt_usd: f64, output: &PathBuf) -> Result<()> {
    let markets = resolve_markets(bot).await?;

    let mut all_results = Vec::new();

    for market in &markets {
        info!(market = %market, "Scanning obligations");

        let mut data = client::fetch_market_and_reserves(&bot.rpc, market).await?;

        // Apply fresh oracle prices so filtering is accurate.
        if let Ok(fresh) = oracle::fetch_fresh_prices(&bot.rpc, &data.reserves).await {
            oracle::apply_fresh_prices(&mut data.reserves, &fresh);
        }

        let obligations = client::fetch_obligations(&bot.rpc, market).await?;
        let reserve_refs: Vec<_> = data.reserves.iter().map(|(pk, r)| (*pk, r)).collect();

        let mut with_debt = 0u64;
        let mut liquidatable = 0u64;

        for (pubkey, obligation) in &obligations {
            if obligation.has_debt == 0 {
                continue;
            }
            with_debt += 1;

            if !math::filter_obligation_fresh(obligation, &reserve_refs, min_debt_usd) {
                continue;
            }
            liquidatable += 1;

            let stats = math::obligation_stats(obligation);
            info!(
                pubkey = %pubkey,
                deposited = format!("${:.2}", stats.deposited_usd),
                actual_debt = format!("${:.2}", stats.actual_debt_usd),
                ltv = format!("{:.1}%", stats.ltv_actual * 100.0),
                "LIQUIDATABLE"
            );

            all_results.push(model::ScanResult {
                pubkey: pubkey.to_string(),
                deposited_usd: stats.deposited_usd,
                adjusted_debt_usd: stats.adjusted_debt_usd,
                actual_debt_usd: stats.actual_debt_usd,
                unhealthy_limit_usd: stats.unhealthy_limit_usd,
            });
        }

        info!(
            market = %market,
            with_debt,
            liquidatable,
            actionable = all_results.len(),
            "Market scan complete"
        );
    }

    all_results.sort_by(|a, b| {
        b.actual_debt_usd
            .partial_cmp(&a.actual_debt_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let json = serde_json::to_string_pretty(&all_results)?;
    tokio::fs::write(output, &json).await?;
    info!(count = all_results.len(), path = %output.display(), "Saved scan results");

    Ok(())
}

// ---------------------------------------------------------------------------
// Liquidate (single obligation)
// ---------------------------------------------------------------------------

async fn cmd_liquidate(
    bot: &BotConfig,
    obligation_pk: Pubkey,
    withdraw_reserve_pk: Pubkey,
    repay_reserve_pk: Pubkey,
    send: bool,
    priority_fee: u64,
) -> Result<()> {
    info!(
        obligation = %obligation_pk,
        mode = if send { "LIVE" } else { "DRY RUN" },
        "Liquidating"
    );

    let obligation_acc = bot.rpc.get_account(&obligation_pk).await?;
    let obligation = klend_interface::state::from_account_data::<klend_interface::state::Obligation>(
        obligation_acc.data(),
    )?;

    let mut reserves = client::fetch_reserves_for_obligation(&bot.rpc, obligation).await?;

    // Apply fresh oracle prices before checking liquidatability.
    if let Ok(fresh) = oracle::fetch_fresh_prices(&bot.rpc, &reserves).await {
        oracle::apply_fresh_prices(&mut reserves, &fresh);
    }

    let reserve_refs: Vec<_> = reserves.iter().map(|(pk, r)| (*pk, r)).collect();
    if !math::filter_obligation_fresh(obligation, &reserve_refs, 0.0) {
        error!("Obligation is not liquidatable with current oracle prices");
        return Ok(());
    }

    let repay_reserve = reserves
        .iter()
        .find(|(pk, _)| *pk == repay_reserve_pk)
        .map(|(_, r)| r)
        .ok_or_else(|| anyhow::anyhow!("Repay reserve not found"))?;

    let withdraw_reserve = reserves
        .iter()
        .find(|(pk, _)| *pk == withdraw_reserve_pk)
        .map(|(_, r)| r)
        .ok_or_else(|| anyhow::anyhow!("Withdraw reserve not found"))?;

    // Calculate repay amount.
    let borrow_position = obligation
        .borrows
        .iter()
        .find(|b| b.borrow_reserve == repay_reserve_pk)
        .ok_or_else(|| anyhow::anyhow!("Borrow position not found"))?;

    let market_acc = bot.rpc.get_account(&obligation.lending_market).await?;
    let lending_market = klend_interface::state::from_account_data::<
        klend_interface::state::LendingMarket,
    >(market_acc.data())?;

    let debt_price = math::scaled_to_f64(u128::from(repay_reserve.liquidity.market_price_sf));
    let repay_amount = math::max_liquidatable_amount(
        obligation,
        lending_market,
        borrow_position,
        debt_price,
        repay_reserve.liquidity.mint_decimals,
    );
    let repay_usd =
        repay_amount as f64 / 10f64.powi(repay_reserve.liquidity.mint_decimals as i32) * debt_price;

    info!(
        repay_amount,
        repay_usd = format!("${repay_usd:.2}"),
        "Repay"
    );

    let all_reserve_infos: Vec<klend_interface::ReserveInfo> = reserves
        .iter()
        .map(|(pk, r)| instructions::reserve_info_with_null_check(*pk, r))
        .collect();

    // Resolve LUTs for this obligation's reserves.
    let tp_cache = liquidator::build_token_program_cache(&reserves);

    let attempt = LiquidationAttempt {
        obligation_pk,
        obligation,
        repay_reserve_pk,
        repay_reserve,
        withdraw_reserve_pk,
        withdraw_reserve,
        all_reserve_infos,
        repay_amount,
    };

    // For single-shot liquidate, resolve LUTs from instruction keys.
    // (cmd_execute/crank use per-market cached LUTs instead.)
    let budget_ixs = instructions::build_compute_budget_ixs(400_000, priority_fee);
    let lut_keys = lookup_table::collect_instruction_keys(&budget_ixs);
    let luts = lookup_table::resolve_luts(&bot.http, &bot.rpc, &lut_keys, &[bot.owner])
        .await
        .unwrap_or_default();

    match execute_liquidation(bot, &attempt, &luts, &tp_cache, priority_fee, send).await? {
        Some(sig) => info!(signature = %sig, "Liquidation executed"),
        None if send => error!("Simulation failed"),
        None => info!("Dry run complete — pass --send to execute"),
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Execute (budget-aware)
// ---------------------------------------------------------------------------

async fn cmd_execute(
    bot: &BotConfig,
    budget: f64,
    send: bool,
    max_attempts: usize,
    priority_fee: u64,
) -> Result<()> {
    info!(
        budget = format!("${budget:.2}"),
        mode = if send { "LIVE" } else { "DRY RUN" },
        "Execute"
    );

    let markets = resolve_markets(bot).await?;

    for market in &markets {
        let mut data = client::fetch_market_and_reserves(&bot.rpc, market).await?;

        // Fetch fresh oracle prices before scoring.
        if let Ok(fresh) = oracle::fetch_fresh_prices(&bot.rpc, &data.reserves).await {
            oracle::apply_fresh_prices(&mut data.reserves, &fresh);
        }

        let obligations = client::fetch_obligations(&bot.rpc, market).await?;

        let mut holdings = liquidator::scan_wallet(&bot.rpc, &bot.owner).await?;
        liquidator::price_holdings(&mut holdings, &data.reserves);

        let effective_budget = budget.min(holdings.total_usd());
        if effective_budget < 0.01 {
            info!("Insufficient wallet balance");
            continue;
        }

        let market_luts = resolve_luts_for_market(bot, &data).await;

        let opportunities =
            score_all_opportunities(&obligations, &data, &holdings, effective_budget);

        if opportunities.is_empty() {
            info!(market = %market, "No profitable opportunities");
            continue;
        }

        info!(count = opportunities.len(), "Found opportunities");
        for (i, (opp, _base, needs_swap)) in opportunities.iter().take(5).enumerate() {
            info!(
                rank = i + 1,
                obligation = %opp.obligation,
                repay_mint = %opp.repay_mint,
                withdraw_mint = %opp.withdraw_mint,
                needs_swap,
                profit = format!("${:.4}", opp.estimated_profit_usd),
                "Opportunity"
            );
        }

        let tp_cache = liquidator::build_token_program_cache(&data.reserves);
        if try_opportunities(
            bot,
            &data,
            &obligations,
            &holdings,
            &opportunities,
            max_attempts,
            send,
            priority_fee,
            budget,
            &market_luts,
            &tp_cache,
        )
        .await?
        {
            return Ok(());
        }
    }

    info!("No viable opportunity found");
    Ok(())
}

// ---------------------------------------------------------------------------
// Crank (continuous loop)
// ---------------------------------------------------------------------------

async fn cmd_crank(
    bot: &BotConfig,
    budget: f64,
    interval: Duration,
    min_profit: f64,
    max_attempts: usize,
    priority_fee: u64,
    max_failures: u32,
) -> Result<()> {
    info!(
        budget = format!("${budget:.2}"),
        interval = ?interval,
        min_profit = format!("${min_profit:.4}"),
        max_failures,
        "Starting crank loop (Ctrl+C to stop gracefully)"
    );

    let markets = resolve_markets(bot).await?;
    let mut consecutive_failures: u32 = 0;

    loop {
        // Graceful shutdown on Ctrl+C.
        let start = Instant::now();
        let cycle_result = tokio::select! {
            result = crank_cycle(bot, &markets, budget, min_profit, max_attempts, priority_fee) => result,
            _ = tokio::signal::ctrl_c() => {
                info!("Shutdown signal received, exiting gracefully");
                return Ok(());
            }
        };

        match cycle_result {
            Ok(true) => {
                // At least one liquidation succeeded this cycle.
                consecutive_failures = 0;
            }
            Ok(false) => {
                // No liquidation executed (no opportunities or all simulations failed).
                // This is normal — don't count as a failure.
            }
            Err(e) => {
                consecutive_failures += 1;
                error!(
                    error = %e,
                    consecutive_failures,
                    max_failures,
                    "Crank cycle failed"
                );
                if consecutive_failures >= max_failures {
                    error!(
                        "Circuit breaker triggered — too many consecutive failures. Shutting down."
                    );
                    return Err(anyhow::anyhow!(
                        "Circuit breaker: {consecutive_failures} consecutive failures"
                    ));
                }
            }
        }

        let elapsed = start.elapsed();
        let sleep_time = interval.saturating_sub(elapsed);
        info!(elapsed = ?elapsed, sleep = ?sleep_time, consecutive_failures, "Cycle complete");
        tokio::time::sleep(sleep_time).await;
    }
}

/// Run a single crank cycle across all markets. Returns true if any liquidation succeeded.
async fn crank_cycle(
    bot: &BotConfig,
    markets: &[Pubkey],
    budget: f64,
    min_profit: f64,
    max_attempts: usize,
    priority_fee: u64,
) -> Result<bool> {
    let mut any_success = false;

    for market in markets {
        info!(market = %market, "Crank cycle");

        let mut data = client::fetch_market_and_reserves(&bot.rpc, market).await?;

        // Fetch fresh oracle prices and apply to reserves before scoring.
        match oracle::fetch_fresh_prices(&bot.rpc, &data.reserves).await {
            Ok(fresh) => oracle::apply_fresh_prices(&mut data.reserves, &fresh),
            Err(e) => debug!(error = %e, "Oracle fetch failed, using on-chain prices"),
        }

        let obligations = client::fetch_obligations(&bot.rpc, market).await?;

        let mut holdings = liquidator::scan_wallet(&bot.rpc, &bot.owner).await?;
        liquidator::price_holdings(&mut holdings, &data.reserves);

        let effective_budget = budget.min(holdings.total_usd());
        if effective_budget < 0.01 {
            debug!("Insufficient balance, skipping");
            continue;
        }

        let market_luts = resolve_luts_for_market(bot, &data).await;

        let mut opportunities =
            score_all_opportunities(&obligations, &data, &holdings, effective_budget);

        opportunities.retain(|(opp, _, _)| opp.estimated_profit_usd >= min_profit);

        if opportunities.is_empty() {
            debug!(market = %market, "No opportunities above min profit");
            continue;
        }

        info!(count = opportunities.len(), market = %market, "Found opportunities");

        let tp_cache = liquidator::build_token_program_cache(&data.reserves);
        if try_opportunities(
            bot,
            &data,
            &obligations,
            &holdings,
            &opportunities,
            max_attempts,
            true,
            priority_fee,
            budget,
            &market_luts,
            &tp_cache,
        )
        .await?
        {
            any_success = true;
        }
    }

    Ok(any_success)
}

// ---------------------------------------------------------------------------
// Swap
// ---------------------------------------------------------------------------

async fn cmd_swap(
    bot: &BotConfig,
    from: Pubkey,
    to: Pubkey,
    amount: u64,
    slippage_bps: u16,
) -> Result<()> {
    info!(from = %from, to = %to, amount, slippage_bps, "Swapping");

    let quote =
        kswap::get_swap_quote(&bot.http, &from, &to, amount, &bot.owner, slippage_bps).await?;
    info!(
        expected_out = quote.expected_amount_out,
        min_out = quote.min_amount_out,
        router = quote.router_type,
        "Quote received"
    );

    let sig = kswap::send_swap_transaction(&bot.rpc, &bot.signer, &quote).await?;
    info!(signature = %sig, "Swap executed");

    Ok(())
}

// ---------------------------------------------------------------------------
// Rebalance
// ---------------------------------------------------------------------------

async fn cmd_rebalance(
    bot: &BotConfig,
    base_token: Pubkey,
    min_sol: f64,
    dust_threshold: f64,
    slippage_bps: u16,
) -> Result<()> {
    info!(base = %base_token, min_sol, dust = format!("${dust_threshold:.2}"), "Rebalancing");

    let markets = resolve_markets(bot).await?;

    // Fetch all reserves across all markets for pricing.
    let mut all_reserves = Vec::new();
    for market in &markets {
        if let Ok(data) = client::fetch_market_and_reserves(&bot.rpc, market).await {
            all_reserves.extend(data.reserves);
        }
    }

    // Apply fresh prices.
    if let Ok(fresh) = oracle::fetch_fresh_prices(&bot.rpc, &all_reserves).await {
        oracle::apply_fresh_prices(&mut all_reserves, &fresh);
    }

    let mut holdings = liquidator::scan_wallet(&bot.rpc, &bot.owner).await?;
    liquidator::price_holdings(&mut holdings, &all_reserves);

    info!("Wallet holdings:");
    for h in holdings.iter() {
        if h.usd_value > 0.01 {
            info!(
                mint = %h.mint,
                balance = h.balance,
                usd = format!("${:.2}", h.usd_value),
                "  token"
            );
        }
    }

    let config = liquidator::RebalanceConfig {
        base_token,
        min_sol_balance: min_sol,
        dust_threshold_usd: dust_threshold,
        slippage_bps,
    };

    // Step 1: Check if SOL needs a top-up from base token.
    if let Some(topup_amount) = liquidator::sol_topup_needed(&holdings, &all_reserves, &config) {
        info!(amount = topup_amount, "Topping up SOL from base token");
        match kswap::get_swap_quote(
            &bot.http,
            &base_token,
            &consts::WSOL_MINT,
            topup_amount,
            &bot.owner,
            slippage_bps,
        )
        .await
        {
            Ok(quote) => match kswap::send_swap_transaction(&bot.rpc, &bot.signer, &quote).await {
                Ok(sig) => info!(signature = %sig, "SOL top-up swap executed"),
                Err(e) => error!(error = %e, "SOL top-up swap failed"),
            },
            Err(e) => error!(error = %e, "SOL top-up quote failed"),
        }

        // Unwrap WSOL after swap.
        let unwrap_ixs = liquidator::build_unwrap_wsol_ixs(&bot.owner);
        match send_with_retry(bot, &unwrap_ixs, &[], 3).await {
            Ok(sig) => info!(signature = %sig, "WSOL unwrapped"),
            Err(e) => debug!(error = %e, "WSOL unwrap failed (may not have WSOL ATA)"),
        }

        tokio::time::sleep(Duration::from_secs(1)).await;

        // Re-scan holdings.
        holdings = liquidator::scan_wallet(&bot.rpc, &bot.owner).await?;
        liquidator::price_holdings(&mut holdings, &all_reserves);
    }

    // Step 2: Unwrap any existing WSOL (if base token is not WSOL).
    if base_token != consts::WSOL_MINT {
        let wsol_holding = holdings.holding_of(&consts::WSOL_MINT);
        if let Some(h) = wsol_holding {
            if h.usd_value > 1.0 {
                // This is the SPL WSOL token account, not native SOL.
                // Check if there's actually a WSOL token account with balance.
                let wsol_ata =
                    spl_associated_token_account::get_associated_token_address_with_program_id(
                        &bot.owner,
                        &consts::WSOL_MINT,
                        &consts::TOKEN_PROGRAM_ID,
                    );
                if let Ok(acc) = bot.rpc.get_account(&wsol_ata).await {
                    if acc.data.len() >= 72 {
                        let token_balance =
                            u64::from_le_bytes(acc.data[64..72].try_into().unwrap());
                        if token_balance > 0 {
                            info!(balance = token_balance, "Unwrapping WSOL");
                            let unwrap_ixs = liquidator::build_unwrap_wsol_ixs(&bot.owner);
                            match send_with_retry(bot, &unwrap_ixs, &[], 3).await {
                                Ok(sig) => info!(signature = %sig, "WSOL unwrapped"),
                                Err(e) => debug!(error = %e, "WSOL unwrap failed"),
                            }
                        }
                    }
                }
            }
        }
    }

    // Step 3: Swap all non-base tokens into base token.
    let swaps = liquidator::plan_rebalance(&holdings, &config);
    for (from_mint, amount) in &swaps {
        info!(from = %from_mint, amount, "Swapping to base token");
        match kswap::get_swap_quote(
            &bot.http,
            from_mint,
            &base_token,
            *amount,
            &bot.owner,
            slippage_bps,
        )
        .await
        {
            Ok(quote) => {
                info!(expected_out = quote.expected_amount_out, "Quote");
                match kswap::send_swap_transaction(&bot.rpc, &bot.signer, &quote).await {
                    Ok(sig) => info!(signature = %sig, "Swap executed"),
                    Err(e) => info!(error = %e, "Swap failed"),
                }
            }
            Err(e) => info!(error = %e, "Quote failed, skipping"),
        }
    }

    if swaps.is_empty() {
        info!("Nothing to rebalance");
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Send a versioned transaction with retry on BlockhashNotFound.
async fn send_with_retry(
    bot: &BotConfig,
    instructions: &[solana_instruction::Instruction],
    luts: &[AddressLookupTableAccount],
    max_retries: u8,
) -> Result<solana_sdk::signature::Signature> {
    for attempt in 1..=max_retries {
        let tx = build_versioned_tx(bot, instructions, luts).await?;
        match bot.rpc.send_and_confirm_transaction(&tx).await {
            Ok(sig) => return Ok(sig),
            Err(e) => {
                let err_str = e.to_string();
                if err_str.contains("BlockhashNotFound") && attempt < max_retries {
                    debug!(attempt, "BlockhashNotFound, retrying");
                    continue;
                }
                return Err(e.into());
            }
        }
    }
    anyhow::bail!("Max retries reached")
}

/// Wait for a token balance to appear after a swap, polling every 500ms up to 10 seconds.
async fn wait_for_token_balance(
    rpc: &solana_client::nonblocking::rpc_client::RpcClient,
    owner: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
) {
    let ata = spl_associated_token_account::get_associated_token_address_with_program_id(
        owner,
        mint,
        token_program,
    );
    for _ in 0..20 {
        if let Ok(acc) = rpc.get_account(&ata).await {
            if acc.data.len() >= 72 {
                let balance = u64::from_le_bytes(acc.data[64..72].try_into().unwrap_or_default());
                if balance > 0 {
                    debug!(mint = %mint, balance, "Token balance confirmed");
                    return;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    debug!(mint = %mint, "Token balance not confirmed after 10s, proceeding anyway");
}

// ---------------------------------------------------------------------------
// Transaction building helpers
// ---------------------------------------------------------------------------

/// Build a versioned transaction with LUT support. Falls back to legacy if
/// v0 message compilation fails (e.g. no LUTs needed).
async fn build_versioned_tx(
    bot: &BotConfig,
    instructions: &[solana_instruction::Instruction],
    luts: &[AddressLookupTableAccount],
) -> Result<VersionedTransaction> {
    let blockhash = bot.rpc.get_latest_blockhash().await?;

    if luts.is_empty() {
        // Legacy transaction — no LUTs needed.
        let msg = Message::new(instructions, Some(&bot.owner));
        let tx = Transaction::new(&[&bot.signer], msg, blockhash);
        return Ok(VersionedTransaction::from(tx));
    }

    let v0_msg = v0::Message::try_compile(&bot.owner, instructions, luts, blockhash)
        .map_err(|e| anyhow::anyhow!("Failed to compile v0 message: {e}"))?;

    let versioned_msg = VersionedMessage::V0(v0_msg);
    let tx = VersionedTransaction::try_new(versioned_msg, &[&bot.signer])
        .map_err(|e| anyhow::anyhow!("Failed to sign versioned tx: {e}"))?;

    Ok(tx)
}

/// Resolve LUTs once for an entire market's reserves. Called once per cycle,
/// reused for all candidates in that cycle.
async fn resolve_luts_for_market(
    bot: &BotConfig,
    data: &client::MarketData,
) -> Vec<AddressLookupTableAccount> {
    // Collect all addresses that any liquidation in this market might reference:
    // reserve pubkeys, vaults, mints, fee receivers, lending market, authority.
    let mut addresses = Vec::new();
    for (pk, reserve) in &data.reserves {
        addresses.push(*pk);
        addresses.push(reserve.liquidity.mint_pubkey);
        addresses.push(reserve.liquidity.supply_vault);
        addresses.push(reserve.liquidity.fee_vault);
        addresses.push(reserve.liquidity.token_program);
        addresses.push(reserve.collateral.mint_pubkey);
        addresses.push(reserve.collateral.supply_vault);
    }
    addresses.push(data.market_pubkey);
    let (lma, _) = klend_interface::pda::lending_market_authority(
        &klend_interface::KLEND_PROGRAM_ID,
        &data.market_pubkey,
    );
    addresses.push(lma);
    addresses.push(klend_interface::KLEND_PROGRAM_ID);

    // Dedup.
    addresses.sort();
    addresses.dedup();

    let user_accounts = vec![bot.owner];
    match lookup_table::resolve_luts(&bot.http, &bot.rpc, &addresses, &user_accounts).await {
        Ok(luts) => {
            info!(count = luts.len(), "Cached LUTs for market");
            luts
        }
        Err(e) => {
            debug!(error = %e, "LUT resolution failed, using legacy tx");
            vec![]
        }
    }
}

// ---------------------------------------------------------------------------

/// Score all obligations across all wallet holdings as potential base tokens.
/// Returns deduplicated opportunities sorted by profit.
fn score_all_opportunities(
    obligations: &[(Pubkey, klend_interface::state::Obligation)],
    data: &client::MarketData,
    holdings: &liquidator::Holdings,
    effective_budget: f64,
) -> Vec<(model::ScoredEntry, Pubkey, bool)> {
    let obligation_refs: Vec<_> = obligations.iter().map(|(pk, o)| (*pk, o)).collect();
    let reserve_refs: Vec<_> = data.reserves.iter().map(|(pk, r)| (*pk, r)).collect();

    let mut all = Vec::new();
    for holding in holdings.iter() {
        if holding.usd_value < 0.01 {
            continue;
        }
        let opps = math::process_obligations(
            &obligation_refs,
            &reserve_refs,
            &data.lending_market,
            &holding.mint,
            effective_budget.min(holding.usd_value),
            consts::DEFAULT_SWAP_SLIPPAGE_BPS,
        );
        for opp in opps {
            let needs_swap = opp.repay_mint != holding.mint;
            all.push((opp, holding.mint, needs_swap));
        }
    }

    // Sort by profit descending.
    all.sort_by(|a, b| {
        b.0.estimated_profit_usd
            .partial_cmp(&a.0.estimated_profit_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Deduplicate by obligation — keep highest profit.
    let mut seen = HashSet::new();
    all.retain(|(opp, _, _)| seen.insert(opp.obligation));

    all
}

/// Parameters for a single liquidation attempt.
struct LiquidationAttempt<'a> {
    obligation_pk: Pubkey,
    obligation: &'a klend_interface::state::Obligation,
    repay_reserve_pk: Pubkey,
    repay_reserve: &'a klend_interface::state::Reserve,
    withdraw_reserve_pk: Pubkey,
    withdraw_reserve: &'a klend_interface::state::Reserve,
    all_reserve_infos: Vec<klend_interface::ReserveInfo>,
    repay_amount: u64,
}

/// Execute a single liquidation: derive ATAs, build farm + refresh + liquidate
/// instructions, simulate, and optionally send.
///
/// Returns `Ok(Some(signature))` on successful send, `Ok(None)` on dry run
/// or simulation failure, `Err` on unrecoverable error.
async fn execute_liquidation(
    bot: &BotConfig,
    attempt: &LiquidationAttempt<'_>,
    luts: &[AddressLookupTableAccount],
    token_program_cache: &std::collections::HashMap<Pubkey, Pubkey>,
    priority_fee: u64,
    send: bool,
) -> Result<Option<solana_sdk::signature::Signature>> {
    let ctoken_mint = attempt.withdraw_reserve.collateral.mint_pubkey;
    let ctoken_program =
        liquidator::get_token_program(&bot.rpc, token_program_cache, &ctoken_mint).await;

    let user_source_liquidity =
        spl_associated_token_account::get_associated_token_address_with_program_id(
            &bot.owner,
            &attempt.repay_reserve.liquidity.mint_pubkey,
            &attempt.repay_reserve.liquidity.token_program,
        );
    let user_destination_collateral =
        spl_associated_token_account::get_associated_token_address_with_program_id(
            &bot.owner,
            &ctoken_mint,
            &ctoken_program,
        );
    let user_destination_liquidity =
        spl_associated_token_account::get_associated_token_address_with_program_id(
            &bot.owner,
            &attempt.withdraw_reserve.liquidity.mint_pubkey,
            &attempt.withdraw_reserve.liquidity.token_program,
        );

    let farms = instructions::FarmAccounts::from_reserves(
        &attempt.obligation_pk,
        attempt.withdraw_reserve,
        attempt.repay_reserve,
    );
    let (farm_pre_ixs, farm_post_ixs) = instructions::build_farm_ixs(
        &bot.rpc,
        &bot.owner,
        &attempt.obligation_pk,
        &attempt.obligation.owner,
        &attempt.withdraw_reserve_pk,
        attempt.withdraw_reserve,
        &attempt.repay_reserve_pk,
        attempt.repay_reserve,
    )
    .await;

    let mut liq_ixs = farm_pre_ixs;
    liq_ixs.extend(instructions::build_refresh_and_liquidate_ixs(
        bot.owner,
        attempt.repay_reserve_pk,
        attempt.repay_reserve,
        attempt.withdraw_reserve_pk,
        attempt.withdraw_reserve,
        &klend_interface::ObligationInfo::from_obligation(
            attempt.obligation_pk,
            attempt.obligation,
        ),
        &attempt.all_reserve_infos,
        user_source_liquidity,
        user_destination_collateral,
        user_destination_liquidity,
        attempt.repay_amount,
        0,
        0,
        &farms,
    ));
    liq_ixs.extend(farm_post_ixs);

    let ata_ixs = instructions::build_ata_creation_ixs(
        &bot.owner,
        attempt.repay_reserve,
        attempt.withdraw_reserve,
        &ctoken_mint,
        &ctoken_program,
    );

    // Simulate.
    let mut sim_ixs = instructions::build_compute_budget_ixs(400_000, priority_fee);
    sim_ixs.extend(ata_ixs.clone());
    sim_ixs.extend(liq_ixs.clone());

    let sim_tx = build_versioned_tx(bot, &sim_ixs, luts).await?;
    let sim = bot.rpc.simulate_transaction(&sim_tx).await?;
    if let Some(err) = sim.value.err {
        debug!(error = %err, "Simulation failed");
        return Ok(None);
    }
    info!("Simulation passed");

    if !send {
        return Ok(None);
    }

    // Create ATAs.
    send_with_retry(bot, &ata_ixs, &[], 3).await?;

    // Send liquidation.
    let mut send_ixs = instructions::build_compute_budget_ixs(400_000, priority_fee);
    send_ixs.extend(liq_ixs);
    let sig = send_with_retry(bot, &send_ixs, luts, 3).await?;
    Ok(Some(sig))
}

/// Try executing opportunities in order. Returns true if one succeeds.
#[allow(clippy::too_many_arguments)]
async fn try_opportunities(
    bot: &BotConfig,
    data: &client::MarketData,
    obligations: &[(Pubkey, klend_interface::state::Obligation)],
    holdings: &liquidator::Holdings,
    opportunities: &[(model::ScoredEntry, Pubkey, bool)],
    max_attempts: usize,
    send: bool,
    priority_fee: u64,
    budget: f64,
    luts: &[AddressLookupTableAccount],
    token_program_cache: &std::collections::HashMap<Pubkey, Pubkey>,
) -> Result<bool> {
    // Cache swap failures: if kswap has no route for (from, to), don't retry it
    // for every candidate in this cycle.
    let mut failed_swap_pairs: HashSet<(Pubkey, Pubkey)> = HashSet::new();

    for (rank, (candidate, base_mint, needs_swap)) in
        opportunities.iter().enumerate().take(max_attempts)
    {
        debug!(rank = rank + 1, obligation = %candidate.obligation, profit = format!("${:.4}", candidate.estimated_profit_usd), "Trying");

        // Look up from already-fetched data — no re-fetching.
        let repay_reserve = match data
            .reserves
            .iter()
            .find(|(pk, _)| *pk == candidate.repay_reserve)
        {
            Some((_, r)) => r,
            None => continue,
        };
        let withdraw_reserve = match data
            .reserves
            .iter()
            .find(|(pk, _)| *pk == candidate.withdraw_reserve)
        {
            Some((_, r)) => r,
            None => continue,
        };
        let obligation = match obligations
            .iter()
            .find(|(pk, _)| *pk == candidate.obligation)
        {
            Some((_, o)) => o,
            None => continue,
        };

        let all_reserve_infos: Vec<klend_interface::ReserveInfo> = obligation
            .deposits
            .iter()
            .filter(|d| d.deposit_reserve != Pubkey::default())
            .map(|d| d.deposit_reserve)
            .chain(
                obligation
                    .borrows
                    .iter()
                    .filter(|b| b.borrow_reserve != Pubkey::default())
                    .map(|b| b.borrow_reserve),
            )
            .filter_map(|pk| {
                data.reserves
                    .iter()
                    .find(|(rpk, _)| *rpk == pk)
                    .map(|(rpk, r)| instructions::reserve_info_with_null_check(*rpk, r))
            })
            .collect();

        let attempt = LiquidationAttempt {
            obligation_pk: candidate.obligation,
            obligation,
            repay_reserve_pk: candidate.repay_reserve,
            repay_reserve,
            withdraw_reserve_pk: candidate.withdraw_reserve,
            withdraw_reserve,
            all_reserve_infos,
            repay_amount: candidate.repay_amount,
        };

        // Swap into the debt token if needed — before executing the liquidation.
        if send && *needs_swap {
            let swap_pair = (*base_mint, candidate.repay_mint);

            // Skip if we already know this swap pair has no route.
            if failed_swap_pairs.contains(&swap_pair) {
                debug!(from = %base_mint, to = %candidate.repay_mint, "Skipping — swap pair previously failed");
                continue;
            }

            let holding = match holdings.holding_of(base_mint) {
                Some(h) => h,
                None => continue,
            };

            let base_reserve = data
                .reserves
                .iter()
                .find(|(_, r)| r.liquidity.mint_pubkey == *base_mint);
            let base_price = base_reserve
                .map(|(_, r)| math::scaled_to_f64(u128::from(r.liquidity.market_price_sf)))
                .unwrap_or(1.0);
            let base_decimals = base_reserve
                .map(|(_, r)| r.liquidity.mint_decimals)
                .unwrap_or(9);

            let budget_in_base = (budget.min(holding.usd_value) / base_price
                * 10f64.powi(base_decimals as i32)) as u64;
            let swap_amount = budget_in_base.min(holding.balance);

            info!(from = %base_mint, to = %candidate.repay_mint, swap_amount, "Swapping");

            match kswap::get_swap_quote(
                &bot.http,
                base_mint,
                &candidate.repay_mint,
                swap_amount,
                &bot.owner,
                consts::DEFAULT_SWAP_SLIPPAGE_BPS,
            )
            .await
            {
                Ok(quote) => {
                    info!(expected_out = quote.expected_amount_out, "Swap quote");
                    match kswap::send_swap_transaction(&bot.rpc, &bot.signer, &quote).await {
                        Ok(sig) => {
                            info!(signature = %sig, "Swap successful");
                            wait_for_token_balance(
                                &bot.rpc,
                                &bot.owner,
                                &candidate.repay_mint,
                                &repay_reserve.liquidity.token_program,
                            )
                            .await;
                        }
                        Err(e) => {
                            info!(error = %e, "Swap send failed");
                            continue;
                        }
                    }
                }
                Err(e) => {
                    info!(error = %e, "Swap quote failed — caching for this cycle");
                    failed_swap_pairs.insert(swap_pair);
                    continue;
                }
            }
        }

        match execute_liquidation(bot, &attempt, luts, token_program_cache, priority_fee, send)
            .await
        {
            Ok(Some(sig)) => {
                info!(signature = %sig, "Liquidation executed");
                return Ok(true);
            }
            Ok(None) if !send => {
                info!(
                    rank = rank + 1,
                    profit = format!("${:.4}", candidate.estimated_profit_usd),
                    "Dry run — simulation passed"
                );
                return Ok(true);
            }
            Ok(None) => {
                debug!(rank = rank + 1, "Simulation failed, trying next");
                continue;
            }
            Err(e) => {
                info!(rank = rank + 1, error = %e, "Liquidation failed, trying next");
                continue;
            }
        }
    }

    Ok(false)
}
