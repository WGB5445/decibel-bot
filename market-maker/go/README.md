# Decibel Market Maker — Go

A perpetual-futures market-maker bot for the [Decibel DEX](https://decibel.trade) on Aptos.

Requires **Go 1.24+**. Built with a **three-layer pluggable architecture**:

**CI:** [GitHub Actions](../../.github/workflows/market-maker-go.yml) on **Linux** (`ubuntu-latest`) runs `gofmt`, `go vet ./...`, `go test ./...`, and `go build` under `market-maker/go/` when paths match. **Pull requests** into `main` or `master` trigger one run per update; **pushes** to `main`/`master` only (so pushing to a PR branch does not double-fire with the PR event). Add your default branch name to the workflow if it differs.

- **Exchange Layer** — abstraction for different DEX/chains (currently Decibel/Aptos)
- **Strategy Layer** — market-making logic decoupled from exchange details
- **Notification Layer** — monitoring/control via Telegram (extensible to Discord, Slack, etc.)

On-chain transactions use [aptos-go-sdk](https://github.com/aptos-labs/aptos-go-sdk) **v1** (`github.com/aptos-labs/aptos-go-sdk`, currently **v1.12.1** in [`go.mod`](go.mod)). Aptos officially recommends v2 for new projects; this bot stays on v1 for a stable path: load module ABI from the node (`EntryFunctionWithArgs`), `BuildTransaction`, sign with `SignedTransaction`, `SubmitTransaction`, then `WaitForTransaction` (poll timeout up to about **60s**, shorter if the request `context` has a deadline).

**Fullnode connection:** [`aptos.NewNodeClient`](aptos/client.go) takes `(fullnodeURL, apiKey, chainID)`. The API key is sent as `Authorization: Bearer …` when non-empty (from `NODE_API_KEY`, else `BEARER_TOKEN`). **`chainID`** comes from [`aptos.ChainIDForNetwork(cfg.Network)`](aptos/client.go): **testnet = 2**, **mainnet = 1**; any other network name uses **0** and the SDK may fetch chain id from the node.

**Signing account:** [`aptos.ParseAccount`](aptos/client.go) accepts raw hex (optional `0x`), **64-byte hex seed‖pubkey** (only the **first 32 bytes** are used as the Ed25519 seed), or AIP-80 `ed25519-priv-…`. **secp256k1** keys are rejected.

## Architecture

```
┌─ main.go ────────────────────────────────────────┐
│                                                   │
├─ Exchange Layer (exchange/)                       │
│  └─ exchange.Exchange interface                   │
│     └─ exchange/decibel: Decibel + Aptos impl    │
│        ├─ REST API client (api/)                 │
│        └─ Aptos chain client (aptos/)            │
│                                                   │
├─ Strategy Layer (strategy/)                       │
│  └─ MarketMaker: Inventory-skew cycle            │
│     └─ Only uses Exchange interface              │
│        (no knowledge of Decibel/Aptos)           │
│                                                   │
└─ Notification Layer (notify/)                     │
   └─ notify.Notifier interface                     │
      └─ notify/telegram: Telegram impl            │
         └─ Reads state via notify.InfoProvider    │
```

**Future extensibility**: Adding a new exchange (e.g., Binance) requires only implementing the `Exchange` interface in `exchange/binance/`. Adding Discord notifications requires only implementing `Notifier` in `notify/discord/`. The strategy layer and `main.go` remain unchanged.

## Telegram Notifications (Optional)

The bot can run Telegram commands and send inventory-limit alerts. Enable by setting:

```bash
export TG_BOT_TOKEN="<your-bot-token>"
export TG_ADMIN_ID="<your-user-id>"
export TG_ALERT_INVENTORY=true        # optional: true to enable inventory alerts
export TG_ALERT_INVENTORY_INTERVAL_MIN=30  # optional: minutes between alerts
```

Commands (sent by `/...` to the bot in Telegram):
- `/balance` — Account equity, available balance, margin usage
- `/gas` — Wallet APT balance
- `/positions` — Non-zero positions across markets (paged in one message; refresh resets to page 1); chain actions still use market addresses
- `/help` — Command list

Alerts: When `abs(inventory) ≥ MAX_INVENTORY`, the bot sends an alert with buttons to refresh or close the position. Auto-refresh period is configurable.

---

## Quick start

```bash
cp .env.example .env
# fill in the required values in .env
go run .
```

Or build a binary first:

```bash
go build -o decibel-mm .
./decibel-mm
```

---

## Configuration

Parameters are read in this priority order (highest wins):

```
CLI flag  >  environment variable  >  .env file  >  built-in default
```

You can mix all three — e.g. keep credentials in `.env` and tweak trading params via flags.

---

### Required — must be set before the bot will start

| Env var | CLI flag | Description |
|---------|----------|-------------|
| `BEARER_TOKEN` | `-bearer-token` | REST API bearer token from the Decibel dashboard |
| `SUBACCOUNT_ADDRESS` | `-subaccount` | Your subaccount object address (`0x…`) |
| `PRIVATE_KEY` | `-private-key` | Ed25519: hex (optional `0x`), 64-byte hex **seed‖pubkey** (first 32 bytes used as seed), or AIP-80 `ed25519-priv-…`. **Avoid passing via CLI in production** (shell history, `ps`). |
| `NODE_API_KEY` | `-node-api-key` | Fullnode API key; sent as `Authorization: Bearer`. Defaults to `BEARER_TOKEN` when unset. |

The **GlobalPerpEngine** object address is **not configurable**: it is always derived from `PACKAGE_ADDRESS` (including the value set by `-network`) for logging only; it is not passed as a transaction argument.

---

### Network

| Env var | CLI flag | Default | Description |
|---------|----------|---------|-------------|
| `NETWORK` | `-network` | `testnet` | Network preset: `testnet` or `mainnet`. Sets the REST API base URL, fullnode URL, Move package address, and the **Aptos chain id** used by the SDK client (**2** / **1**). |

**Testnet defaults**
- REST API: `https://api.testnet.aptoslabs.com/decibel/api/v1`
- Fullnode: `https://api.testnet.aptoslabs.com/v1`
- Package: `0xe7da2794b1d8af76532ed95f38bfdf1136abfd8ea3a240189971988a83101b7f`

**Mainnet defaults**
- REST API: `https://api.mainnet.aptoslabs.com/decibel/api/v1`
- Fullnode: `https://api.mainnet.aptoslabs.com/v1`
- Package: `0x50ead22afd6ffd9769e3b3d6e0e64a2a350d68e8b102c4e72e33d0b8cfdfdb06`

---

### Trading parameters

| Env var | CLI flag | Default | Description |
|---------|----------|---------|-------------|
| `MARKET_NAME` | `-market-name` | `BTC/USD` | Market to trade. Use the symbol shown on the Decibel UI (e.g. `BTC/USD`, `ETH/USD`). |
| `SPREAD` | `-spread` | `0.001` | Total bid-ask spread as a fraction of mid price. `0.001` = 0.1%. |
| `ORDER_SIZE` | `-order-size` | `0.001` | Base-asset units to quote on each side per cycle (e.g. 0.001 BTC). |
| `MAX_INVENTORY` | `-max-inventory` | `0.005` | Stop quoting new orders when `abs(position) ≥ this`. |
| `SKEW_PER_UNIT` | `-skew-per-unit` | `0.0001` | Extra half-spread added per 1.0 unit of net inventory (inventory skew coefficient). Positive inventory shifts quotes down; negative shifts them up. |
| `MAX_MARGIN_USAGE` | `-max-margin-usage` | `0.5` | Pause quoting when `cross_margin_ratio > this` (0–1). `0.5` = pause above 50% margin usage. |
| `REFRESH_INTERVAL` | `-refresh-interval` | `20.0` | Seconds to sleep between full quote cycles. |
| `COOLDOWN_S` | `-cooldown-s` | `1.5` | Seconds to wait between placing the bid and the ask within a single cycle. |
| `CANCEL_RESYNC_S` | `-cancel-resync-s` | `8.0` | Seconds to wait before re-fetching open orders after a cancel fails. |
| `AUTO_FLATTEN` | `-auto-flatten` | `false` | When `true`, automatically place a reduce-only GTC order to cut inventory when `MAX_INVENTORY` is hit. |
| `FLATTEN_AGGRESSION` | `-flatten-aggression` | `0.001` | Price offset from mid for the flatten order, as a fraction. `0.001` = 0.1% through mid. |
| `DRY_RUN` | `-dry-run` | `false` | Log all actions without submitting any on-chain transactions. Use this to verify configuration before going live. |

---

### Adaptive spread (auto-tuning)

When enabled, the bot automatically adjusts spread based on fill activity.

| Env var | CLI flag | Default | Description |
|---------|----------|---------|-------------|
| `AUTO_SPREAD` | `-auto-spread` | `false` | When `true`, automatically narrow the spread after `SPREAD_NO_FILL_CYCLES` consecutive cycles with no fill. Also widens slightly (up to the initial `SPREAD`) when a fill is detected. When `false`, only logs a suggestion without changing anything. |
| `SPREAD_MIN` | `-spread-min` | `0.0004` | Minimum spread the auto-adjuster will narrow down to (fraction). **Do not set below 0.0004 (0.04%) to avoid posting at a loss on volatile markets.** |
| `SPREAD_MAX` | `-spread-max` | `0.02` | Maximum spread the auto-adjuster will widen up to (fraction). Also used as the reset ceiling on fill. |
| `SPREAD_NO_FILL_CYCLES` | `-spread-no-fill-cycles` | `3` | Number of consecutive cycles with no detected fill before narrowing spread by one step. |
| `SPREAD_STEP` | `-spread-step` | `0.0002` | Amount (fraction) to narrow spread per adjustment step. On fill, widens by `SPREAD_STEP * 0.5`. |

---

### Optional overrides

These override the values set by `NETWORK`. Leave unset to use the network profile defaults.

| Env var | CLI flag | Description |
|---------|----------|-------------|
| `REST_API_BASE` | `-api-base` | Decibel REST API base URL |
| `APTOS_FULLNODE_URL` | `-fullnode-url` | Aptos-compatible fullnode URL |
| `PACKAGE_ADDRESS` | `-package-address` | Move package address |
| `MARKET_ADDR` | _(env only)_ | Skip market discovery and use this PerpMarket object address directly. |

---

### Telegram notifications (optional)

| Env var | CLI flag | Default | Description |
|---------|----------|---------|-------------|
| `TG_BOT_TOKEN` | `-tg-token` | _(unset)_ | Telegram bot token. When unset or empty, Telegram is disabled. **Prefer `.env`**; CLI values are visible in `ps`. |
| `TG_ADMIN_ID` | `-tg-admin-id` | _(unset)_ | Your Telegram user ID (numeric). Telegram is only enabled when **both** `TG_BOT_TOKEN` and `TG_ADMIN_ID` are set. **Prefer `.env`**; CLI values are visible in `ps`. |
| `TG_ALERT_INVENTORY` | `-tg-alert-inventory` | `false` | Enable automated alerts when `abs(inventory) ≥ MAX_INVENTORY`. |
| `TG_ALERT_INVENTORY_INTERVAL_MIN` | `-tg-alert-interval` | `30` | Minutes between repeated inventory-limit alerts. |
| `TG_STRICT_START` | `-tg-strict-start` | `false` | When Telegram is enabled, **exit the process** if bot init, API ready check (`getMe`), or `setMyCommands` fails. Default: log a warning and run the market maker without Telegram. |

---

## Ways to pass parameters

### 1. `.env` file (recommended for credentials)

```bash
cp .env.example .env
# edit .env
go run .
```

### 2. Environment variables

```bash
export BEARER_TOKEN=xxx
export PRIVATE_KEY=0xabc...
go run .
```

### 3. CLI flags (recommended for one-off overrides)

Changing `-network` after other defaults were loaded updates REST/fullnode/package **from the new preset** unless you already set the matching URL fields via **CLI** or **environment variables** (CLI wins, then env, then preset).

> **Note:** Go's `flag` package uses a single ASCII dash (`-`). For **non-boolean** flags, both `-spread 0.001` and `-spread=0.001` work.

#### Boolean flags

The boolean CLI flags are: `-auto-flatten`, `-dry-run`, `-auto-spread`, `-tg-alert-inventory`, `-tg-strict-start` (same set as `boolCLIFlagNames` in [`config/config.go`](config/config.go)).

| Form | Meaning |
|------|---------|
| `-dry-run` | Sets the flag to `true` (flag present) |
| `-dry-run=true` / `-dry-run=false` | **Recommended** in scripts; safe in any order with other flags |
| `-auto-flatten false` then more flags | For the flags listed above, `config.Load` rewrites `-name <bool-token>` to `-name=<value>` before `flag.Parse`, so **`-auto-flatten false -spread 0.002` works** when the second token is a boolean literal (`true`, `false`, `0`, `1`, `yes`, `no`, …). |

If the token after a boolean flag is **not** a boolean literal (e.g. `-auto-flatten 0.002`), standard `flag` parsing can still stop early and **skip later flags**. Use the right flag for numbers (e.g. `-flatten-aggression` for flatten price offset).

```bash
go run . \
  -network mainnet \
  -market-name ETH/USD \
  -spread 0.002 \
  -order-size 0.01 \
  -dry-run
```

### 4. Mix

Credentials in `.env`, trading params as flags:

```bash
# .env has BEARER_TOKEN, PRIVATE_KEY, SUBACCOUNT_ADDRESS
go run . -spread 0.002 -order-size 0.01
```

---

## Examples

```bash
# Dry run on testnet with defaults
go run . -dry-run

# Live on mainnet, BTC/USD, tighter spread
go run . \
  -network mainnet \
  -spread 0.0005 \
  -order-size 0.002 \
  -max-inventory 0.01

# Use a custom fullnode (e.g. your own node)
go run . -fullnode-url https://my-node.example.com/v1

# Explicit booleans (script-friendly) + spread
go run . -auto-flatten=false -spread 0.0005 -dry-run
```
