package engine

import (
	"testing"

	"github.com/bujih/decibel-mm-go/internal/models"
	"github.com/shopspring/decimal"
	"go.uber.org/zap"
)

func TestPositionManagerUpdateFromWS(t *testing.T) {
	pm := NewPositionManager(zap.NewNop())

	pm.UpdateFromWS(models.PositionUpdate{
		MarketAddr:    "m1",
		PositionAmt:   "1.5",
		EntryPrice:    "1000",
		Leverage:      10,
		MarginType:    "cross",
		UnrealizedPnl: "50",
	})

	pos := pm.GetPosition("m1")
	if pos == nil {
		t.Fatal("expected position")
	}
	if !pos.PositionAmt.Equal(decimal.NewFromFloat(1.5)) {
		t.Fatalf("expected amt 1.5, got %s", pos.PositionAmt.String())
	}
	if !pos.IsLong() {
		t.Fatal("expected long")
	}

	// Close position
	pm.UpdateFromWS(models.PositionUpdate{
		MarketAddr:  "m1",
		PositionAmt: "0",
	})
	if pm.HasPosition("m1") {
		t.Fatal("expected no position after zero amt update")
	}
}

func TestPositionManagerUpdateFromTradeFill(t *testing.T) {
	pm := NewPositionManager(zap.NewNop())

	// Open long
	pm.UpdateFromTradeFill(models.TradeFill{
		MarketAddr: "m1",
		Price:      "1000",
		Size:       "1",
		IsBuy:      true,
	})

	pos := pm.GetPosition("m1")
	if pos == nil || !pos.PositionAmt.Equal(decimal.NewFromInt(1)) {
		t.Fatalf("expected long 1, got %v", pos)
	}

	// Add to long
	pm.UpdateFromTradeFill(models.TradeFill{
		MarketAddr: "m1",
		Price:      "1100",
		Size:       "1",
		IsBuy:      true,
	})
	pos = pm.GetPosition("m1")
	expectedEntry := decimal.NewFromInt(1050) // (1000+1100)/2
	if !pos.EntryPrice.Equal(expectedEntry) {
		t.Fatalf("expected entry %s, got %s", expectedEntry.String(), pos.EntryPrice.String())
	}

	// Close
	pm.UpdateFromTradeFill(models.TradeFill{
		MarketAddr: "m1",
		Price:      "1200",
		Size:       "2",
		IsBuy:      false,
	})
	if pm.HasPosition("m1") {
		t.Fatal("expected position closed")
	}
}
