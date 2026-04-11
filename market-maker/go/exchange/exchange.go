// Package exchange defines the abstract exchange interface that decouples the
// market-making strategy from any specific exchange implementation.
package exchange

import (
	"context"

	"decibel-mm-bot/api"
)

// Exchange abstracts all exchange interactions needed by the market-maker strategy.
// Each exchange (Decibel, Binance, dYdX, ...) provides its own implementation.
type Exchange interface {
	// FindMarket discovers market metadata by name (e.g. "BTC/USD").
	FindMarket(ctx context.Context, name string) (*MarketConfig, error)

	// MarketsCatalog returns all markets from the venue REST catalog (GET /markets on Decibel).
	// Implementations may cache the result for the process lifetime after the first success.
	MarketsCatalog(ctx context.Context) ([]api.MarketConfig, error)

	// FetchState fetches the full state snapshot for the current cycle
	// (account overview, positions, orders, mid price — in parallel where possible).
	FetchState(ctx context.Context) (*StateSnapshot, error)

	// FetchOpenOrders fetches open orders (used for resync after cancel failures).
	FetchOpenOrders(ctx context.Context) ([]OpenOrder, error)

	// PlaceOrder places an order on the exchange.
	// On VM success returns PlaceOrderOutcome (TxHash; OrderID when parsed from events).
	PlaceOrder(ctx context.Context, req PlaceOrderRequest) (PlaceOrderOutcome, error)

	// FetchTradeHistory queries Decibel REST GET /trade_history.
	FetchTradeHistory(ctx context.Context, p api.TradeHistoryParams) ([]api.TradeHistoryItem, error)

	// PlaceBulkOrders atomically replaces all bulk quotes for the target market.
	// bids and asks are price levels (POST_ONLY). An empty slice clears that side.
	// Passing empty slices for both sides is equivalent to CancelBulkOrders.
	PlaceBulkOrders(ctx context.Context, bids, asks []BulkOrderEntry) error

	// CancelBulkOrders removes all bulk quotes for the target market in one transaction.
	CancelBulkOrders(ctx context.Context) error

	// CancelOrder cancels a single order by ID.
	// Returns nil when the order was already gone (not-found is success).
	CancelOrder(ctx context.Context, orderID string) error

	// WalletAddress returns the signing wallet address (for display in notifications).
	WalletAddress() string

	// GasBalance returns the gas token balance: amount, unit symbol, error.
	GasBalance(ctx context.Context) (float64, string, error)

	// DryRun reports whether the exchange is in dry-run mode (log-only, no real txns).
	DryRun() bool
}
