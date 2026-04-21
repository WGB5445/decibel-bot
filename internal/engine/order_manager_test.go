package engine

import (
	"testing"

	"github.com/bujih/decibel-mm-go/internal/models"
	"github.com/shopspring/decimal"
	"go.uber.org/zap"
)

func TestOrderManagerLifecycle(t *testing.T) {
	om := NewOrderManager(zap.NewNop())

	// Add pending
	om.AddPending("cid-1", "tx-1", models.SideBuy, decimal.NewFromFloat(100), decimal.NewFromFloat(1), false)
	if !om.HasOpenOrders() {
		t.Fatal("expected open orders")
	}

	// Confirm
	om.ConfirmOrder("cid-1", "oid-1")
	o := om.GetOrder("oid-1")
	if o == nil {
		t.Fatal("expected order after confirm")
	}
	if o.Status != models.OrderStatusNew {
		t.Fatalf("expected NEW, got %s", o.Status)
	}

	// WS partial fill
	om.UpdateFromWS(models.OrderUpdate{
		OrderID:       "oid-1",
		Status:        "PARTIALLY_FILLED",
		FilledSize:    "0.5",
		AvgPrice:      "100",
		RemainingSize: "0.5",
	})
	o = om.GetOrder("oid-1")
	if !o.FilledSize.Equal(decimal.NewFromFloat(0.5)) {
		t.Fatalf("expected filled 0.5, got %s", o.FilledSize.String())
	}

	// WS full fill
	om.UpdateFromWS(models.OrderUpdate{
		OrderID:    "oid-1",
		Status:     "FILLED",
		FilledSize: "1",
		AvgPrice:   "100",
	})
	if om.HasOpenOrders() {
		t.Fatal("expected no open orders after fill")
	}
}

func TestOrderManagerCancel(t *testing.T) {
	om := NewOrderManager(zap.NewNop())
	om.AddPending("cid-2", "tx-2", models.SideSell, decimal.NewFromFloat(200), decimal.NewFromFloat(2), false)
	om.ConfirmOrder("cid-2", "oid-2")

	om.CancelOrder("oid-2")
	if om.HasOpenOrders() {
		t.Fatal("expected no open orders after cancel")
	}
}
