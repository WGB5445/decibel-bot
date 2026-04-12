package strategy

import (
	"context"
	"testing"

	"decibel-mm-bot/api"
	"decibel-mm-bot/config"
	"decibel-mm-bot/exchange"
)

// ── Mock exchange ─────────────────────────────────────────────────────────────

type mockExchange struct {
	state           exchange.StateSnapshot
	placed          []exchange.PlaceOrderRequest
	bulkBids        []exchange.BulkOrderEntry
	bulkAsks        []exchange.BulkOrderEntry
	bulkCancelCalls int
}

func (m *mockExchange) FindMarket(_ context.Context, _ string) (*exchange.MarketConfig, error) {
	return nil, nil
}
func (m *mockExchange) MarketsCatalog(_ context.Context) ([]api.MarketConfig, error) {
	return nil, nil
}
func (m *mockExchange) FetchState(_ context.Context) (*exchange.StateSnapshot, error) {
	snap := m.state
	return &snap, nil
}
func (m *mockExchange) FetchOpenOrders(_ context.Context) ([]exchange.OpenOrder, error) {
	return nil, nil
}
func (m *mockExchange) PlaceOrder(_ context.Context, req exchange.PlaceOrderRequest) (exchange.PlaceOrderOutcome, error) {
	m.placed = append(m.placed, req)
	return exchange.PlaceOrderOutcome{TxHash: "0xmock"}, nil
}
func (m *mockExchange) FetchTradeHistory(_ context.Context, _ api.TradeHistoryParams) ([]api.TradeHistoryItem, error) {
	return nil, nil
}
func (m *mockExchange) PlaceBulkOrders(_ context.Context, bids, asks []exchange.BulkOrderEntry) error {
	m.bulkBids = append(m.bulkBids, bids...)
	m.bulkAsks = append(m.bulkAsks, asks...)
	return nil
}
func (m *mockExchange) CancelBulkOrders(_ context.Context) error {
	m.bulkCancelCalls++
	return nil
}
func (m *mockExchange) CancelOrder(_ context.Context, _ string) error { return nil }
func (m *mockExchange) WalletAddress() string                         { return "0xtest" }
func (m *mockExchange) DryRun() bool                                  { return false }
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

// ── Tests ─────────────────────────────────────────────────────────────────────

// First cycle must not trigger adaptive spread adjustments (no previous inventory to compare).
func TestFirstCycleSkipsSpreadAdjustment(t *testing.T) {
	cfg := testConfig()
	ex := &mockExchange{state: exchange.StateSnapshot{
		Inventory: 0.005, // large change from zero — would trigger fill if not first cycle
		Mid:       ptr(100_000),
	}}

	mm := New(cfg, ex, testMarket())
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
	ex := &mockExchange{state: exchange.StateSnapshot{Inventory: 0.0, Mid: ptr(100_000)}}

	mm := New(cfg, ex, testMarket())
	// Pre-narrow spread below cfg.Spread so there is room to widen.
	mm.effectiveSpread = cfg.Spread - cfg.SpreadStep

	// Cycle 1 — first cycle, baseline only.
	if err := mm.runCycle(context.Background()); err != nil {
		t.Fatalf("cycle 1: %v", err)
	}

	// Cycle 2 — inventory jumps (>> lotSize × 0.5) → fill detected.
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
	cfg.Spread = 0.001
	cfg.SpreadStep = 0.001 // large step to force the cap
	ex := &mockExchange{state: exchange.StateSnapshot{Inventory: 0.0, Mid: ptr(100_000)}}

	mm := New(cfg, ex, testMarket())
	mm.effectiveSpread = 0.0005 // below cap so widen can fire

	if err := mm.runCycle(context.Background()); err != nil {
		t.Fatal(err)
	}
	ex.state.Inventory = 0.001
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
	ex := &mockExchange{state: exchange.StateSnapshot{Inventory: 0.0, Mid: ptr(100_000)}}

	mm := New(cfg, ex, testMarket())

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
	ex := &mockExchange{state: exchange.StateSnapshot{Inventory: 0.0, Mid: ptr(100_000)}}

	mm := New(cfg, ex, testMarket())

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
	cfg.SpreadStep = 1.0 // huge step to force immediate floor
	cfg.SpreadMin = 0.001
	ex := &mockExchange{state: exchange.StateSnapshot{Inventory: 0.0, Mid: ptr(100_000)}}

	mm := New(cfg, ex, testMarket())

	if err := mm.runCycle(context.Background()); err != nil {
		t.Fatal(err)
	}
	if err := mm.runCycle(context.Background()); err != nil {
		t.Fatal(err)
	}

	if mm.effectiveSpread < cfg.SpreadMin {
		t.Errorf("spread %.6f dropped below SpreadMin %.6f", mm.effectiveSpread, cfg.SpreadMin)
	}
}

