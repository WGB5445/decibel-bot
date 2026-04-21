package models

import (
	"time"

	"github.com/shopspring/decimal"
)

// LocalPosition represents a tracked position
type LocalPosition struct {
	MarketAddr      string
	PositionAmt     decimal.Decimal // positive=long, negative=short
	EntryPrice      decimal.Decimal
	Leverage        int
	MarginType      string // isolated / cross
	UnrealizedPnl   decimal.Decimal
	FundingPnl      decimal.Decimal
	LiquidationPrice decimal.Decimal
	UpdatedAt       time.Time
}

// IsLong returns true if position is long
func (p *LocalPosition) IsLong() bool {
	return p.PositionAmt.GreaterThan(decimal.Zero)
}

// IsShort returns true if position is short
func (p *LocalPosition) IsShort() bool {
	return p.PositionAmt.LessThan(decimal.Zero)
}

// IsFlat returns true if no position
func (p *LocalPosition) IsFlat() bool {
	return p.PositionAmt.IsZero()
}

// Notional returns the absolute notional size
func (p *LocalPosition) Notional() decimal.Decimal {
	return p.PositionAmt.Abs().Mul(p.EntryPrice)
}
