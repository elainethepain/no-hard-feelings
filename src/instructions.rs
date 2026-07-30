use klend_interface::{
    helpers::refresh,
    instructions::{
        liquidate::{
            liquidate_obligation_and_redeem_reserve_collateral_v2,
            LiquidateObligationAndRedeemReserveCollateralV2Accounts,
        },
        obligation::{
            init_obligation_farms_for_reserve, refresh_obligation_farms_for_reserve,
            InitObligationFarmsForReserveAccounts, RefreshObligationFarmsForReserveAccounts,
        },
    },
    pda,
    state::Reserve,
    ObligationInfo, ReserveInfo, FARMS_PROGRAM_ID, KLEND_PROGRAM_ID,
};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_instruction::{AccountMeta, Instruction};
use solana_pubkey::Pubkey;
use solana_sdk::compute_budget::ComputeBudgetInstruction;

use crate::consts::KLEND_NULL_PUBKEY;

/// Seed for the obligation farm user state PDA in the Kamino Farms program.
/// From kfarms/src/utils/consts.rs: `BASE_SEED_USER_STATE = b"user"`.
const FARMS_USER_STATE_SEED: &[u8] = b"user";

// ---------------------------------------------------------------------------
// Oracle null check
// ---------------------------------------------------------------------------

/// Return `None` for both `Pubkey::default()` and `KLEND_NULL_PUBKEY`.
///
/// kLend uses two sentinel values for disabled oracles. klend-interface's
/// `non_default()` only checks `Pubkey::default()`, missing the `nu1111...`
/// sentinel used by some mainnet reserves.
pub fn maybe_null_pk(pk: Pubkey) -> Option<Pubkey> {
    if pk == Pubkey::default() || pk == KLEND_NULL_PUBKEY {
        None
    } else {
        Some(pk)
    }
}

/// Strip `KLEND_NULL_PUBKEY` from an existing `ReserveInfo`'s oracle fields.
fn null_check_reserve_info(mut info: ReserveInfo) -> ReserveInfo {
    info.pyth_oracle = info.pyth_oracle.and_then(maybe_null_pk);
    info.switchboard_price_oracle = info.switchboard_price_oracle.and_then(maybe_null_pk);
    info.switchboard_twap_oracle = info.switchboard_twap_oracle.and_then(maybe_null_pk);
    info.scope_prices = info.scope_prices.and_then(maybe_null_pk);
    info
}

/// Build a `ReserveInfo` with null-checked oracle fields.
pub fn reserve_info_with_null_check(address: Pubkey, reserve: &Reserve) -> ReserveInfo {
    ReserveInfo {
        address,
        lending_market: reserve.lending_market,
        liquidity_mint: reserve.liquidity.mint_pubkey,
        liquidity_token_program: reserve.liquidity.token_program,
        pyth_oracle: maybe_null_pk(reserve.config.token_info.pyth_configuration.price),
        switchboard_price_oracle: maybe_null_pk(
            reserve
                .config
                .token_info
                .switchboard_configuration
                .price_aggregator,
        ),
        switchboard_twap_oracle: maybe_null_pk(
            reserve
                .config
                .token_info
                .switchboard_configuration
                .twap_aggregator,
        ),
        scope_prices: maybe_null_pk(reserve.config.token_info.scope_configuration.price_feed),
    }
}

// ---------------------------------------------------------------------------
// ATA creation
// ---------------------------------------------------------------------------

/// Build idempotent ATA creation instructions for the three token accounts
/// needed by a liquidation: source liquidity, destination collateral,
/// and destination liquidity.
pub fn build_ata_creation_ixs(
    owner: &Pubkey,
    repay_reserve: &Reserve,
    withdraw_reserve: &Reserve,
    ctoken_mint: &Pubkey,
    ctoken_program: &Pubkey,
) -> Vec<Instruction> {
    vec![
        spl_associated_token_account::instruction::create_associated_token_account_idempotent(
            owner,
            owner,
            &repay_reserve.liquidity.mint_pubkey,
            &repay_reserve.liquidity.token_program,
        ),
        spl_associated_token_account::instruction::create_associated_token_account_idempotent(
            owner,
            owner,
            ctoken_mint,
            ctoken_program,
        ),
        spl_associated_token_account::instruction::create_associated_token_account_idempotent(
            owner,
            owner,
            &withdraw_reserve.liquidity.mint_pubkey,
            &withdraw_reserve.liquidity.token_program,
        ),
    ]
}

