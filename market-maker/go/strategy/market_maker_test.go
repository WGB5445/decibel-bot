package strategy

import (
	"context"
	"testing"

	"decibel-mm-bot/config"
	"decibel-mm-bot/exchange"
)

// ── Mock exchange ─────────────────────────────────────────────────────────────

type mockExchange struct {
	state      exchange.StateSnapshot
	openOrders []exchange.OpenOrder // returned by FetchOpenOrders (used in resync)
	placed     []exchange.PlaceOrderRequest
	cancelled  []string
}

func (m *mockExchange) FindMarket(_ context.Context, _ string) (*exchange.MarketConfig, error) {
	return nil, nil
}
func (m *mockExchange) FetchState(_ context.Context) (*exchange.StateSnapshot, error) {
	snap := m.state
	return &snap, nil
}
func (m *mockExchange) FetchOpenOrders(_ context.Context) ([]exchange.OpenOrder, error) {
	return m.openOrders, nil
}
func (m *mockExchange) PlaceOrder(_ context.Context, req exchange.PlaceOrderRequest) error {
	m.placed = append(m.placed, req)
	return nil
}
func (m *mockExchange) CancelOrder(_ context.Context, id string) error {
	m.cancelled = append(m.cancelled, id)
	return nil
}
func (m *mockExchange) WalletAddress() string         { return "0xtest" }
func (m *mockExchange) DryRun() bool                  { return false }
func (m *mockExchange) GasBalance(_ context.Context) (float64, string, error) {
	return 1.0, "APT", nil
}

// ── Helpers ───────────────────────────────────────────────────────────────────

func ptr(f float64) *float64 { return &f }

func testMarket() *exchange.MarketConfig {
	return &exchange.MarketConfig{
		MarketID:   "0xmarket",
		MarketName: "BTC/USD",
		TickSize:   1.0,
		LotSize:    0.00001,
		MinSize:    0.00001,
	}
}

func testConfig() *config.Config {
	return &config.Config{
		Spread:             0.002,
		SpreadStep:         0.0002,
		SpreadMin:          0.0005,
		SpreadNoFillCycles: 3,
		AutoSpread:         true,
		OrderSize:          0.001,
		MaxInventory:       0.01,
		SkewPerUnit:        0.0001,
		MaxMarginUsage:     0.9,
		CooldownS:          0.0,
		CancelResyncS:      0.0,
	}
}

