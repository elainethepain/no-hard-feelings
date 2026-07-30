use klend_interface::state::Obligation;
use solana_pubkey::Pubkey;

/// Canonical null pubkey used by kLend for disabled oracle fields.
/// Prints as "nu11111111111111111111111111111111111111111".
/// klend-interface's `non_default` only checks `Pubkey::default()`, missing this sentinel.
pub const KLEND_NULL_PUBKEY: Pubkey = Pubkey::new_from_array([
    11, 193, 238, 216, 208, 116, 241, 195, 55, 212, 76, 22, 75, 202, 40, 216, 76, 206, 27, 169,
    138, 64, 177, 28, 19, 90, 156, 0, 0, 0, 0, 0,
]);

/// Wrapped SOL mint.
pub const WSOL_MINT: Pubkey = Pubkey::new_from_array([
    6, 155, 136, 87, 254, 171, 129, 132, 251, 104, 127, 99, 70, 24, 192, 53, 218, 196, 57, 220, 26,
    235, 59, 85, 152, 160, 240, 0, 0, 0, 0, 1,
]);

/// SPL Token program ID.
pub const TOKEN_PROGRAM_ID: Pubkey = Pubkey::new_from_array([
    6, 221, 246, 225, 215, 101, 161, 147, 217, 203, 225, 70, 206, 235, 121, 172, 28, 180, 133, 237,
    95, 91, 55, 145, 58, 140, 245, 133, 126, 255, 0, 169,
]);

/// SPL Token-2022 program ID.
pub const TOKEN_2022_PROGRAM_ID: Pubkey = Pubkey::new_from_array([
    6, 221, 246, 225, 238, 117, 143, 222, 170, 49, 155, 47, 74, 38, 166, 5, 188, 15, 219, 134, 62,
    249, 202, 95, 117, 82, 227, 42, 234, 43, 50, 30,
]);

/// kswap API endpoint.
pub const KSWAP_API: &str = "https://api.kamino.finance/kswap/swap/";

/// Default swap slippage (50 bps = 0.5%).
pub const DEFAULT_SWAP_SLIPPAGE_BPS: u16 = 50;

/// Minimum actual debt (USD) below which positions are skipped.
pub const DEFAULT_MIN_DEBT_USD: f64 = 5.0;

/// Rough SOL transaction fee in USD for gas cost estimation.
pub const GAS_COST_USD: f64 = 0.01;

/// Full basis points (100% = 10_000 bps).
pub const FULL_BPS: f64 = 10_000.0;

/// Obligation account data size: 8-byte discriminator + struct size.
pub const OBLIGATION_ACCOUNT_SIZE: u64 = 8 + std::mem::size_of::<Obligation>() as u64;
