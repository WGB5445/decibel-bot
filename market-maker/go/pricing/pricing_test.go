package pricing_test

import (
	"errors"
	"math"
	"testing"

	"decibel-mm-bot/pricing"
)

// Default test parameters.
const (
	mid     = 100_000.0
	spread  = 0.001 // 0.1 %
	tick    = 1.0
	lot     = 0.00001
	minSz   = 0.00002
	orderSz = 0.001
	skew    = 0.0001
	maxInv  = 0.005
	eps     = 1e-9
)

func mustQuote(t *testing.T, inventory float64) *pricing.Quotes {
	t.Helper()
	q, err := pricing.ComputeQuotes(mid, inventory, spread, skew, maxInv, tick, lot, minSz, orderSz)
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if q == nil {
		t.Fatal("expected non-nil quotes")
	}
	return q
}

func almostEq(a, b float64) bool {
	return math.Abs(a-b) < eps
}

// ── Basic correctness ─────────────────────────────────────────────────────────

func TestZeroInventorySymmetric(t *testing.T) {
	q := mustQuote(t, 0)
	// half_spread=0.0005 → raw_bid=99950, raw_ask=100050
	if !almostEq(q.Bid, 99_950.0) {
		t.Errorf("bid = %g, want 99950", q.Bid)
	}
	if !almostEq(q.Ask, 100_050.0) {
		t.Errorf("ask = %g, want 100050", q.Ask)
	}
	if !almostEq(q.Size, orderSz) {
		t.Errorf("size = %g, want %g", q.Size, orderSz)
	}
}

func TestAskAlwaysGreaterThanBid(t *testing.T) {
	inventories := []float64{-0.004, -0.002, -0.001, 0, 0.001, 0.002, 0.004}
	for _, inv := range inventories {
		q := mustQuote(t, inv)
		if q.Ask <= q.Bid {
			t.Errorf("inv=%g: ask (%g) must be > bid (%g)", inv, q.Ask, q.Bid)
		}
	}
}

func TestPositiveInventoryShiftsQuotesDown(t *testing.T) {
	q0 := mustQuote(t, 0)
	ql := mustQuote(t, 0.001)
	if ql.Bid > q0.Bid {
		t.Errorf("bid should shift down with positive inventory: %g vs %g", ql.Bid, q0.Bid)
	}
	if ql.Ask > q0.Ask {
		t.Errorf("ask should shift down with positive inventory: %g vs %g", ql.Ask, q0.Ask)
	}
}

func TestNegativeInventoryShiftsQuotesUp(t *testing.T) {
	q0 := mustQuote(t, 0)
	qs := mustQuote(t, -0.001)
	if qs.Bid < q0.Bid {
		t.Errorf("bid should shift up with negative inventory: %g vs %g", qs.Bid, q0.Bid)
	}
	if qs.Ask < q0.Ask {
		t.Errorf("ask should shift up with negative inventory: %g vs %g", qs.Ask, q0.Ask)
	}
}

func TestBidIsMultipleOfTick(t *testing.T) {
	q := mustQuote(t, 0)
	if !almostEq(math.Mod(q.Bid, tick), 0) {
		t.Errorf("bid %g is not a multiple of tick %g", q.Bid, tick)
	}
}

func TestAskIsMultipleOfTick(t *testing.T) {
	q := mustQuote(t, 0)
	if !almostEq(math.Mod(q.Ask, tick), 0) {
		t.Errorf("ask %g is not a multiple of tick %g", q.Ask, tick)
	}
}

func TestSizeIsMultipleOfLot(t *testing.T) {
	q := mustQuote(t, 0)
	remainder := math.Abs(math.Round(q.Size/lot)*lot - q.Size)
	if remainder > eps {
		t.Errorf("size %g is not a multiple of lot %g (remainder %g)", q.Size, lot, remainder)
	}
}

// ── None returns ──────────────────────────────────────────────────────────────

func TestMaxInventoryLongReturnsNil(t *testing.T) {
	q, err := pricing.ComputeQuotes(mid, maxInv, spread, skew, maxInv, tick, lot, minSz, orderSz)
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if q != nil {
		t.Errorf("expected nil quotes at max inventory, got %+v", q)
	}
}

func TestMaxInventoryShortReturnsNil(t *testing.T) {
	q, err := pricing.ComputeQuotes(mid, -maxInv, spread, skew, maxInv, tick, lot, minSz, orderSz)
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if q != nil {
		t.Errorf("expected nil quotes at max inventory, got %+v", q)
	}
}