// At max inventory, CancelBulkOrders is called and no new quotes are placed.
func TestAtMaxInventoryCancelsBulkAndSkipsQuotes(t *testing.T) {
	cfg := testConfig()
	cfg.MaxInventory = 0.001
	cfg.AutoFlatten = false
	ex := &mockExchange{state: exchange.StateSnapshot{
		Inventory: 0.002, // exceeds MaxInventory
		Mid:       ptr(100_000),
	}}

	mm := New(cfg, ex, testMarket())
	if err := mm.runCycle(context.Background()); err != nil {
		t.Fatalf("runCycle: %v", err)
	}

	if ex.bulkCancelCalls == 0 {
		t.Error("expected CancelBulkOrders to be called at max inventory")
	}
	if len(ex.bulkBids)+len(ex.bulkAsks) != 0 {
		t.Errorf("expected no new bulk quotes at max inventory, got %d bids %d asks",
			len(ex.bulkBids), len(ex.bulkAsks))
	}
}

// Repeated max-inventory cycles only call CancelBulkOrders once (invLimitBulkCancelDone guard).
func TestAtMaxInventoryCancelsBulkOnlyOnce(t *testing.T) {
	cfg := testConfig()
	cfg.MaxInventory = 0.001
	cfg.AutoFlatten = false
	ex := &mockExchange{state: exchange.StateSnapshot{
		Inventory: 0.002,
		Mid:       ptr(100_000),
	}}

	mm := New(cfg, ex, testMarket())
	for i := 0; i < 3; i++ {
		if err := mm.runCycle(context.Background()); err != nil {
			t.Fatalf("cycle %d: %v", i+1, err)
		}
	}

	if ex.bulkCancelCalls != 1 {
		t.Errorf("CancelBulkOrders should be called exactly once while inventory stays at limit, got %d", ex.bulkCancelCalls)
	}
}

// Normal cycle places exactly one bulk bid and one bulk ask via PlaceBulkOrders.
func TestNormalCyclePlacesBulkBidAndAsk(t *testing.T) {
	cfg := testConfig()
	ex := &mockExchange{state: exchange.StateSnapshot{Inventory: 0.0, Mid: ptr(100_000)}}

	mm := New(cfg, ex, testMarket())
	if err := mm.runCycle(context.Background()); err != nil {
		t.Fatalf("runCycle: %v", err)
	}

	if len(ex.bulkBids) != 1 {
		t.Errorf("expected 1 bulk bid entry, got %d", len(ex.bulkBids))
	}
	if len(ex.bulkAsks) != 1 {
		t.Errorf("expected 1 bulk ask entry, got %d", len(ex.bulkAsks))
	}
	if ex.bulkBids[0].Price >= ex.bulkAsks[0].Price {
		t.Errorf("bid price %.2f must be < ask price %.2f",
			ex.bulkBids[0].Price, ex.bulkAsks[0].Price)
	}
}