// runOneCycle drives one cycle with the given inventory and returns the updated MarketMaker.
func newMM(cfg *config.Config, ex exchange.Exchange) *MarketMaker {
	return New(cfg, ex, testMarket())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

// First cycle must not trigger adaptive spread adjustments (no previous inventory to compare).
func TestFirstCycleSkipsSpreadAdjustment(t *testing.T) {
	cfg := testConfig()
	ex := &mockExchange{state: exchange.StateSnapshot{
		Inventory: 0.005, // large inventory change from zero — would trigger fill if not first cycle
		Mid:       ptr(100_000),
	}}

	mm := newMM(cfg, ex)
	initialSpread := mm.effectiveSpread

	if err := mm.runCycle(context.Background()); err != nil {
		t.Fatalf("runCycle: %v", err)
	}

	if mm.effectiveSpread != initialSpread {
		t.Errorf("first cycle should not change spread: got %.6f, want %.6f",
			mm.effectiveSpread, initialSpread)
	}
	if mm.firstCycle {
		t.Error("firstCycle flag should be cleared after first cycle")
	}
}

// Fill detected (inventory changes by > lotSize × 0.5) → spread widens.
func TestFillDetectedWidensSpread(t *testing.T) {
	cfg := testConfig()
	ex := &mockExchange{state: exchange.StateSnapshot{
		Inventory: 0.0, // will be set per cycle
		Mid:       ptr(100_000),
	}}

	mm := newMM(cfg, ex)
	// Pre-narrow spread below cfg.Spread so there is room to widen.
	mm.effectiveSpread = cfg.Spread - cfg.SpreadStep

	// Cycle 1 — baseline inventory = 0 (first cycle, no adjustment)
	if err := mm.runCycle(context.Background()); err != nil {
		t.Fatalf("cycle 1: %v", err)
	}

	// Cycle 2 — inventory jumps by 0.001 (>> lotSize × 0.5 = 0.000005) → fill
	before := mm.effectiveSpread
	ex.state.Inventory = 0.001
	if err := mm.runCycle(context.Background()); err != nil {
		t.Fatalf("cycle 2: %v", err)
	}

	if mm.effectiveSpread <= before {
		t.Errorf("spread should widen on fill: before=%.6f after=%.6f", before, mm.effectiveSpread)
	}
	if mm.noFillCycles != 0 {
		t.Errorf("noFillCycles should reset to 0 on fill, got %d", mm.noFillCycles)
	}
}

// Spread must not exceed the initial cfg.Spread even after repeated fills.
func TestFillSpreadCappedAtInitial(t *testing.T) {
	cfg := testConfig()
	cfg.Spread = 0.001       // initial = cap
	cfg.SpreadStep = 0.001   // large step to force the cap
	ex := &mockExchange{state: exchange.StateSnapshot{Mid: ptr(100_000)}}

	mm := newMM(cfg, ex)
	// Manually set spread below cap so widen fires.
	mm.effectiveSpread = 0.0005

	// First cycle sets baseline.
	if err := mm.runCycle(context.Background()); err != nil {
		t.Fatal(err)
	}
	// Second cycle: big fill.
	ex.state.Inventory = 0.01
	if err := mm.runCycle(context.Background()); err != nil {
		t.Fatal(err)
	}

	if mm.effectiveSpread > cfg.Spread {
		t.Errorf("spread %.6f exceeded cap %.6f", mm.effectiveSpread, cfg.Spread)
	}
}

// After SpreadNoFillCycles with no fill, AutoSpread=true narrows the spread.
func TestNoFillNarrowsSpreadWhenAutoSpread(t *testing.T) {
	cfg := testConfig()
	cfg.SpreadNoFillCycles = 2
	cfg.AutoSpread = true
	ex := &mockExchange{state: exchange.StateSnapshot{
		Inventory: 0.0,
		Mid:       ptr(100_000),
	}}

	mm := newMM(cfg, ex)

	// Cycle 1 — first cycle, baseline only.
	if err := mm.runCycle(context.Background()); err != nil {
		t.Fatal(err)
	}

	spreadBefore := mm.effectiveSpread

	// Cycles 2 and 3 — no inventory change → no fill.
	for i := 0; i < cfg.SpreadNoFillCycles; i++ {
		if err := mm.runCycle(context.Background()); err != nil {
			t.Fatalf("cycle %d: %v", i+2, err)
		}
	}

	if mm.effectiveSpread >= spreadBefore {
		t.Errorf("spread should narrow after %d no-fill cycles: before=%.6f after=%.6f",
			cfg.SpreadNoFillCycles, spreadBefore, mm.effectiveSpread)
	}
}

// After SpreadNoFillCycles with no fill, AutoSpread=false must NOT mutate the spread.
func TestNoFillDoesNotMutateSpreadWhenAutoSpreadOff(t *testing.T) {
	cfg := testConfig()
	cfg.SpreadNoFillCycles = 2
	cfg.AutoSpread = false
	ex := &mockExchange{state: exchange.StateSnapshot{
		Inventory: 0.0,
		Mid:       ptr(100_000),
	}}

	mm := newMM(cfg, ex)

	// Run enough cycles to trigger the no-fill suggestion path.
	for i := 0; i < cfg.SpreadNoFillCycles+2; i++ {
		if err := mm.runCycle(context.Background()); err != nil {
			t.Fatalf("cycle %d: %v", i+1, err)
		}
	}

	if mm.effectiveSpread != cfg.Spread {
		t.Errorf("AutoSpread=false must not mutate spread: got %.6f, want %.6f",
			mm.effectiveSpread, cfg.Spread)
	}
}

// Spread must not drop below SpreadMin when narrowing.
func TestSpreadFlooredAtSpreadMin(t *testing.T) {
	cfg := testConfig()
	cfg.SpreadNoFillCycles = 1
	cfg.AutoSpread = true
	cfg.SpreadStep = 1.0  // huge step to force immediate floor
	cfg.SpreadMin = 0.001
	ex := &mockExchange{state: exchange.StateSnapshot{
		Inventory: 0.0,
		Mid:       ptr(100_000),
	}}

	mm := newMM(cfg, ex)

	// First cycle: baseline.
	if err := mm.runCycle(context.Background()); err != nil {
		t.Fatal(err)
	}
	// Second cycle: no fill → triggers narrowing.
	if err := mm.runCycle(context.Background()); err != nil {
		t.Fatal(err)
	}

	if mm.effectiveSpread < cfg.SpreadMin {
		t.Errorf("spread %.6f dropped below SpreadMin %.6f", mm.effectiveSpread, cfg.SpreadMin)
	}
}

// At max inventory with quotes=nil the bot cancels all orders and does NOT place new ones.
func TestAtMaxInventoryCancelsAndSkipsQuotes(t *testing.T) {
	cfg := testConfig()
	cfg.MaxInventory = 0.001
	cfg.AutoFlatten = false
	mid := 100_000.0
	ex := &mockExchange{state: exchange.StateSnapshot{
		Inventory:  0.002, // exceeds MaxInventory
		Mid:        &mid,
		OpenOrders: []exchange.OpenOrder{{OrderID: "order-1", MarketID: "0xmarket"}},
	}}

	mm := newMM(cfg, ex)

	if err := mm.runCycle(context.Background()); err != nil {
		t.Fatalf("runCycle: %v", err)
	}

	if len(ex.cancelled) == 0 {
		t.Error("expected existing orders to be cancelled at max inventory")
	}
	if len(ex.placed) != 0 {
		t.Errorf("expected no new orders at max inventory, got %d", len(ex.placed))
	}
}

// Normal cycle places exactly one bid (IsBuy=true) and one ask (IsBuy=false).
func TestNormalCyclePlacesBidAndAsk(t *testing.T) {
	cfg := testConfig()
	ex := &mockExchange{state: exchange.StateSnapshot{
		Inventory: 0.0,
		Mid:       ptr(100_000),
	}}

	mm := newMM(cfg, ex)
	if err := mm.runCycle(context.Background()); err != nil {
		t.Fatalf("runCycle: %v", err)
	}

	bids := 0
	asks := 0
	for _, o := range ex.placed {
		if o.IsBuy {
			bids++
		} else {
			asks++
		}
	}
	if bids != 1 || asks != 1 {
		t.Errorf("expected 1 bid and 1 ask, got %d bids and %d asks", bids, asks)
	}
	for _, o := range ex.placed {
		if o.TimeInForce != 1 {
			t.Errorf("orders must be POST_ONLY (TimeInForce=1), got %d", o.TimeInForce)
		}
	}
}
