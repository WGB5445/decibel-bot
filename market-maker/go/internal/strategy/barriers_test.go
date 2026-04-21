package strategy

import (
	"testing"
	"time"

	"decibel-mm-bot/internal/config"
	"decibel-mm-bot/internal/models"
	"github.com/shopspring/decimal"
)

func TestCheckStopLoss(t *testing.T) {
	tb := NewTripleBarrier(
		decimal.NewFromFloat(0.03),
		decimal.NewFromFloat(0.02),
		0,
		nil,
	)

	if !tb.CheckStopLoss(decimal.NewFromFloat(-0.05)) {
		t.Fatal("expected stop loss to trigger at -5%")
	}
	if tb.CheckStopLoss(decimal.NewFromFloat(-0.01)) {
		t.Fatal("expected stop loss NOT to trigger at -1%")
	}
}

func TestCheckTakeProfit(t *testing.T) {
	tb := NewTripleBarrier(
		decimal.NewFromFloat(0.03),
		decimal.NewFromFloat(0.02),
		0,
		nil,
	)

	if !tb.CheckTakeProfit(decimal.NewFromFloat(0.03)) {
		t.Fatal("expected take profit to trigger at 3%")
	}
	if tb.CheckTakeProfit(decimal.NewFromFloat(0.01)) {
		t.Fatal("expected take profit NOT to trigger at 1%")
	}
}

func TestCheckTimeLimit(t *testing.T) {
	tb := NewTripleBarrier(
		decimal.Zero,
		decimal.Zero,
		5*time.Second,
		nil,
	)
	tb.RecordOpen(time.Now().Add(-10 * time.Second))
	if !tb.CheckTimeLimit(time.Now()) {
		t.Fatal("expected time limit to trigger after 10s")
	}
}

func TestTrailingStop(t *testing.T) {
	tb := NewTripleBarrier(
		decimal.Zero,
		decimal.Zero,
		0,
		&config.TrailingStopCfg{
			ActivationPrice: decimal.NewFromFloat(0.02),
			TrailingDelta:   decimal.NewFromFloat(0.005),
		},
	)

	// Not activated yet
	if tb.CheckTrailingStop(decimal.NewFromFloat(0.01)) {
		t.Fatal("expected trailing stop NOT to trigger before activation")
	}

	// Activate at 3%
	if tb.CheckTrailingStop(decimal.NewFromFloat(0.03)) {
		t.Fatal("expected trailing stop NOT to trigger right after activation")
	}

	// Drop back below trigger (3% - 0.5% = 2.5%)
	if !tb.CheckTrailingStop(decimal.NewFromFloat(0.024)) {
		t.Fatal("expected trailing stop to trigger when dropping below trailing trigger")
	}
}

func TestComputePnLPct(t *testing.T) {
	pos := &models.LocalPosition{
		PositionAmt: decimal.NewFromFloat(1),
		EntryPrice:  decimal.NewFromFloat(10000),
	}

	pnl := ComputePnLPct(pos, decimal.NewFromFloat(11000))
	expected := decimal.NewFromFloat(0.1)
	if !pnl.Equal(expected) {
		t.Fatalf("long pnl = %s, want %s", pnl.String(), expected.String())
	}

	pos.PositionAmt = decimal.NewFromFloat(-1)
	pnl = ComputePnLPct(pos, decimal.NewFromFloat(9000))
	if !pnl.Equal(expected) {
		t.Fatalf("short pnl = %s, want %s", pnl.String(), expected.String())
	}
}
