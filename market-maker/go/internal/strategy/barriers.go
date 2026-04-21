package strategy

import (
	"time"

	"decibel-mm-bot/internal/config"
	"decibel-mm-bot/internal/models"
	"github.com/shopspring/decimal"
)

// TripleBarrier implements stop-loss, take-profit, time-limit and trailing stop
type TripleBarrier struct {
	stopLoss       decimal.Decimal
	takeProfit     decimal.Decimal
	timeLimit      time.Duration
	trailingStop   *config.TrailingStopCfg
	positionOpenTime time.Time
	trailingTrigger decimal.Decimal
}

// NewTripleBarrier creates a barrier tracker
func NewTripleBarrier(stopLoss, takeProfit decimal.Decimal, timeLimit time.Duration, trailing *config.TrailingStopCfg) *TripleBarrier {
	return &TripleBarrier{
		stopLoss:     stopLoss,
		takeProfit:   takeProfit,
		timeLimit:    timeLimit,
		trailingStop: trailing,
	}
}

// RecordOpen records when a position was opened
func (tb *TripleBarrier) RecordOpen(t time.Time) {
	tb.positionOpenTime = t
	tb.trailingTrigger = decimal.Zero
}

// CheckStopLoss returns true if stop loss should trigger
func (tb *TripleBarrier) CheckStopLoss(pnlPct decimal.Decimal) bool {
	if tb.stopLoss.IsZero() {
		return false
	}
	return pnlPct.LessThanOrEqual(tb.stopLoss.Neg())
}

// CheckTakeProfit returns true if take profit should trigger
func (tb *TripleBarrier) CheckTakeProfit(pnlPct decimal.Decimal) bool {
	if tb.takeProfit.IsZero() {
		return false
	}
	return pnlPct.GreaterThanOrEqual(tb.takeProfit)
}

// CheckTimeLimit returns true if time limit exceeded
func (tb *TripleBarrier) CheckTimeLimit(now time.Time) bool {
	if tb.timeLimit == 0 || tb.positionOpenTime.IsZero() {
		return false
	}
	return now.Sub(tb.positionOpenTime) >= tb.timeLimit
}

// CheckTrailingStop evaluates trailing stop and returns true if triggered
func (tb *TripleBarrier) CheckTrailingStop(pnlPct decimal.Decimal) bool {
	if tb.trailingStop == nil {
		return false
	}
	if tb.trailingStop.ActivationPrice.IsZero() || tb.trailingStop.TrailingDelta.IsZero() {
		return false
	}

	if tb.trailingTrigger.IsZero() {
		// Not yet activated
		if pnlPct.GreaterThan(tb.trailingStop.ActivationPrice) {
			tb.trailingTrigger = pnlPct.Sub(tb.trailingStop.TrailingDelta)
		}
		return false
	}

	// Update trigger if PnL moved higher
	newTrigger := pnlPct.Sub(tb.trailingStop.TrailingDelta)
	if newTrigger.GreaterThan(tb.trailingTrigger) {
		tb.trailingTrigger = newTrigger
	}

	// Check if PnL fell below trigger
	if pnlPct.LessThan(tb.trailingTrigger) {
		return true
	}
	return false
}

// ComputePnLPct computes unrealized PnL percentage for a position
func ComputePnLPct(pos *models.LocalPosition, currentPrice decimal.Decimal) decimal.Decimal {
	if pos == nil || pos.PositionAmt.IsZero() || pos.EntryPrice.IsZero() {
		return decimal.Zero
	}
	if pos.IsLong() {
		return currentPrice.Sub(pos.EntryPrice).Div(pos.EntryPrice)
	}
	return pos.EntryPrice.Sub(currentPrice).Div(pos.EntryPrice)
}
