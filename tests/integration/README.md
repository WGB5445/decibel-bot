# Decibel Testnet Integration Tests

These tests execute real on-chain transactions against Decibel testnet.
They are gated behind the `integration` build tag and do **not** run with the normal unit-test suite.

## Required Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `TESTNET_API_KEY` | Decibel API bearer token | *(required)* |
| `TESTNET_PRIVATE_KEY` | Aptos wallet private key (hex or AIP-80) | *(required)* |
| `TESTNET_SUBACCOUNT` | Subaccount address to trade with | *(required)* |
| `TESTNET_MARKET` | Market address to trade on | *(required)* |
| `TESTNET_MARKET_NAME` | Human-readable market name | `APT-PERP` |
| `TESTNET_PACKAGE_ADDR` | Decibel Move package address on testnet | `0xe7da2794b1d8af76532ed95f38bfdf1136abfd8ea3a240189971988a83101b7f` |
| `TESTNET_FULLNODE_URL` | Aptos fullnode URL | `https://api.testnet.aptoslabs.com/v1` |
| `TESTNET_REST_BASE_URL` | Decibel REST API base URL | `https://api.testnet.aptoslabs.com/decibel` |

## Running

```bash
# Run all integration tests
go test -tags=integration ./tests/integration/ -v -count=1

# Run a specific test
go test -tags=integration ./tests/integration/ -v -count=1 -run TestIntegration_PlaceAndCancelOrder
```

## What the Tests Do

1. **ConnectAndBalance** – verifies the Aptos node client can connect and the signing wallet has non-zero APT for gas.
2. **PlaceAndCancelOrder** – places a tiny POST_ONLY limit order, extracts the `order_id` from transaction events, then cancels it.
3. **PlaceBulkOrdersAndCancelAll** – syncs the bulk sequence number from REST, places one bid + one ask via `place_bulk_orders_to_subaccount`, then cancels all orders for the market.

## Safety Notes

- Tests use **real funds** (testnet APT/USDC). Ensure the wallet has sufficient gas.
- Orders are placed at prices far from the current mid to minimize the chance of accidental fills.
- Each test cleans up after itself by canceling orders.
