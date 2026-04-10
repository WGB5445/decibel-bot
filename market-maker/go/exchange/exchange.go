// Package exchange defines the abstract exchange interface that decouples the
// market-making strategy from any specific exchange implementation.
package exchange

import "context"

// Exchange abstracts all exchange interactions needed by the market-maker strategy.
// Each exchange (Decibel, Binance, dYdX, ...) provides its own implementation.
type Exchange interface {
	// FindMarket discovers market metadata by name (e.g. "BTC/USD").
	FindMarket(ctx context.Context, name string) (*MarketConfig, error)

	// FetchState fetches the full state snapshot for the current cycle
	// (account overview, positions, orders, mid price — in parallel where possible).
	FetchState(ctx context.Context) (*StateSnapshot, error)

	// FetchOpenOrders fetches open orders (used for resync after cancel failures).
	FetchOpenOrders(ctx context.Context) ([]OpenOrder, error)

	// PlaceOrder places an order on the exchange.
	PlaceOrder(ctx context.Context, req PlaceOrderRequest) error

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