func TestSizeBelowMinReturnsNil(t *testing.T) {
	// orderSize much smaller than minSize → nil
	q, err := pricing.ComputeQuotes(mid, 0, spread, skew, maxInv, tick, lot, 1.0 /*min=1BTC*/, 0.0001)
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if q != nil {
		t.Errorf("expected nil quotes when size < minSize, got %+v", q)
	}
}

func TestOrderSizeRoundsToZeroReturnsNil(t *testing.T) {
	// orderSize < lot/2 → rounds to 0 → nil
	q, err := pricing.ComputeQuotes(mid, 0, spread, skew, maxInv, tick, 0.01, 0.001, 0.004)
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if q != nil {
		t.Errorf("expected nil when size rounds to zero, got %+v", q)
	}
}

// ── Error returns ─────────────────────────────────────────────────────────────

func TestSpreadTooTightReturnsError(t *testing.T) {
	tooTight := (tick / mid) * 0.5
	_, err := pricing.ComputeQuotes(mid, 0, tooTight, skew, maxInv, tick, lot, minSz, orderSz)
	if !errors.Is(err, pricing.ErrSpreadTooTight) {
		t.Errorf("expected ErrSpreadTooTight, got %v", err)
	}
}

func TestZeroMidPriceReturnsError(t *testing.T) {
	_, err := pricing.ComputeQuotes(0, 0, spread, skew, maxInv, tick, lot, minSz, orderSz)
	if err == nil {
		t.Error("expected error for zero midPrice")
	}
}

func TestZeroTickSizeReturnsError(t *testing.T) {
	_, err := pricing.ComputeQuotes(mid, 0, spread, skew, maxInv, 0, lot, minSz, orderSz)
	if err == nil {
		t.Error("expected error for zero tickSize")
	}
}

func TestZeroLotSizeReturnsError(t *testing.T) {
	_, err := pricing.ComputeQuotes(mid, 0, spread, skew, maxInv, tick, 0, minSz, orderSz)
	if err == nil {
		t.Error("expected error for zero lotSize")
	}
}

// ── Rounding edge case ────────────────────────────────────────────────────────

func TestMinimumSpreadEnforcedAfterRounding(t *testing.T) {
	// tick=10, mid=1000 → min_spread=0.01; use spread=0.011 (just above min)
	q, err := pricing.ComputeQuotes(1000, 0, 0.011, 0, 10.0, 10.0, 0.001, 0.001, 0.001)
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}
	if q == nil {
		t.Fatal("expected non-nil quotes")
	}
	if q.Ask < q.Bid+10.0 {
		t.Errorf("ask (%g) must be >= bid (%g) + tick (10)", q.Ask, q.Bid)
	}
}

// ── Skew ──────────────────────────────────────────────────────────────────────

func TestSkewSymmetry(t *testing.T) {
	// Inventory +x and −x should produce bid/ask that are mirror-images of each other.
	qPos := mustQuote(t, 0.001)
	qNeg := mustQuote(t, -0.001)
	if qPos.Bid > qNeg.Bid {
		t.Errorf("positive inventory bid (%g) should be <= negative inventory bid (%g)",
			qPos.Bid, qNeg.Bid)
	}
	if qPos.Ask > qNeg.Ask {
		t.Errorf("positive inventory ask (%g) should be <= negative inventory ask (%g)",
			qPos.Ask, qNeg.Ask)
	}
}

// ── Table-driven subtests ─────────────────────────────────────────────────────

func TestComputeQuotes_Subtests(t *testing.T) {
	tests := []struct {
		name      string
		inventory float64
		wantNil   bool
		wantErr   bool
	}{
		{"zero_inventory", 0, false, false},
		{"small_long", 0.001, false, false},
		{"small_short", -0.001, false, false},
		{"just_below_max", maxInv - lot, false, false},
		{"at_max_long", maxInv, true, false},
		{"at_max_short", -maxInv, true, false},
		{"beyond_max", maxInv * 2, true, false},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			q, err := pricing.ComputeQuotes(mid, tt.inventory, spread, skew, maxInv, tick, lot, minSz, orderSz)
			if (err != nil) != tt.wantErr {
				t.Fatalf("error = %v, wantErr = %v", err, tt.wantErr)
			}
			if (q == nil) != tt.wantNil {
				t.Fatalf("quotes = %v, wantNil = %v", q, tt.wantNil)
			}
			if q != nil {
				if q.Ask <= q.Bid {
					t.Errorf("ask (%g) must be > bid (%g)", q.Ask, q.Bid)
				}
				if q.Size < minSz {
					t.Errorf("size (%g) must be >= minSize (%g)", q.Size, minSz)
				}
			}
		})
	}
}