// ---------------------------------------------------------------------------
// Compute budget
// ---------------------------------------------------------------------------

/// Build compute budget instructions with a priority fee.
pub fn build_compute_budget_ixs(
    compute_units: u32,
    priority_fee_micro_lamports: u64,
) -> Vec<Instruction> {
    vec![
        ComputeBudgetInstruction::set_compute_unit_limit(compute_units),
        ComputeBudgetInstruction::set_compute_unit_price(priority_fee_micro_lamports),
    ]
}

// ---------------------------------------------------------------------------
// Farm handling
// ---------------------------------------------------------------------------

/// Farm mode constants matching `ReserveFarmKind` in the on-chain program.
pub const FARM_MODE_DEBT: u8 = 0;
pub const FARM_MODE_COLLATERAL: u8 = 1;

/// Derive the obligation farm user state PDA.
fn obligation_farm_user_state(farm_state: &Pubkey, obligation: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[
            FARMS_USER_STATE_SEED,
            farm_state.as_ref(),
            obligation.as_ref(),
        ],
        &FARMS_PROGRAM_ID,
    )
    .0
}

/// Build farm init and refresh instructions that wrap a liquidation.
///
/// Returns (pre_instructions, post_instructions) to prepend/append to the
/// liquidation instruction. Matches the terminator's
/// `wrap_obligation_instruction_with_farms` pattern.
///
/// Batches all farm user state existence checks into a single
/// `get_multiple_accounts` call instead of sequential per-PDA fetches.
#[allow(clippy::too_many_arguments)]
pub async fn build_farm_ixs(
    rpc: &RpcClient,
    payer: &Pubkey,
    obligation_pk: &Pubkey,
    obligation_owner: &Pubkey,
    collateral_reserve_pk: &Pubkey,
    collateral_reserve: &Reserve,
    debt_reserve_pk: &Pubkey,
    debt_reserve: &Reserve,
) -> (Vec<Instruction>, Vec<Instruction>) {
    let mut pre_ixs = Vec::new();
    let mut post_ixs = Vec::new();

    let lending_market = collateral_reserve.lending_market;
    let (lma, _) = pda::lending_market_authority(&KLEND_PROGRAM_ID, &lending_market);

    // Collect all (reserve, farm_state, mode) combinations with active farms.
    let farm_entries: Vec<(Pubkey, Pubkey, u8)> = [
        (
            *collateral_reserve_pk,
            collateral_reserve.farm_collateral,
            FARM_MODE_COLLATERAL,
        ),
        (
            *collateral_reserve_pk,
            collateral_reserve.farm_debt,
            FARM_MODE_DEBT,
        ),
        (
            *debt_reserve_pk,
            debt_reserve.farm_collateral,
            FARM_MODE_COLLATERAL,
        ),
        (*debt_reserve_pk, debt_reserve.farm_debt, FARM_MODE_DEBT),
    ]
    .into_iter()
    .filter(|(_, farm_state, _)| *farm_state != Pubkey::default())
    .collect();

    if farm_entries.is_empty() {
        return (pre_ixs, post_ixs);
    }

    // Derive all user state PDAs and batch-check existence in one RPC call.
    let user_states: Vec<Pubkey> = farm_entries
        .iter()
        .map(|(_, farm_state, _)| obligation_farm_user_state(farm_state, obligation_pk))
        .collect();

    let existence = rpc
        .get_multiple_accounts(&user_states)
        .await
        .unwrap_or_else(|_| vec![None; user_states.len()]);

    for (i, (reserve_pk, farm_state, mode)) in farm_entries.iter().enumerate() {
        let user_state = user_states[i];
        let exists = existence.get(i).map(|a| a.is_some()).unwrap_or(false);

        // Init if the obligation farm user state doesn't exist yet.
        if !exists {
            pre_ixs.push(init_obligation_farms_for_reserve(
                InitObligationFarmsForReserveAccounts {
                    payer: *payer,
                    owner: *obligation_owner,
                    obligation: *obligation_pk,
                    lending_market_authority: lma,
                    reserve: *reserve_pk,
                    reserve_farm_state: *farm_state,
                    obligation_farm: user_state,
                    lending_market,
                },
                *mode,
            ));
        }

        // Refresh farm before and after the liquidation.
        let refresh_ix = refresh_obligation_farms_for_reserve(
            RefreshObligationFarmsForReserveAccounts {
                crank: *payer,
                obligation: *obligation_pk,
                lending_market_authority: lma,
                reserve: *reserve_pk,
                reserve_farm_state: *farm_state,
                obligation_farm_user_state: user_state,
                lending_market,
            },
            *mode,
        );

        pre_ixs.push(refresh_ix.clone());
        post_ixs.push(refresh_ix);
    }

    (pre_ixs, post_ixs)
}

