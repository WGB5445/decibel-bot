# Decibel Market Maker Bot (Go)

A Go-based market making bot for [Decibel](https://docs.decibel.trade/), a fully on-chain perpetual DEX built on the Aptos blockchain.

## Features

- **On-chain order placement** via `aptos-go-sdk`
- **Bulk Order support** for atomic market-making updates
- **REST API integration** for market data, positions, and order queries
- **WebSocket streaming** for real-time order book, price, and account updates
- **Hummingbot-style perpetual market making strategy**:
  - Bid/ask spread around reference price
  - Multiple order levels
  - Order refresh with tolerance
  - Profit taking and stop loss
  - Triple Barrier (stop loss, take profit, time limit, trailing stop)
- **Transaction tracking** for async Aptos transaction confirmation

## Project Structure

```
decibel-mm-go/
├── cmd/decibel-bot/       # Entry point
├── internal/
│   ├── config/            # Configuration loading and validation
│   ├── decibel/           # Decibel REST client, WS client, on-chain write client
│   ├── engine/            # Event bus, order manager, position manager, tx tracker
│   ├── models/            # Shared types (events, orders, positions)
│   ├── pkg/decimal/       # Decimal helpers for 9-decimal precision
│   └── strategy/          # Market making strategy and pricing logic
├── configs/
│   └── config.yaml        # Example configuration
├── Makefile
└── go.mod
```

## Quick Start

### 1. Prerequisites

- Go 1.21+
- Decibel API credentials:
  - `API_WALLET_PRIVATE_KEY` (from app.decibel.trade/api)
  - `API_WALLET_ADDRESS`
  - `SUBACCOUNT_ADDRESS`
  - `API_BEARER_TOKEN` (from Geomi/Aptos Build)

### 2. Configure

Copy and edit `configs/config.yaml`:

```yaml
decibel:
  bearer_token: "your-bearer-token"
  api_wallet_private_key: "0x..."
  api_wallet_address: "0x..."
  subaccount_address: "0x..."
  market_name: "BTC-USD"
```

### 3. Build

```bash
make build
```

### 4. Run

```bash
make run
# or with custom config
DECIBEL_CONFIG=configs/config.yaml ./bin/decibel-bot
```

## Configuration Reference

| Key | Description | Default |
|-----|-------------|---------|
| `env` | Environment: `testnet`, `mainnet` | `testnet` |
| `strategy.bid_spread` | Bid spread from mid price | `0.01` (1%) |
| `strategy.ask_spread` | Ask spread from mid price | `0.01` (1%) |
| `strategy.order_amount` | Base order size | `0.1` |
| `strategy.order_levels` | Number of order levels per side | `1` |
| `strategy.use_bulk_orders` | Use bulk order tx for MM updates | `true` |
| `strategy.post_only` | Use PostOnly time-in-force | `true` |
| `strategy.stop_loss` | Triple barrier stop loss | `0.03` |
| `strategy.take_profit` | Triple barrier take profit | `0.02` |

## Architecture

The bot follows an **event-driven architecture**:

1. **WebSocket Client** streams depth, prices, order updates, and positions into the **EventBus**.
2. **Scheduler** generates periodic `EventTick` to drive the strategy loop.
3. **Strategy** (`DecibelMM`) computes target orders and submits them via the **WriteClient**.
4. **TxTracker** polls the Aptos node for transaction confirmation and publishes `EventTxConfirmed`.
5. **OrderManager** and **PositionManager** maintain local shadow state synchronized with on-chain events.

## Testing

```bash
make test
```

## Roadmap

- [x] Project scaffolding and config
- [x] REST read client
- [x] Aptos write client (single orders)
- [x] WebSocket client skeleton
- [x] Event bus, order manager, position manager, tx tracker
- [x] Strategy state machine and pricing logic
- [x] Bulk orders on-chain implementation
- [ ] Full WebSocket message parsing and event mapping
- [ ] Integration tests on testnet
- [ ] Risk guard and Prometheus metrics
- [ ] Vault integration

## Disclaimer

This is experimental software for automated trading on a decentralized exchange. Use at your own risk. Always test thoroughly on testnet before deploying with real funds.
