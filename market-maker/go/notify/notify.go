// Package notify defines the abstract notification interface that decouples
// alerting/monitoring from the market-making strategy and exchange.
package notify

import (
	"context"
	"strings"

	"decibel-mm-bot/api"
	"decibel-mm-bot/botstate"
	"decibel-mm-bot/exchange"
)

// Notifier is a notification service that runs alongside the strategy.
type Notifier interface {
	// Run starts the notification service (blocking). Returns when ctx is cancelled.
	Run(ctx context.Context) error
}

// InfoProvider is the read-only interface that notification services use to
// query bot state and trigger actions. It bridges the strategy and exchange
// layers without the notification layer needing to know about either directly.
type InfoProvider interface {
	// GetSnapshot returns a consistent copy of the current bot state.
	GetSnapshot() botstate.Snapshot

	// FetchLiveSnapshot pulls a fresh exchange state for monitoring UI (e.g.
	// Telegram refresh). It does not call BotState.Update — VWAP/entry logic
	// stays owned by the strategy.
	FetchLiveSnapshot(ctx context.Context) (botstate.Snapshot, error)

	// FlattenPosition places a reduce-only POST_ONLY order to close the current position.
	// On VM success, PlaceOrderOutcome includes TxHash and OrderID when parsed from events.
	FlattenPosition(ctx context.Context) (exchange.PlaceOrderOutcome, error)

	// DryRun mirrors exchange dry-run mode (no chain transactions).
	DryRun() bool

	// FetchTradeHistoryByOrder queries GET /trade_history with account + market + order_id.
	FetchTradeHistoryByOrder(ctx context.Context, marketAddr, orderID string) ([]api.TradeHistoryItem, error)

	// FetchRecentTrades returns the latest fills for the configured target market (newest first).
	// The Telegram bot may request a larger limit (e.g. 100) to paginate fills client-side.
	FetchRecentTrades(ctx context.Context, limit int) ([]api.TradeHistoryItem, error)

	// GasBalance returns the gas token balance: amount, unit symbol, error.
	GasBalance(ctx context.Context) (float64, string, error)

	// WalletAddress returns the exchange wallet address (for display).
	WalletAddress() string

	// MaxInventory returns the configured inventory limit.
	MaxInventory() float64

	// MarketDisplayName resolves a market address to a human-readable name from
	// the cached /markets catalog; falls back to a shortened address when unknown.
	MarketDisplayName(marketAddr string) string
}

// ShortAddrForDisplay is a fallback label when MarketDisplayName has no mapping.
func ShortAddrForDisplay(addr string) string {
	s := strings.TrimSpace(addr)
	if s == "" {
		return ""
	}
	lower := strings.ToLower(s)
	lower = strings.TrimPrefix(lower, "0x")
	if len(lower) <= 12 {
		return "0x" + lower
	}
	return "0x" + lower[:6] + "…" + lower[len(lower)-4:]
}
