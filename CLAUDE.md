# Decibel Bot — AI Assistant Guide

## Project Overview

This is a perpetual futures **market-maker bot** for [Decibel DEX](https://decibel.markets) on the
Aptos blockchain. Every cycle the bot fetches state, computes bid/ask quotes using an inventory-skew
model, cancels its resting orders, and places fresh POST_ONLY limit orders on both sides. It has two
complete, logically-identical implementations — Go 1.21 and Rust/tokio — that must always be kept in sync.

---

## CRITICAL: Dual-Language Rule

> **Any change to market-making logic MUST be applied to both implementations simultaneously.**
>
> - `market-maker/go/` — Go 1.21, `log/slog`, `errgroup`
> - `market-maker/rust/src/` — Rust + tokio, `tracing`, `tokio::try_join!`
>
> Idiomatic differences are allowed (goroutine vs async/await, slog vs tracing).
> The numeric output of `ComputeQuotes` / `compute_quotes` must be bit-identical.

Counterpart file map — always edit both sides:

| Go | Rust |
|----|------|
| `go/bot/bot.go` | `rust/src/bot.rs` |
| `go/pricing/pricing.go` | `rust/src/pricing.rs` |
| `go/config/config.go` | `rust/src/config.rs` |
| `go/api/client.go` | `rust/src/api.rs` |
| `go/aptos/client.go` | `rust/src/aptos.rs` |

---

## Repository Layout

```
decibel-bot/
└── market-maker/
    ├── go/                          Go 1.21 implementation
    │   ├── main.go                  Entry: load .env → config.Load() → bot.Run()
    │   ├── go.mod                   Module: decibel-mm-bot, go 1.21
    │   ├── .env.example             Credential + trading param template
    │   ├── README.md                Go-specific usage and parameter reference
    │   ├── config/config.go         30+ params; CLI flag > env > .env > default layering
    │   ├── bot/bot.go               Main loop, runCycle(), adaptive spread state machine
    │   ├── api/client.go            Decibel REST API; FetchState() parallel fetch
    │   ├── aptos/client.go          Ed25519 sign + submit; SubmitEntryFunction(); DeriveAddress()
    │   └── pricing/
    │       ├── pricing.go           Pure ComputeQuotes() — no I/O, no side effects
    │       └── pricing_test.go      Table-driven unit tests
    ├── rust/                        Rust implementation (mirrors Go exactly)
    │   ├── Cargo.toml               Crate: decibel-mm-bot, binary: decibel-mm
    │   ├── Cargo.lock               Locked for reproducible builds
    │   ├── README.md                Rust-specific usage and parameter reference
    │   └── src/
    │       ├── main.rs              Entry: dotenvy → Args::parse() → bot::run()
    │       ├── config.rs            clap::Parser Args; effective_urls(); parse_private_key()
    │       ├── bot.rs               Async MarketMaker struct; run_loop(); run_cycle()
    │       ├── api.rs               reqwest ApiClient; fetch_state() via tokio::try_join!
    │       ├── aptos.rs             ed25519-dalek; submit_entry_function(); derive_address()
    │       └── pricing.rs           compute_quotes() + #[cfg(test)] inline tests
    └── 做市参数调优指南.md           Chinese parameter tuning guide (applies to both impls)
```

---

## Build & Test Commands

```bash
# ── Go ──────────────────────────────────────────────────────────────────────
cd market-maker/go

go run .                                        # run with .env / env vars
go run . -dry-run                               # no transactions sent
go run . -network mainnet -spread 0.0005        # override specific params
go build -o decibel-mm .                        # build binary
go test ./...                                   # run all tests

# ── Rust ────────────────────────────────────────────────────────────────────
cd market-maker/rust

cargo run --release                             # run with .env / env vars
cargo run --release -- --dry-run               # note: -- before flags
cargo run --release -- --network mainnet --spread 0.0005
cargo build --release                           # binary: target/release/decibel-mm
cargo test                                      # run all tests

RUST_LOG=debug cargo run --release              # verbose logging
```

**Flag syntax gotcha:** Go uses single dashes (`-dry-run`, `-spread`); Rust/clap uses double
dashes (`--dry-run`, `--spread`). The `--` separator before Rust flags is required when using
`cargo run`.

---

## Key Concepts

### Inventory-skew pricing model

The core algorithm lives in `pricing/pricing.go` (Go) and `pricing.rs` (Rust). It is pure math
with no I/O — safe to unit test in isolation.

```
half_spread = spread / 2
skew        = inventory × skew_per_unit    // positive inventory → push quotes DOWN

raw_bid = mid × (1 − half_spread − skew)
raw_ask = mid × (1 + half_spread − skew)

bid = floor(raw_bid / tick) × tick         // always round DOWN
ask =  ceil(raw_ask / tick) × tick         // always round UP

if ask ≤ bid: ask = bid + tick             // enforce minimum spread after rounding

size = round(order_size / lot) × lot
```

The skew term is the key: a long position (positive inventory) shifts both quotes downward,
making the ask cheaper and the bid farther from market, which encourages fills that reduce
the position (mean-reversion).

Returns `nil` / `None` when `|inventory| >= max_inventory` (stop quoting) or size rounds to zero.

### Adaptive spread

`effectiveSpread` (`effective_spread` in Rust) is mutable state on the bot struct, starting at
`cfg.Spread`. Adjusted each cycle:

- **Fill detected** (inventory changed by > `lot_size × 0.5`): widen by `spread_step × 0.5`,
  capped at the initial `cfg.Spread`
- **No fill for N cycles** (`SPREAD_NO_FILL_CYCLES`): narrow by `spread_step`, floored at
  `SPREAD_MIN`
- When `AUTO_SPREAD=false` (default): only log a suggestion — never mutate the spread

### Aptos transaction lifecycle

The fullnode REST API requires this 6-step protocol — it cannot be simplified:

1. Parallel-fetch sequence number (`GET /accounts/{addr}`) and gas price (`GET /estimate_gas_price`)
2. Build unsigned JSON transaction with `entry_function_payload`
3. `POST /transactions/encode_submission` → receive BCS-encoded bytes as hex string
4. Ed25519-sign the **BCS bytes** (not the JSON)
5. Attach `{"type": "ed25519_signature", "public_key": "0x...", "signature": "0x..."}` to the JSON
6. `POST /transactions` → get hash; poll `GET /transactions/by_hash/{hash}` until committed (12 attempts)

### Move ABI argument encoding

`place_order_to_subaccount` takes 15 positional arguments:

- `price` and `size` are sent as **decimal strings** (`"12345678"`), not JSON numbers
- `time_in_force`: `0` = GTC, `1` = POST_ONLY, `2` = IOC
- `Option<T>` None = `{"vec": []}` — **not** JSON `null`
- `order_id` for cancels is a u128 decimal string

### Market config scaling

The `/markets` API returns `tick_size`, `lot_size`, `min_size` as raw chain integers. Both
implementations divide by `10^decimals` immediately after fetching (`FetchMarkets` /
`fetch_markets`). All internal math uses human-readable floats (e.g. `1.0` = $1, `0.001` = 0.001 BTC).
Values are scaled back up in `scale_price()` / `scale_size()` when building transactions.

### Address normalization

Aptos addresses may have varying `0x` prefixes and leading zeros. Both implementations normalize
before comparison: strip `0x`, lowercase, strip leading zeros. See `AddrEqual` (Go) / `addr_eq` (Rust).

### Cancel semantics

A cancel that fails with `ERESOURCE_DOES_NOT_EXIST` or `EORDER_NOT_FOUND` is treated as
**success** — the order is already gone. Only genuine VM failures trigger the `CANCEL_RESYNC_S` wait.
See `TxResult.CancelSucceeded()` (Go) / `tx_result.cancel_succeeded()` (Rust).

### State fetch is always parallel

Every cycle, account overview + positions + open orders + mid-price are fetched concurrently:
- Go: `errgroup.WithContext`
- Rust: `tokio::try_join!`

This is intentional. Serial fetching adds ~200 ms per cycle.

---

## Configuration Priority

```
CLI flag  >  explicit env var  >  .env file  >  built-in default
```

For `REST_API_BASE`, `APTOS_FULLNODE_URL`, `PACKAGE_ADDRESS`: the `NETWORK` preset fills defaults
which can then be individually overridden.

**Required — bot refuses to start without these:**

| Variable | Description |
|----------|-------------|
| `BEARER_TOKEN` | REST API bearer token from Decibel dashboard |
| `SUBACCOUNT_ADDRESS` | Your subaccount object address (`0x...`) |
| `PRIVATE_KEY` | 32-byte Ed25519 seed, hex-encoded (`0x` prefix optional; also accepts 64-byte seed‖pubkey) |
| `PERP_ENGINE_GLOBAL_ADDRESS` | Perp engine global object address |

**Network (defaults apply per preset):**

- `NETWORK=testnet` (default) or `mainnet` — sets REST API base, fullnode URL, package address

**Key trading parameters (defaults):**

| Param | Default | Purpose |
|-------|---------|---------|
| `MARKET_NAME` | `BTC/USD` | Market to trade |
| `SPREAD` | `0.001` | Total bid-ask spread (0.1%) |
| `ORDER_SIZE` | `0.001` | Base units per side per cycle |
| `MAX_INVENTORY` | `0.005` | Stop quoting when \|position\| ≥ this |
| `SKEW_PER_UNIT` | `0.0001` | Extra half-spread per unit of inventory |
| `REFRESH_INTERVAL` | `20.0` | Seconds between cycles |
| `DRY_RUN` | `false` | Log without sending transactions |
| `AUTO_SPREAD` | `false` | Enable adaptive spread adjustment |

`NODE_API_KEY` falls back to `BEARER_TOKEN` if unset.

**Quick start:**
```bash
cp market-maker/go/.env.example market-maker/go/.env
# Fill in: BEARER_TOKEN, SUBACCOUNT_ADDRESS, PRIVATE_KEY, PERP_ENGINE_GLOBAL_ADDRESS
go run . -dry-run    # validate config before going live
```

---

## Testing Conventions

- Pricing logic is unit-tested in isolation in **both** languages — no network, no state
- Test parameters are matched across Go and Rust: `mid=100_000`, `spread=0.001`, `tick=1.0`, `lot=0.00001`
- **When changing pricing logic: run `go test ./...` AND `cargo test`**
- Rust also has unit tests for address derivation in `aptos.rs`
- No integration tests exist — use `--dry-run` against a live network for end-to-end validation

---

## Notes for AI Assistants

- Always open both language counterparts before editing any module
- The pricing module is the most common change target — tests exist for it in both languages
- `做市参数调优指南.md` is operational guidance for human traders; it does not require code changes
- Go module name: `decibel-mm-bot`; Rust crate name: `decibel-mm-bot`, binary: `decibel-mm`
- Do not add error handling for impossible states (e.g. mid-price always positive inside `runCycle` — it's already guarded before the call)
