use std::collections::HashSet;

use anyhow::{Context, Result};
use klend_interface::{
    state::{from_account_data, LendingMarket, Obligation, Reserve, SplDiscriminate},
    KLEND_PROGRAM_ID,
};
use solana_account::ReadableAccount;
use solana_account_decoder_client_types::UiAccountEncoding;
use solana_client::{
    nonblocking::rpc_client::RpcClient,
    rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig},
    rpc_filter::{Memcmp, RpcFilterType},
};
use solana_pubkey::Pubkey;
use tracing::{debug, warn};

use crate::consts::OBLIGATION_ACCOUNT_SIZE;

// ---------------------------------------------------------------------------
// Market data
// ---------------------------------------------------------------------------

/// All on-chain data for a single lending market.
pub struct MarketData {
    pub market_pubkey: Pubkey,
    pub lending_market: LendingMarket,
    pub reserves: Vec<(Pubkey, Reserve)>,
}

/// Discover all LendingMarket accounts from the kLend program on-chain.
pub async fn fetch_all_markets(rpc: &RpcClient) -> Result<Vec<(Pubkey, LendingMarket)>> {
    let lm_size = 8 + std::mem::size_of::<LendingMarket>() as u64;
    let discrim_filter = RpcFilterType::Memcmp(Memcmp::new_raw_bytes(
        0,
        LendingMarket::SPL_DISCRIMINATOR_SLICE.to_vec(),
    ));
    let config = RpcProgramAccountsConfig {
        filters: Some(vec![discrim_filter, RpcFilterType::DataSize(lm_size)]),
        account_config: RpcAccountInfoConfig {
            encoding: Some(UiAccountEncoding::Base64Zstd),
            ..Default::default()
        },
        ..Default::default()
    };

    let accounts = rpc
        .get_program_accounts_with_config(&KLEND_PROGRAM_ID, config)
        .await
        .context("Failed to fetch LendingMarket accounts")?;

    let mut markets = Vec::new();
    for (pk, acc) in &accounts {
        if let Ok(lm) = from_account_data::<LendingMarket>(acc.data()) {
            markets.push((*pk, *lm));
        }
    }
    debug!(count = markets.len(), "Discovered lending markets");
    Ok(markets)
}

// ---------------------------------------------------------------------------
// Obligations
// ---------------------------------------------------------------------------

/// Fetch all obligations for a lending market. Uses three RPC-level filters:
/// discriminator, lending_market memcmp at offset 32, and DataSize.
/// Retries up to 3 times on failure.
pub async fn fetch_obligations(
    rpc: &RpcClient,
    market: &Pubkey,
) -> Result<Vec<(Pubkey, Obligation)>> {
    let filters = vec![
        RpcFilterType::Memcmp(Memcmp::new_raw_bytes(
            0,
            Obligation::SPL_DISCRIMINATOR_SLICE.to_vec(),
        )),
        RpcFilterType::Memcmp(Memcmp::new_raw_bytes(32, market.to_bytes().to_vec())),
        RpcFilterType::DataSize(OBLIGATION_ACCOUNT_SIZE),
    ];

    let config = RpcProgramAccountsConfig {
        filters: Some(filters),
        account_config: RpcAccountInfoConfig {
            encoding: Some(UiAccountEncoding::Base64Zstd),
            ..Default::default()
        },
        ..Default::default()
    };

    let mut last_err = String::new();
    for attempt in 1..=3 {
        match rpc
            .get_program_accounts_with_config(&KLEND_PROGRAM_ID, config.clone())
            .await
        {
            Ok(accounts) => {
                let mut parsed = Vec::new();
                for (pk, acc) in &accounts {
                    if let Ok(obligation) = from_account_data::<Obligation>(acc.data()) {
                        parsed.push((*pk, *obligation));
                    }
                }
                debug!(count = parsed.len(), market = %market, "Fetched obligations");
                return Ok(parsed);
            }
            Err(e) => {
                last_err = format!("{e}");
                warn!(attempt, error = %last_err, "Obligation fetch failed, retrying");
                if attempt < 3 {
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            }
        }
    }

    anyhow::bail!("Failed to fetch obligations after 3 attempts: {last_err}")
}

// ---------------------------------------------------------------------------
// Reserves
// ---------------------------------------------------------------------------

/// Fetch the LendingMarket and all its reserves.
pub async fn fetch_market_and_reserves(rpc: &RpcClient, market: &Pubkey) -> Result<MarketData> {
    let market_account = rpc
        .get_account(market)
        .await
        .with_context(|| format!("Failed to fetch market {market}"))?;
    let lending_market = *from_account_data::<LendingMarket>(market_account.data())
        .map_err(|e| anyhow::anyhow!("Failed to parse LendingMarket: {e}"))?;

    let filter = RpcFilterType::Memcmp(Memcmp::new_raw_bytes(32, market.to_bytes().to_vec()));
    let reserve_size = 8 + std::mem::size_of::<Reserve>() as u64;
    let discrim_filter = RpcFilterType::Memcmp(Memcmp::new_raw_bytes(
        0,
        Reserve::SPL_DISCRIMINATOR_SLICE.to_vec(),
    ));
    let config = RpcProgramAccountsConfig {
        filters: Some(vec![
            discrim_filter,
            filter,
            RpcFilterType::DataSize(reserve_size),
        ]),
        account_config: RpcAccountInfoConfig {
            encoding: Some(UiAccountEncoding::Base64Zstd),
            ..Default::default()
        },
        ..Default::default()
    };

    let accounts = rpc
        .get_program_accounts_with_config(&KLEND_PROGRAM_ID, config)
        .await
        .context("Failed to fetch reserves")?;

    let mut reserves = Vec::new();
    for (pk, acc) in &accounts {
        if let Ok(reserve) = from_account_data::<Reserve>(acc.data()) {
            reserves.push((*pk, *reserve));
        }
    }

    debug!(count = reserves.len(), market = %market, "Fetched reserves");

    Ok(MarketData {
        market_pubkey: *market,
        lending_market,
        reserves,
    })
}

/// Fetch reserves for a single obligation (batch by unique reserve pubkeys).
pub async fn fetch_reserves_for_obligation(
    rpc: &RpcClient,
    obligation: &Obligation,
) -> Result<Vec<(Pubkey, Reserve)>> {
    let mut reserve_pks: Vec<Pubkey> = obligation
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
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    reserve_pks.sort();

    let mut parsed = Vec::new();
    for chunk in reserve_pks.chunks(100) {
        let accounts = rpc.get_multiple_accounts(chunk).await?;
        for (pk, acc_opt) in chunk.iter().zip(accounts.iter()) {
            if let Some(acc) = acc_opt {
                if let Ok(reserve) = from_account_data::<Reserve>(acc.data()) {
                    parsed.push((*pk, *reserve));
                }
            }
        }
    }

    Ok(parsed)
}

// ---------------------------------------------------------------------------
// Token program detection
// ---------------------------------------------------------------------------

/// Detect whether a mint uses Token or Token-2022 by checking the account owner.
pub async fn detect_token_program(rpc: &RpcClient, mint: &Pubkey) -> Pubkey {
    match rpc.get_account(mint).await {
        Ok(acc) => acc.owner,
        Err(e) => {
            tracing::warn!(mint = %mint, error = %e, "Failed to detect token program, defaulting to Token");
            crate::consts::TOKEN_PROGRAM_ID
        }
    }
}
