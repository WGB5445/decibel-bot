// Package notify defines the abstract notification interface that decouples
// alerting/monitoring from the market-making strategy and exchange.
package notify

import (
	"context"

	"decibel-mm-bot/botstate"
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

	// FlattenPosition places a reduce-only order to close the current position.
	FlattenPosition(ctx context.Context) error

	// GasBalance returns the gas token balance: amount, unit symbol, error.
	GasBalance(ctx context.Context) (float64, string, error)

	// WalletAddress returns the exchange wallet address (for display).
	WalletAddress() string

	// MaxInventory returns the configured inventory limit.
	MaxInventory() float64
}
