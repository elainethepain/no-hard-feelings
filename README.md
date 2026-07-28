# No Hard Feelings

A simple, fast, and easily deployable liquidation bot for the **Kamino Lend (kLend)** protocol on **Solana**. 


### Disclaimer

This bot is provided as-is, without any warranty. Use at your own risk. The authors are not responsible for any loss of funds resulting from the use of this bot, including gas fees, failed transactions, or liquidations on misconfigured markets.


## Requirements

- Rust >= 1.82
- A Solana RPC endpoint that supports `getProgramAccounts` (e.g. Helius, Triton, QuickNode)
- A funded Solana keypair

## Installation

```bash
git clone https://github.com/elainethepain/No-Hard-Feelings.git
cd No-Hard-Feelings
cargo build --release
```

## Configuration

All configuration is via CLI flags or environment variables. Create a `.env` file at the repo root:

```
RPC_URL=https://mainnet.helius-rpc.com/?api-key=your-key-here
LIQUIDATOR_KEYPAIR=/path/to/keypair.json
PRIORITY_FEE=1000
```

| Variable             | CLI Flag           | Description                                              | Default                        |
| -------------------- | ------------------ | -------------------------------------------------------- | ------------------------------ |
| `RPC_URL`            | `--rpc-url`        | Solana RPC endpoint                                      | (required)                     |
| `LIQUIDATOR_KEYPAIR` | `--keypair`        | Path to liquidator keypair JSON                          | `~/.config/solana/id.json`     |
| `MARKETS`            | `--markets`        | Comma-separated market pubkeys (omit to discover all)    | (auto-discover)                |
| `PRIORITY_FEE`       | `--priority-fee`   | Priority fee in micro-lamports per compute unit          | `1000`                         |

## Run the Bot

### Scan

Scan all markets for liquidatable positions and save results to `scan_results.json`:

```bash
nhf scan
nhf scan --min-debt-usd 10          # only show positions with >$10 debt
```

### Execute

Find the best liquidation opportunity within your budget. Dry-run by default — add `--send` to execute:

```bash
nhf execute --budget 5              # dry run: simulate only
nhf execute --budget 5 --send       # live: swap + liquidate on-chain
nhf execute --budget 50 --send --max-attempts 20
```

### Crank

Continuous loop — scans, scores, and executes every cycle:

```bash
nhf crank --budget 5 --interval 10 --min-profit 0.01
```

| Flag             | Description                                        | Default |
| ---------------- | -------------------------------------------------- | ------- |
| `--budget`       | Max USD to deploy per liquidation                  | `5.0`   |
| `--interval`     | Seconds between scan cycles                        | `10`    |
| `--min-profit`   | Skip opportunities below this USD profit           | `0.01`  |
| `--max-attempts` | Max candidates to simulate per cycle               | `10`    |

### Liquidate

Manually liquidate a specific obligation with known reserve pair:

```bash
nhf liquidate <OBLIGATION_PUBKEY> <WITHDRAW_RESERVE> <REPAY_RESERVE>
nhf liquidate <OBLIGATION_PUBKEY> <WITHDRAW_RESERVE> <REPAY_RESERVE> --send
```

### Swap

Swap tokens via the kswap API. Amount is in native token units (lamports, raw USDC, etc.):

```bash
nhf swap <INPUT_MINT> <OUTPUT_MINT> <AMOUNT_IN_NATIVE_UNITS>
nhf swap So111...112 EPjFW...Dt1v 1000000000 --slippage-bps 100
#        ^SOL mint   ^USDC mint   ^1 SOL      ^1% slippage
```

### Rebalance

Convert all non-base tokens back to a base currency, maintain minimum SOL for gas, and unwrap WSOL:

```bash
nhf rebalance
nhf rebalance --base-token EPjFW...Dt1v --min-sol 0.05 --dust-threshold 5.0
```

| Flag                | Description                                        | Default |
| ------------------- | -------------------------------------------------- | ------- |
| `--base-token`      | Token mint to hold as base currency                | USDC    |
| `--min-sol`         | Minimum SOL to keep for gas                        | `0.5`   |
| `--dust-threshold`  | Don't swap holdings worth less than this (USD)     | `5.0`   |
| `--slippage-bps`    | Swap slippage in basis points                      | `50`    |

## Dependencies

| Crate                              | Purpose                                    |
| ---------------------------------- | ------------------------------------------ |
| `klend-interface 0.6.0`           | kLend types, parsers, instruction structs  |
| `solana-client ~2.3`              | Async RPC client                           |
| `solana-sdk ~2.3`                 | Transaction building, signing              |
| `spl-associated-token-account 6`  | ATA derivation and creation                |
| `tokio 1`                         | Async runtime                              |
| `clap 4`                          | CLI                                        |
| `reqwest ~0.12`                   | HTTP client for kswap API                  |
| `tracing`                         | Structured logging                         |
