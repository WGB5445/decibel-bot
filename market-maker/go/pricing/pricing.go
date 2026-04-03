// Package pricing implements the pure inventory-skew quote computation.
// No I/O — suitable for unit testing in isolation.
package pricing

import (
	"errors"
	"fmt"
	"math"
)

// ErrSpreadTooTight is returned when the spread cannot fit even one tick.
var ErrSpreadTooTight = errors.New("spread too tight")

// Quotes is the output of a successful ComputeQuotes call.
type Quotes struct {
	Bid  float64
	Ask  float64
	Size float64
}

// ComputeQuotes computes POST-ONLY bid/ask quotes with inventory skew.
//
// Algorithm:
//
//	half_spread = spread / 2
//	skew        = inventory * skewPerUnit   // positive → shift quotes DOWN
//
//	raw_bid = mid * (1 − half_spread − skew)
//	raw_ask = mid * (1 + half_spread − skew)
//
//	bid = floor(raw_bid / tick) * tick     // always round DOWN
//	ask =  ceil(raw_ask / tick) * tick     // always round UP
//
//	if ask ≤ bid: ask = bid + tick         // enforce minimum post-round spread
//
//	size = round(orderSize / lot) * lot
//
// Returns (nil, nil) when quoting should stop (inventory at limit, or size rounds to zero).
// Returns (nil, err) when parameters are invalid (spread too tight, non-positive mid/tick/lot).
func ComputeQuotes(
	midPrice, inventory, spread, skewPerUnit, maxInventory,
	tickSize, lotSize, minSize, orderSize float64,
) (*Quotes, error) {
	// ── Parameter validation ──────────────────────────────────────────────────
	if midPrice <= 0 {
		return nil, fmt.Errorf("midPrice must be positive, got %g", midPrice)
	}
	if tickSize <= 0 {
		return nil, fmt.Errorf("tickSize must be positive, got %g", tickSize)
	}
	if lotSize <= 0 {
		return nil, fmt.Errorf("lotSize must be positive, got %g", lotSize)
	}

	// Spread must be wide enough for at least one tick.
	minSpread := tickSize / midPrice
	if spread < minSpread {
		return nil, fmt.Errorf("%w: spread %g < minimum %g (tick=%g / mid=%g)",
			ErrSpreadTooTight, spread, minSpread, tickSize, midPrice)
	}

	// ── Inventory guard ───────────────────────────────────────────────────────
	if math.Abs(inventory) >= maxInventory {
		return nil, nil
	}

	// ── Core pricing ──────────────────────────────────────────────────────────
	halfSpread := spread / 2.0
	skew := inventory * skewPerUnit // positive inventory → push quotes down

	rawBid := midPrice * (1.0 - halfSpread - skew)
	rawAsk := midPrice * (1.0 + halfSpread - skew)

	bid := math.Floor(rawBid/tickSize) * tickSize // round DOWN
	ask := math.Ceil(rawAsk/tickSize) * tickSize  // round UP

	// Enforce minimum spread after rounding.
	if ask <= bid {
		ask = bid + tickSize
	}

	// ── Size ─────────────────────────────────────────────────────────────────
	size := math.Round(orderSize/lotSize) * lotSize
	if size <= 0 || size < minSize {
		return nil, nil
	}

	return &Quotes{Bid: bid, Ask: ask, Size: size}, nil
}
