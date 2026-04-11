# Decibel Bot — AI Assistant Guide

## Project Overview

This is a perpetual futures **market-maker bot** for [Decibel DEX](https://decibel.markets) on the
Aptos blockchain. Every cycle the bot fetches state, computes bid/ask quotes using an inventory-skew
model, cancels its resting orders, and places fresh POST_ONLY limit orders on both sides. This
repository ships a **Go 1.24+** implementation under `market-maker/go/` (Aptos I/O via `aptos-go-sdk` **v1** — root module, not `/v2`).

---

## Repository Layout

```
decibel-bot/
└── market-maker/
    ├── go/                          Go 1.24+ implementation
    │   ├── main.go                  Entry: load .env → config.Load() → bot.Run()
    │   ├── go.mod                   Module: decibel-mm-bot, go 1.24+
    │   ├── .env.example             Credential + trading param template
    │   ├── README.md                Usage and parameter reference
    │   ├── config/config.go         Params; CLI flag > env > .env > default layering
    │   ├── bot/bot.go               Main loop, runCycle(), adaptive spread state machine
    │   ├── api/client.go            Decibel REST API; FetchState() parallel fetch
    │   ├── aptos/client.go          aptos-go-sdk v1; SubmitEntryFunction()
    │   ├── aptos/address.go         Named-object derivation (e.g. GlobalPerpEngine for logs)
    │   └── pricing/
    │       ├── pricing.go           Pure ComputeQuotes() — no I/O, no side effects
    │       └── pricing_test.go      Table-driven unit tests
    └── 做市参数调优指南.md           Chinese parameter tuning guide
```

---

## Build & Test Commands

```bash
# ── Go ──────────────────────────────────────────────────────────────────────
cd market-maker/go
# Go 1.24+ required (aptos-go-sdk v1)

go run .                                        # run with .env / env vars
go run . -dry-run                               # no transactions sent
go run . -network mainnet -spread 0.0005        # override specific params
go build -o decibel-mm .                        # build binary
go test ./...                                   # run all tests
```

**Flags:** Go `flag` package uses single dashes (`-dry-run`, `-spread`); `-flag=value` is also valid. For **boolean** flags, prefer `-name=true` / `-name=false` in scripts; `config.Load` also normalizes `-name false` for the known bool flags (see `market-maker/go/README.md`, section **Boolean flags**).

---

## Key Concepts

### Inventory-skew pricing model

The core algorithm lives in `pricing/pricing.go`. It is pure math with no I/O — safe to unit test in isolation.

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

Returns `nil` when `|inventory| >= max_inventory` (stop quoting) or size rounds to zero.

### Adaptive spread

`effectiveSpread` is mutable state on the bot struct, starting at
`cfg.Spread`. Adjusted each cycle:

- **Fill detected** (inventory changed by > `lot_size × 0.5`): widen by `spread_step × 0.5`,
  capped at the initial `cfg.Spread`
- **No fill for N cycles** (`SPREAD_NO_FILL_CYCLES`): narrow by `spread_step`, floored at
  `SPREAD_MIN`
- When `AUTO_SPREAD=false` (default): only log a suggestion — never mutate the spread

### Aptos transaction lifecycle

**Go** uses `aptos-go-sdk` v1 (`EntryFunctionWithArgs` from on-chain ABI, `BuildTransaction`, `SignedTransaction`, `SubmitTransaction`, `WaitForTransaction`), which performs the same logical steps as the classic fullnode flow (sequence + gas, BCS signing, submit, poll).

Reference sequence if implementing against raw REST instead of the SDK:

1. Parallel-fetch sequence number and gas price
2. Build unsigned transaction with `entry_function_payload`
3. Encode for signing, Ed25519-sign BCS payload, submit, poll by hash until committed

### Move ABI argument encoding

`place_order_to_subaccount` takes 15 positional arguments:

- `price` and `size` are sent as **decimal strings** (`"12345678"`), not JSON numbers
- `time_in_force`: `0` = GTC, `1` = POST_ONLY, `2` = IOC
- `Option<T>` None = `{"vec": []}` — **not** JSON `null`
- `order_id` for cancels is a u128 decimal string

### Market config scaling

The `/markets` API returns `tick_size`, `lot_size`, `min_size` as raw chain integers. The bot divides by `10^decimals` immediately after fetching (`FetchMarkets`). All internal math uses human-readable floats (e.g. `1.0` = $1, `0.001` = 0.001 BTC). Values are scaled back up in `scale_price()` / `scale_size()` when building transactions.

### Address normalization

Aptos addresses may have varying `0x` prefixes and leading zeros. The API client normalizes before comparison: strip `0x`, lowercase, strip leading zeros (`AddrEqual`).

### Cancel semantics

A cancel that fails with `ERESOURCE_DOES_NOT_EXIST` or `EORDER_NOT_FOUND` is treated as
**success** — the order is already gone. Only genuine VM failures trigger the `CANCEL_RESYNC_S` wait.
See `TxResult.CancelSucceeded()`.

### State fetch is always parallel

Every cycle, account overview + positions + open orders + mid-price are fetched concurrently via `errgroup.WithContext`. Serial fetching adds ~200 ms per cycle.

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

`PACKAGE_ADDRESS` must be non-empty (set explicitly or via `NETWORK` preset). The **GlobalPerpEngine** address is derived from the package address for logging only (not configurable).

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
# Fill in: BEARER_TOKEN, SUBACCOUNT_ADDRESS, PRIVATE_KEY
go run . -dry-run    # validate config before going live
```

---

## Testing Conventions

- Pricing logic is unit-tested in isolation — no network, no state (`go test ./...`)
- Typical test parameters: `mid=100_000`, `spread=0.001`, `tick=1.0`, `lot=0.00001`
- Aptos named-object derivation is tested in `aptos/address_test.go`
- No integration tests — use `-dry-run` against a live network for end-to-end validation

---

## Notes for AI Assistants

- The pricing module is the most common change target — unit tests live in `pricing_test.go`
- `做市参数调优指南.md` is operational guidance for human traders; it does not require code changes
- Go module name: `decibel-mm-bot`
- Do not add error handling for impossible states (e.g. mid-price always positive inside `runCycle` — it's already guarded before the call)