// ---------------------------------------------------------------------------
// Liquidation instruction
// ---------------------------------------------------------------------------

/// Build the full instruction set for a liquidation: refresh reserves,
/// refresh obligation, then liquidate-and-redeem.
///
/// Uses actual addresses from the `Reserve` struct fields, NOT PDA derivation.
/// `ReservePdas::derive()` does not match existing mainnet reserves. The
/// on-chain program validates against stored addresses, same as Kamino's
/// terminator bot.
/// Farm accounts for a liquidation. Derived from the collateral and debt reserves.
#[derive(Default)]
pub struct FarmAccounts {
    pub collateral_obligation_farm_user_state: Option<Pubkey>,
    pub collateral_reserve_farm_state: Option<Pubkey>,
    pub debt_obligation_farm_user_state: Option<Pubkey>,
    pub debt_reserve_farm_state: Option<Pubkey>,
}

impl FarmAccounts {
    /// Derive farm accounts from the collateral and debt reserves.
    ///
    /// The liquidation instruction needs the collateral reserve's collateral farm
    /// and the debt reserve's debt farm. These are the two farms directly affected
    /// by the liquidation (collateral is seized, debt is repaid).
    ///
    /// Note: `build_farm_ixs` handles all 4 combinations for the refresh
    /// instructions that wrap the liquidation. This struct only covers what
    /// the liquidation instruction itself needs.
    pub fn from_reserves(
        obligation_pk: &Pubkey,
        collateral_reserve: &Reserve,
        debt_reserve: &Reserve,
    ) -> Self {
        let coll_farm = collateral_reserve.farm_collateral;
        let debt_farm = debt_reserve.farm_debt;

        Self {
            collateral_reserve_farm_state: maybe_null_pk(coll_farm),
            collateral_obligation_farm_user_state: maybe_null_pk(coll_farm)
                .map(|f| obligation_farm_user_state(&f, obligation_pk)),
            debt_reserve_farm_state: maybe_null_pk(debt_farm),
            debt_obligation_farm_user_state: maybe_null_pk(debt_farm)
                .map(|f| obligation_farm_user_state(&f, obligation_pk)),
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn build_refresh_and_liquidate_ixs(
    liquidator: Pubkey,
    repay_reserve_pk: Pubkey,
    repay_reserve: &Reserve,
    withdraw_reserve_pk: Pubkey,
    withdraw_reserve: &Reserve,
    obligation: &ObligationInfo,
    obligation_reserves: &[ReserveInfo],
    user_source_liquidity: Pubkey,
    user_destination_collateral: Pubkey,
    user_destination_liquidity: Pubkey,
    liquidity_amount: u64,
    min_acceptable_received_liquidity_amount: u64,
    max_allowed_ltv_override_percent: u64,
    farms: &FarmAccounts,
) -> Vec<Instruction> {
    let (lma, _) = pda::lending_market_authority(&KLEND_PROGRAM_ID, &repay_reserve.lending_market);

    let repay_info = reserve_info_with_null_check(repay_reserve_pk, repay_reserve);
    let withdraw_info = reserve_info_with_null_check(withdraw_reserve_pk, withdraw_reserve);

    // Null-check all obligation reserve infos as well.
    let checked_obligation_reserves: Vec<ReserveInfo> = obligation_reserves
        .iter()
        .map(|r| null_check_reserve_info(r.clone()))
        .collect();

    let reserve_lookup = |pk: &Pubkey| -> Option<ReserveInfo> {
        checked_obligation_reserves
            .iter()
            .find(|r| r.address == *pk)
            .cloned()
    };

    // Build refresh instructions. Fall back to manual refresh if
    // `refresh_all_for_obligation` can't resolve all reserves.
    let mut ixs = match refresh::refresh_all_for_obligation(
        &repay_reserve.lending_market,
        obligation,
        &reserve_lookup,
    ) {
        Ok(refresh_ixs) => refresh_ixs,
        Err(_) => {
            vec![
                refresh::refresh_reserve(&repay_info),
                refresh::refresh_reserve(&withdraw_info),
                refresh::refresh_obligation(&repay_reserve.lending_market, obligation),
            ]
        }
    };

    // Remaining accounts: deposit reserves + borrow reserves + referrer token states.
    let mut remaining: Vec<AccountMeta> = Vec::new();
    for r in &obligation.deposit_reserves {
        remaining.push(AccountMeta::new(*r, false));
    }
    for r in &obligation.borrow_reserves {
        remaining.push(AccountMeta::new(*r, false));
    }
    if let Some(referrer) = obligation.referrer {
        for borrow_reserve in &obligation.borrow_reserves {
            let (rts, _) = pda::referrer_token_state(&KLEND_PROGRAM_ID, &referrer, borrow_reserve);
            remaining.push(AccountMeta::new(rts, false));
        }
    }

    ixs.push(liquidate_obligation_and_redeem_reserve_collateral_v2(
        LiquidateObligationAndRedeemReserveCollateralV2Accounts {
            liquidator,
            obligation: obligation.address,
            lending_market: repay_reserve.lending_market,
            lending_market_authority: lma,
            repay_reserve: repay_reserve_pk,
            repay_reserve_liquidity_mint: repay_reserve.liquidity.mint_pubkey,
            repay_reserve_liquidity_supply: repay_reserve.liquidity.supply_vault,
            withdraw_reserve: withdraw_reserve_pk,
            withdraw_reserve_liquidity_mint: withdraw_reserve.liquidity.mint_pubkey,
            withdraw_reserve_collateral_mint: withdraw_reserve.collateral.mint_pubkey,
            withdraw_reserve_collateral_supply: withdraw_reserve.collateral.supply_vault,
            withdraw_reserve_liquidity_supply: withdraw_reserve.liquidity.supply_vault,
            withdraw_reserve_liquidity_fee_receiver: withdraw_reserve.liquidity.fee_vault,
            user_source_liquidity,
            user_destination_collateral,
            user_destination_liquidity,
            repay_liquidity_token_program: repay_reserve.liquidity.token_program,
            withdraw_liquidity_token_program: withdraw_reserve.liquidity.token_program,
            collateral_obligation_farm_user_state: farms.collateral_obligation_farm_user_state,
            collateral_reserve_farm_state: farms.collateral_reserve_farm_state,
            debt_obligation_farm_user_state: farms.debt_obligation_farm_user_state,
            debt_reserve_farm_state: farms.debt_reserve_farm_state,
        },
        liquidity_amount,
        min_acceptable_received_liquidity_amount,
        max_allowed_ltv_override_percent,
        remaining,
    ));

    ixs
}
