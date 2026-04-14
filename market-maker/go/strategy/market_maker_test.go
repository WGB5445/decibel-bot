package strategy

import (
	"context"
	"errors"
	"fmt"
	"testing"
	"time"

	"decibel-mm-bot/api"
	"decibel-mm-bot/config"
	"decibel-mm-bot/exchange"
)

// ── Mock exchange ─────────────────────────────────────────────────────────────

type mockExchange struct {
	state            exchange.StateSnapshot
	fetchErr         error // if non-nil, FetchState returns this error
	placed           []exchange.PlaceOrderRequest
	placedIDs        []string // order IDs returned by PlaceOrder, in order
	orderSeq         int      // incremented each PlaceOrder call; used to generate unique IDs
	bulkBids         []exchange.BulkOrderEntry
	bulkAsks         []exchange.BulkOrderEntry
	bulkCancelCalls  int
	cancelOrderCalls int
	bulkCancelErr    error // if non-nil, CancelBulkOrders returns this error
	bulkOrderErr     error // if non-nil, PlaceBulkOrders returns this error
}

func (m *mockExchange) FindMarket(_ context.Context, _ string) (*exchange.MarketConfig, error) {
	return nil, nil
}
func (m *mockExchange) MarketsCatalog(_ context.Context) ([]api.MarketConfig, error) {
	return nil, nil
}
func (m *mockExchange) FetchState(_ context.Context) (*exchange.StateSnapshot, error) {
	if m.fetchErr != nil {
		return nil, m.fetchErr
	}
	snap := m.state
	return &snap, nil
}
func (m *mockExchange) FetchOpenOrders(_ context.Context) ([]exchange.OpenOrder, error) {
	return nil, nil
}
func (m *mockExchange) PlaceOrder(_ context.Context, req exchange.PlaceOrderRequest) (exchange.PlaceOrderOutcome, error) {
	m.orderSeq++
	id := fmt.Sprintf("order-%d", m.orderSeq)
	m.placed = append(m.placed, req)
	m.placedIDs = append(m.placedIDs, id)
	return exchange.PlaceOrderOutcome{TxHash: "0xmock", OrderID: id}, nil
}
func (m *mockExchange) FetchTradeHistory(_ context.Context, _ api.TradeHistoryParams) ([]api.TradeHistoryItem, error) {
	return nil, nil
}
func (m *mockExchange) PlaceBulkOrders(_ context.Context, bids, asks []exchange.BulkOrderEntry) error {
	if m.bulkOrderErr != nil {
		return m.bulkOrderErr
	}
	m.bulkBids = append(m.bulkBids, bids...)
	m.bulkAsks = append(m.bulkAsks, asks...)
	return nil
}
func (m *mockExchange) CancelBulkOrders(_ context.Context) error {
	m.bulkCancelCalls++
	if m.bulkCancelErr != nil {
		return m.bulkCancelErr
	}
	return nil
}
func (m *mockExchange) CancelOrder(_ context.Context, id string) error {
	m.cancelOrderCalls++
	var kept []exchange.OpenOrder
	for _, o := range m.state.OpenOrders {
		if o.OrderID != id {
			kept = append(kept, o)
		}
	}
	m.state.OpenOrders = kept
	return nil
}
func (m *mockExchange) WalletAddress() string { return "0xtest" }
func (m *mockExchange) DryRun() bool          { return false }
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
		Spread:                 0.002,
		SpreadStep:             0.0002,
		SpreadMin:              0.0005,
		SpreadNoFillCycles:     3,
		AutoSpread:             true,
		OrderSize:              0.001,
		MaxInventory:           0.01,
		SkewPerUnit:            0.0001,
		MaxMarginUsage:         0.9,
		ShutdownCancelTimeoutS: 60,
	}
}

// ── Tests: Adaptive Spread ────────────────────────────────────────────────────

// First cycle must not trigger adaptive spread adjustments (no previous inventory to compare).
func TestFirstCycleSkipsSpreadAdjustment(t *testing.T) {
	cfg := testConfig()
	ex := &mockExchange{state: exchange.StateSnapshot{
		Inventory: 0.005, // large change from zero — would trigger fill if not first cycle
		Mid:       ptr(100_000),
	}}

	mm := New(cfg, ex, testMarket())
	initialSpread := mm.effectiveSpread

	if err := mm.runCycle(context.Background(), 1); err != nil {
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
	if err := mm.runCycle(context.Background(), 1); err != nil {
		t.Fatalf("cycle 1: %v", err)
	}

	// Cycle 2 — inventory jumps (>> lotSize × 0.5) → fill detected.
	before := mm.effectiveSpread
	ex.state.Inventory = 0.001
	if err := mm.runCycle(context.Background(), 1); err != nil {
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

	if err := mm.runCycle(context.Background(), 1); err != nil {
		t.Fatal(err)
	}
	ex.state.Inventory = 0.001
	if err := mm.runCycle(context.Background(), 1); err != nil {
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
	if err := mm.runCycle(context.Background(), 1); err != nil {
		t.Fatal(err)
	}
	spreadBefore := mm.effectiveSpread

	// Cycles 2 and 3 — no inventory change → no fill.
	for i := 0; i < cfg.SpreadNoFillCycles; i++ {
		if err := mm.runCycle(context.Background(), 1); err != nil {
			t.Fatalf("cycle %d: %v", i+2, err)
		}
	}

	if mm.effectiveSpread >= spreadBefore {
		t.Errorf("spread should narrow after %d no-fill cycles: before=%.6f after=%.6f",
			cfg.SpreadNoFillCycles, spreadBefore, mm.effectiveSpread)
	}
}

// At max inventory the bot does not place bulk quotes; no-fill cycles must not narrow spread.
func TestNoFillDoesNotAccumulateAtMaxInventory(t *testing.T) {
	cfg := testConfig()
	cfg.MaxInventory = 0.001
	cfg.SpreadNoFillCycles = 2
	cfg.AutoSpread = true
	cfg.AutoFlatten = false

	ex := &mockExchange{state: exchange.StateSnapshot{
		Inventory: 0.002,
		Mid:       ptr(100_000),
	}}

	mm := New(cfg, ex, testMarket())
	spreadWant := cfg.Spread

	if err := mm.runCycle(context.Background(), 1); err != nil {
		t.Fatalf("cycle 1: %v", err)
	}
	for i := 0; i < 8; i++ {
		if err := mm.runCycle(context.Background(), 1); err != nil {
			t.Fatalf("cycle %d: %v", i+2, err)
		}
	}

	if mm.effectiveSpread != spreadWant {
		t.Errorf("at max inventory spread must stay %.6f, got %.6f", spreadWant, mm.effectiveSpread)
	}
	if mm.noFillCycles != 0 {
		t.Errorf("noFillCycles should stay 0 at max inventory, got %d", mm.noFillCycles)
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
		if err := mm.runCycle(context.Background(), 1); err != nil {
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

	if err := mm.runCycle(context.Background(), 1); err != nil {
		t.Fatal(err)
	}
	if err := mm.runCycle(context.Background(), 1); err != nil {
		t.Fatal(err)
	}

	if mm.effectiveSpread < cfg.SpreadMin {
		t.Errorf("spread %.6f dropped below SpreadMin %.6f", mm.effectiveSpread, cfg.SpreadMin)
	}
}

// ── Tests: Risk Guards ────────────────────────────────────────────────────────

// When MarginUsage exceeds MaxMarginUsage, the cycle must pause — no quotes placed.
func TestMarginGuardPausesQuoting(t *testing.T) {
	cfg := testConfig()
	cfg.MaxMarginUsage = 0.8
	ex := &mockExchange{state: exchange.StateSnapshot{
		Inventory:   0.0,
		MarginUsage: 0.95, // exceeds 0.8
		Mid:         ptr(100_000),
	}}

	mm := New(cfg, ex, testMarket())
	if err := mm.runCycle(context.Background(), 1); err != nil {
		t.Fatalf("runCycle: %v", err)
	}

	if len(ex.bulkBids)+len(ex.bulkAsks) != 0 {
		t.Errorf("expected no quotes placed when margin too high, got %d bids %d asks",
			len(ex.bulkBids), len(ex.bulkAsks))
	}
}

// When MarginUsage is below threshold, quotes are placed normally.
func TestMarginGuardAllowsQuotingBelowThreshold(t *testing.T) {
	cfg := testConfig()
	cfg.MaxMarginUsage = 0.8
	ex := &mockExchange{state: exchange.StateSnapshot{
		Inventory:   0.0,
		MarginUsage: 0.5, // below 0.8
		Mid:         ptr(100_000),
	}}

	mm := New(cfg, ex, testMarket())
	if err := mm.runCycle(context.Background(), 1); err != nil {
		t.Fatalf("runCycle: %v", err)
	}

	if len(ex.bulkBids) == 0 || len(ex.bulkAsks) == 0 {
		t.Error("expected quotes placed when margin is within threshold")
	}
}

// When mid-price is nil, the cycle must pause — no quotes placed.
func TestMidPriceMissingPausesQuoting(t *testing.T) {
	cfg := testConfig()
	ex := &mockExchange{state: exchange.StateSnapshot{
		Inventory: 0.0,
		Mid:       nil, // no price
	}}

	mm := New(cfg, ex, testMarket())
	if err := mm.runCycle(context.Background(), 1); err != nil {
		t.Fatalf("runCycle: %v", err)
	}

	if len(ex.bulkBids)+len(ex.bulkAsks) != 0 {
		t.Errorf("expected no quotes placed when mid-price missing, got %d bids %d asks",
			len(ex.bulkBids), len(ex.bulkAsks))
	}
}

// When FetchState returns an error, runCycle must return that error.
func TestStateFetchFailurePropagatesError(t *testing.T) {
	cfg := testConfig()
	fetchErr := errors.New("network timeout")
	ex := &mockExchange{fetchErr: fetchErr}

	mm := New(cfg, ex, testMarket())
	err := mm.runCycle(context.Background(), 1)
	if err == nil {
		t.Fatal("expected error from runCycle when FetchState fails, got nil")
	}
	if !errors.Is(err, fetchErr) {
		t.Errorf("expected error chain to contain %v, got %v", fetchErr, err)
	}
}

// ── Tests: Inventory Limit ────────────────────────────────────────────────────

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
	if err := mm.runCycle(context.Background(), 1); err != nil {
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
		if err := mm.runCycle(context.Background(), 1); err != nil {
			t.Fatalf("cycle %d: %v", i+1, err)
		}
	}

	if ex.bulkCancelCalls != 1 {
		t.Errorf("CancelBulkOrders should be called exactly once while inventory stays at limit, got %d", ex.bulkCancelCalls)
	}
}

// When inventory recovers below limit, the invLimitBulkCancelDone flag resets and
// the next time inventory hits the limit, CancelBulkOrders fires again.
func TestInventoryRecoveryReArmsFlag(t *testing.T) {
	cfg := testConfig()
	cfg.MaxInventory = 0.001
	cfg.AutoFlatten = false
	ex := &mockExchange{state: exchange.StateSnapshot{
		Inventory: 0.002,
		Mid:       ptr(100_000),
	}}

	mm := New(cfg, ex, testMarket())

	// Cycle 1: hit limit — first cancel fires.
	if err := mm.runCycle(context.Background(), 1); err != nil {
		t.Fatal(err)
	}
	if ex.bulkCancelCalls != 1 {
		t.Fatalf("expected 1 cancel after first limit hit, got %d", ex.bulkCancelCalls)
	}

	// Cycle 2: inventory recovers below limit.
	ex.state.Inventory = 0.0005
	if err := mm.runCycle(context.Background(), 1); err != nil {
		t.Fatal(err)
	}
	if mm.invLimitBulkCancelDone {
		t.Error("invLimitBulkCancelDone should be reset when inventory recovers")
	}

	// Cycle 3: inventory hits limit again — cancel should fire again.
	ex.state.Inventory = 0.002
	if err := mm.runCycle(context.Background(), 1); err != nil {
		t.Fatal(err)
	}
	if ex.bulkCancelCalls != 2 {
		t.Errorf("expected 2nd cancel after second limit hit, got %d total", ex.bulkCancelCalls)
	}
}

// ── Tests: Flatten Orders ─────────────────────────────────────────────────────

// When AutoFlatten=true and inventory is at limit, a flatten order is placed.
func TestFlattenOrderPlacedWhenAutoFlattenOn(t *testing.T) {
	cfg := testConfig()
	cfg.MaxInventory = 0.001
	cfg.AutoFlatten = true
	cfg.FlattenAggression = 0.001
	ex := &mockExchange{state: exchange.StateSnapshot{
		Inventory: 0.002,
		Mid:       ptr(100_000),
	}}

	mm := New(cfg, ex, testMarket())
	if err := mm.runCycle(context.Background(), 1); err != nil {
		t.Fatalf("runCycle: %v", err)
	}

	if len(ex.placed) == 0 {
		t.Error("expected a flatten PlaceOrder when AutoFlatten=true at max inventory")
	}
	if len(ex.placed) > 0 && !ex.placed[0].ReduceOnly {
		t.Error("flatten order must be ReduceOnly=true")
	}
	if len(ex.placed) > 0 && ex.placed[0].TimeInForce != exchange.TimeInForcePostOnly {
		t.Errorf("flatten order must use POST_ONLY (TimeInForce=%d), got %d", exchange.TimeInForcePostOnly, ex.placed[0].TimeInForce)
	}
}

// When AutoFlatten=false and inventory is at limit, no flatten order is placed.
func TestFlattenOrderSkippedWhenAutoFlattenOff(t *testing.T) {
	cfg := testConfig()
	cfg.MaxInventory = 0.001
	cfg.AutoFlatten = false
	ex := &mockExchange{state: exchange.StateSnapshot{
		Inventory: 0.002,
		Mid:       ptr(100_000),
	}}

	mm := New(cfg, ex, testMarket())
	if err := mm.runCycle(context.Background(), 1); err != nil {
		t.Fatalf("runCycle: %v", err)
	}

	if len(ex.placed) != 0 {
		t.Errorf("expected no flatten order when AutoFlatten=false, got %d PlaceOrder calls", len(ex.placed))
	}
}

// Long position (positive inventory) → flatten sells (IsBuy=false).
func TestFlattenLongPositionSells(t *testing.T) {
	cfg := testConfig()
	cfg.MaxInventory = 0.001
	cfg.AutoFlatten = true
	cfg.FlattenAggression = 0.001
	ex := &mockExchange{state: exchange.StateSnapshot{
		Inventory: 0.002, // long
		Mid:       ptr(100_000),
	}}

	mm := New(cfg, ex, testMarket())
	if err := mm.runCycle(context.Background(), 1); err != nil {
		t.Fatal(err)
	}

	if len(ex.placed) == 0 {
		t.Fatal("expected flatten order")
	}
	if ex.placed[0].IsBuy {
		t.Error("flatten for long position must be a sell (IsBuy=false)")
	}
	// Long flatten: POST_ONLY sell above mid (maker side vs mid proxy).
	if ex.placed[0].Price <= 100_000 {
		t.Errorf("flatten sell price %.2f should be above mid 100000", ex.placed[0].Price)
	}
}

// Short position (negative inventory) → flatten buys (IsBuy=true).
func TestFlattenShortPositionBuys(t *testing.T) {
	cfg := testConfig()
	cfg.MaxInventory = 0.001
	cfg.AutoFlatten = true
	cfg.FlattenAggression = 0.001
	ex := &mockExchange{state: exchange.StateSnapshot{
		Inventory: -0.002, // short
		Mid:       ptr(100_000),
	}}

	mm := New(cfg, ex, testMarket())
	if err := mm.runCycle(context.Background(), 1); err != nil {
		t.Fatal(err)
	}

	if len(ex.placed) == 0 {
		t.Fatal("expected flatten order")
	}
	if !ex.placed[0].IsBuy {
		t.Error("flatten for short position must be a buy (IsBuy=true)")
	}
	// Short flatten: POST_ONLY buy below mid (maker side vs mid proxy).
	if ex.placed[0].Price >= 100_000 {
		t.Errorf("flatten buy price %.2f should be below mid 100000", ex.placed[0].Price)
	}
}

// Flatten order is not placed when inventory is too small to round to MinSize.
func TestFlattenSkippedWhenInventoryTooSmall(t *testing.T) {
	cfg := testConfig()
	cfg.MaxInventory = 0.001
	cfg.AutoFlatten = true
	cfg.FlattenAggression = 0.001
	market := testMarket()
	market.MinSize = 0.1 // very large min size
	ex := &mockExchange{state: exchange.StateSnapshot{
		Inventory: 0.002, // too small to round to MinSize=0.1
		Mid:       ptr(100_000),
	}}

	mm := New(cfg, ex, market)
	if err := mm.runCycle(context.Background(), 1); err != nil {
		t.Fatal(err)
	}

	if len(ex.placed) != 0 {
		t.Errorf("expected no flatten order when inventory too small for MinSize, got %d", len(ex.placed))
	}
}

// ── Tests: Normal Quote Placement ─────────────────────────────────────────────

// Normal cycle places exactly one bulk bid and one bulk ask via PlaceBulkOrders.
func TestNormalCyclePlacesBulkBidAndAsk(t *testing.T) {
	cfg := testConfig()
	ex := &mockExchange{state: exchange.StateSnapshot{Inventory: 0.0, Mid: ptr(100_000)}}

	mm := New(cfg, ex, testMarket())
	if err := mm.runCycle(context.Background(), 1); err != nil {
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

// PlaceBulkOrders error causes runCycle to return an error.
func TestPlaceBulkOrdersErrorPropagates(t *testing.T) {
	cfg := testConfig()
	bulkErr := errors.New("on-chain tx failed")
	ex := &mockExchange{
		state:        exchange.StateSnapshot{Inventory: 0.0, Mid: ptr(100_000)},
		bulkOrderErr: bulkErr,
	}

	mm := New(cfg, ex, testMarket())
	err := mm.runCycle(context.Background(), 1)
	if err == nil {
		t.Fatal("expected error when PlaceBulkOrders fails, got nil")
	}
	if !errors.Is(err, bulkErr) {
		t.Errorf("expected error chain to contain %v, got %v", bulkErr, err)
	}
}

// Skew shifts bid and ask in the correct direction for a long position.
// Uses large SkewPerUnit (0.1) and sizeable inventory (0.1) so the skew
// (0.01 = 1%) is large enough to survive tick rounding.
func TestSkewShiftsBidAskForLongPosition(t *testing.T) {
	cfg := testConfig()
	cfg.SkewPerUnit = 0.1  // 10% per unit of inventory
	cfg.MaxInventory = 1.0 // allow larger inventory
	mid := 100_000.0

	// Cycle with zero inventory (baseline prices).
	ex := &mockExchange{state: exchange.StateSnapshot{Inventory: 0.0, Mid: ptr(mid)}}
	mm := New(cfg, ex, testMarket())
	if err := mm.runCycle(context.Background(), 1); err != nil {
		t.Fatal(err)
	}
	baseBid := ex.bulkBids[0].Price
	baseAsk := ex.bulkAsks[0].Price

	// Fresh bot with positive inventory (long) — quotes should be skewed down.
	// skew = 0.1 × 0.1 = 0.01 (1%), well above tick size.
	ex2 := &mockExchange{state: exchange.StateSnapshot{Inventory: 0.1, Mid: ptr(mid)}}
	mm2 := New(cfg, ex2, testMarket())
	if err := mm2.runCycle(context.Background(), 1); err != nil {
		t.Fatal(err)
	}
	skewBid := ex2.bulkBids[0].Price
	skewAsk := ex2.bulkAsks[0].Price

	if skewBid >= baseBid {
		t.Errorf("long position should push bid down: base=%.2f skewed=%.2f", baseBid, skewBid)
	}
	if skewAsk >= baseAsk {
		t.Errorf("long position should push ask down: base=%.2f skewed=%.2f", baseAsk, skewAsk)
	}
}

// Quotes are the same size regardless of inventory direction (symmetric sizing).
func TestQuoteSizeSymmetric(t *testing.T) {
	cfg := testConfig()
	mid := 100_000.0

	for _, inv := range []float64{-0.005, 0.0, 0.005} {
		ex := &mockExchange{state: exchange.StateSnapshot{Inventory: inv, Mid: ptr(mid)}}
		mm := New(cfg, ex, testMarket())
		if err := mm.runCycle(context.Background(), 1); err != nil {
			t.Fatalf("inv=%.4f: %v", inv, err)
		}
		if len(ex.bulkBids) == 0 || len(ex.bulkAsks) == 0 {
			t.Fatalf("inv=%.4f: expected bulk orders", inv)
		}
		if ex.bulkBids[0].Size != ex.bulkAsks[0].Size {
			t.Errorf("inv=%.4f: bid size %.5f != ask size %.5f",
				inv, ex.bulkBids[0].Size, ex.bulkAsks[0].Size)
		}
	}
}

// ── Tests: State Update ───────────────────────────────────────────────────────

// FlattenMaxDeviation caps the flatten sell price (long position) when FlattenAggression
// would place the order too far above mid.
func TestFlattenMaxDeviationCapsLongSellPrice(t *testing.T) {
	cfg := testConfig()
	cfg.MaxInventory = 0.001
	cfg.AutoFlatten = true
	cfg.FlattenAggression = 0.10   // 10% above mid — would be wide without cap
	cfg.FlattenMaxDeviation = 0.02 // cap at 2% above mid
	mid := 100_000.0
	ex := &mockExchange{state: exchange.StateSnapshot{
		Inventory: 0.002, // long
		Mid:       ptr(mid),
	}}

	mm := New(cfg, ex, testMarket())
	if err := mm.runCycle(context.Background(), 1); err != nil {
		t.Fatal(err)
	}
	if len(ex.placed) == 0 {
		t.Fatal("expected flatten order")
	}
	// Price must not be more than 2% above mid.
	maxAllowed := mid * (1.0 + cfg.FlattenMaxDeviation)
	if ex.placed[0].Price > maxAllowed {
		t.Errorf("flatten sell price %.2f exceeds FlattenMaxDeviation cap %.2f",
			ex.placed[0].Price, maxAllowed)
	}
}

// FlattenMaxDeviation floors the flatten buy price (short position) when FlattenAggression
// would place the order too far below mid.
func TestFlattenMaxDeviationCapsShortBuyPrice(t *testing.T) {
	cfg := testConfig()
	cfg.MaxInventory = 0.001
	cfg.AutoFlatten = true
	cfg.FlattenAggression = 0.10   // 10% below mid — would be wide without floor
	cfg.FlattenMaxDeviation = 0.02 // floor at 2% below mid
	mid := 100_000.0
	ex := &mockExchange{state: exchange.StateSnapshot{
		Inventory: -0.002, // short
		Mid:       ptr(mid),
	}}

	mm := New(cfg, ex, testMarket())
	if err := mm.runCycle(context.Background(), 1); err != nil {
		t.Fatal(err)
	}
	if len(ex.placed) == 0 {
		t.Fatal("expected flatten order")
	}
	// Price must not be more than 2% below mid.
	minAllowed := mid * (1.0 - cfg.FlattenMaxDeviation)
	if ex.placed[0].Price < minAllowed {
		t.Errorf("flatten buy price %.2f is below FlattenMaxDeviation floor %.2f",
			ex.placed[0].Price, minAllowed)
	}
}

// After tick rounding, price must still respect FlattenMaxDeviation (tick grid can
// otherwise push buys below the floor or sells above the cap).
func TestFlattenDeviationPostTickClampBuy(t *testing.T) {
	cfg := testConfig()
	cfg.FlattenAggression = 0.0025 // 100*(1-0.0025)=99.75 → Floor to tick 1 gives 99
	cfg.FlattenMaxDeviation = 0.003
	mid := 100.0
	mkt := &exchange.MarketConfig{
		MarketID: "0xmarket", MarketName: "TEST/USD",
		TickSize: 1.0, LotSize: 0.00001, MinSize: 0.00001,
	}
	ex := &mockExchange{}
	mm := New(cfg, ex, mkt)
	ctx := context.Background()
	if _, err := mm.placeFlattenOrder(ctx, -0.002, mid); err != nil {
		t.Fatalf("placeFlattenOrder: %v", err)
	}
	if len(ex.placed) != 1 {
		t.Fatalf("expected 1 placed order, got %d", len(ex.placed))
	}
	minAllowed := mid * (1.0 - cfg.FlattenMaxDeviation)
	if ex.placed[0].Price < minAllowed {
		t.Errorf("buy price %.2f below deviation floor %.2f after tick clamp", ex.placed[0].Price, minAllowed)
	}
}

func TestFlattenDeviationPostTickClampSell(t *testing.T) {
	cfg := testConfig()
	cfg.FlattenAggression = 0.0025 // 100*(1+0.0025)=100.25 → Ceil to tick 1 gives 101
	cfg.FlattenMaxDeviation = 0.003
	mid := 100.0
	mkt := &exchange.MarketConfig{
		MarketID: "0xmarket", MarketName: "TEST/USD",
		TickSize: 1.0, LotSize: 0.00001, MinSize: 0.00001,
	}
	ex := &mockExchange{}
	mm := New(cfg, ex, mkt)
	ctx := context.Background()
	if _, err := mm.placeFlattenOrder(ctx, 0.002, mid); err != nil {
		t.Fatalf("placeFlattenOrder: %v", err)
	}
	if len(ex.placed) != 1 {
		t.Fatalf("expected 1 placed order, got %d", len(ex.placed))
	}
	maxAllowed := mid * (1.0 + cfg.FlattenMaxDeviation)
	if ex.placed[0].Price > maxAllowed {
		t.Errorf("sell price %.2f above deviation cap %.2f after tick clamp", ex.placed[0].Price, maxAllowed)
	}
}

// When FlattenMaxDeviation=0, no cap is applied.
func TestFlattenMaxDeviationZeroDisablesCap(t *testing.T) {
	cfg := testConfig()
	cfg.MaxInventory = 0.001
	cfg.AutoFlatten = true
	cfg.FlattenAggression = 0.10  // 10% above mid
	cfg.FlattenMaxDeviation = 0.0 // disabled
	mid := 100_000.0
	ex := &mockExchange{state: exchange.StateSnapshot{
		Inventory: 0.002, // long
		Mid:       ptr(mid),
	}}

	mm := New(cfg, ex, testMarket())
	if err := mm.runCycle(context.Background(), 1); err != nil {
		t.Fatal(err)
	}
	if len(ex.placed) == 0 {
		t.Fatal("expected flatten order")
	}
	// With no cap, price should be ~10% above mid (after tick rounding).
	expected := mid * (1.0 + cfg.FlattenAggression)
	if ex.placed[0].Price < expected-1.0 {
		t.Errorf("expected uncapped price near %.2f, got %.2f", expected, ex.placed[0].Price)
	}
}

// ── Tests: Margin Recovery Tracking ──────────────────────────────────────────

// marginHighCycles increments each cycle spent above the threshold.
func TestMarginHighCyclesIncrement(t *testing.T) {
	cfg := testConfig()
	cfg.MaxMarginUsage = 0.5
	ex := &mockExchange{state: exchange.StateSnapshot{
		Inventory:   0.0,
		MarginUsage: 0.9, // above threshold
		Mid:         ptr(100_000),
	}}

	mm := New(cfg, ex, testMarket())
	for i := 0; i < 3; i++ {
		if err := mm.runCycle(context.Background(), 1); err != nil {
			t.Fatalf("cycle %d: %v", i+1, err)
		}
	}

	if mm.marginHighCycles != 3 {
		t.Errorf("marginHighCycles should be 3 after 3 paused cycles, got %d", mm.marginHighCycles)
	}
}

// marginHighCycles resets when margin returns below threshold.
func TestMarginHighCyclesResetOnRecovery(t *testing.T) {
	cfg := testConfig()
	cfg.MaxMarginUsage = 0.5
	ex := &mockExchange{state: exchange.StateSnapshot{
		Inventory:   0.0,
		MarginUsage: 0.9,
		Mid:         ptr(100_000),
	}}

	mm := New(cfg, ex, testMarket())
	// 3 high-margin cycles.
	for i := 0; i < 3; i++ {
		_ = mm.runCycle(context.Background(), 1)
	}
	if mm.marginHighCycles == 0 {
		t.Fatal("expected marginHighCycles > 0 after high-margin cycles")
	}

	// Margin recovers.
	ex.state.MarginUsage = 0.3
	if err := mm.runCycle(context.Background(), 1); err != nil {
		t.Fatal(err)
	}
	if mm.marginHighCycles != 0 {
		t.Errorf("marginHighCycles should reset to 0 after margin recovers, got %d", mm.marginHighCycles)
	}
}

// ── Tests: Circuit Breaker ────────────────────────────────────────────────────

// After a PlaceBulkOrders failure, the failure counter increments and a backoff is set.
func TestCircuitBreakerEngagesOnFailure(t *testing.T) {
	cfg := testConfig()
	bulkErr := errors.New("tx rejected")
	ex := &mockExchange{
		state:        exchange.StateSnapshot{Inventory: 0.0, Mid: ptr(100_000)},
		bulkOrderErr: bulkErr,
	}

	mm := New(cfg, ex, testMarket())
	err := mm.runCycle(context.Background(), 1)
	if err == nil {
		t.Fatal("expected error from failed PlaceBulkOrders")
	}

	if mm.bulkOrderFailures != 1 {
		t.Errorf("bulkOrderFailures should be 1 after first failure, got %d", mm.bulkOrderFailures)
	}
	if mm.bulkOrderBackoffUntil.IsZero() {
		t.Error("bulkOrderBackoffUntil should be set after failure")
	}
}

// After a successful PlaceBulkOrders, the circuit breaker resets.
func TestCircuitBreakerResetsOnSuccess(t *testing.T) {
	cfg := testConfig()
	ex := &mockExchange{state: exchange.StateSnapshot{Inventory: 0.0, Mid: ptr(100_000)}}

	mm := New(cfg, ex, testMarket())
	// Simulate prior failures.
	mm.bulkOrderFailures = 3

	if err := mm.runCycle(context.Background(), 1); err != nil {
		t.Fatalf("runCycle: %v", err)
	}

	if mm.bulkOrderFailures != 0 {
		t.Errorf("bulkOrderFailures should reset to 0 on success, got %d", mm.bulkOrderFailures)
	}
	if !mm.bulkOrderBackoffUntil.IsZero() {
		t.Error("bulkOrderBackoffUntil should be cleared on success")
	}
}

// While the backoff window is active, PlaceBulkOrders is skipped.
func TestCircuitBreakerSkipsPlacementDuringBackoff(t *testing.T) {
	cfg := testConfig()
	ex := &mockExchange{state: exchange.StateSnapshot{Inventory: 0.0, Mid: ptr(100_000)}}

	mm := New(cfg, ex, testMarket())
	// Set active backoff window (far future).
	mm.bulkOrderFailures = 2
	mm.bulkOrderBackoffUntil = time.Now().Add(10 * time.Minute)

	if err := mm.runCycle(context.Background(), 1); err != nil {
		t.Fatalf("expected no error during backoff, got: %v", err)
	}

	if len(ex.bulkBids)+len(ex.bulkAsks) != 0 {
		t.Errorf("expected no PlaceBulkOrders during backoff, got %d bids %d asks",
			len(ex.bulkBids), len(ex.bulkAsks))
	}
}

// ── Tests: Flatten Order Deduplication ───────────────────────────────────────

// While a flatten order is still resting (present in OpenOrders), no new flatten
// order should be placed — even across many cycles.
func TestFlattenNotDuplicatedWhileResting(t *testing.T) {
	cfg := testConfig()
	cfg.MaxInventory = 0.001
	cfg.AutoFlatten = true
	cfg.FlattenAggression = 0.001

	ex := &mockExchange{state: exchange.StateSnapshot{
		Inventory: 0.002,
		Mid:       ptr(100_000),
		// OpenOrders starts empty; filled in after first cycle below.
	}}

	mm := New(cfg, ex, testMarket())

	// Cycle 1: no resting order yet → flatten placed, returns "order-1".
	if err := mm.runCycle(context.Background(), 1); err != nil {
		t.Fatalf("cycle 1: %v", err)
	}
	if len(ex.placed) != 1 {
		t.Fatalf("cycle 1: expected 1 PlaceOrder call, got %d", len(ex.placed))
	}
	flattenID := ex.placedIDs[0] // "order-1"
	if mm.lastFlattenOrderID != flattenID {
		t.Errorf("lastFlattenOrderID should be %q, got %q", flattenID, mm.lastFlattenOrderID)
	}

	// Simulate the order sitting on the book for cycles 2 and 3.
	ex.state.OpenOrders = []exchange.OpenOrder{{OrderID: flattenID, MarketID: "0xmarket"}}

	for cycle := 2; cycle <= 3; cycle++ {
		if err := mm.runCycle(context.Background(), 1); err != nil {
			t.Fatalf("cycle %d: %v", cycle, err)
		}
	}

	if len(ex.placed) != 1 {
		t.Errorf("expected exactly 1 total PlaceOrder across 3 cycles, got %d", len(ex.placed))
	}
	if ex.cancelOrderCalls != 0 {
		t.Errorf("with FlattenRepriceStallCycles=0, expected no CancelOrder, got %d", ex.cancelOrderCalls)
	}
}

// After N consecutive cycles with the same resting flatten order, cancel and re-place once.
func TestFlattenRepriceAfterStallCycles(t *testing.T) {
	cfg := testConfig()
	cfg.MaxInventory = 0.001
	cfg.AutoFlatten = true
	cfg.FlattenAggression = 0.001
	cfg.FlattenRepriceStallCycles = 2

	ex := &mockExchange{state: exchange.StateSnapshot{
		Inventory: 0.002,
		Mid:       ptr(100_000),
	}}

	mm := New(cfg, ex, testMarket())

	if err := mm.runCycle(context.Background(), 1); err != nil {
		t.Fatalf("cycle 1: %v", err)
	}
	if len(ex.placed) != 1 {
		t.Fatalf("cycle 1: expected 1 PlaceOrder, got %d", len(ex.placed))
	}
	flattenID := ex.placedIDs[0]
	ex.state.OpenOrders = []exchange.OpenOrder{{OrderID: flattenID, MarketID: "0xmarket"}}

	if err := mm.runCycle(context.Background(), 2); err != nil {
		t.Fatalf("cycle 2: %v", err)
	}
	if ex.cancelOrderCalls != 0 {
		t.Fatalf("cycle 2: expected no cancel yet, got %d", ex.cancelOrderCalls)
	}
	if len(ex.placed) != 1 {
		t.Fatalf("cycle 2: expected still 1 PlaceOrder, got %d", len(ex.placed))
	}

	if err := mm.runCycle(context.Background(), 3); err != nil {
		t.Fatalf("cycle 3: %v", err)
	}
	if ex.cancelOrderCalls != 1 {
		t.Fatalf("cycle 3: expected 1 CancelOrder, got %d", ex.cancelOrderCalls)
	}
	if len(ex.placed) != 2 {
		t.Fatalf("cycle 3: expected 2 PlaceOrder total, got %d", len(ex.placed))
	}
	if mm.lastFlattenOrderID != ex.placedIDs[1] {
		t.Errorf("lastFlattenOrderID want %q got %q", ex.placedIDs[1], mm.lastFlattenOrderID)
	}
}

// When the flatten order is filled (disappears from OpenOrders), the next cycle
// should place a fresh flatten order.
func TestFlattenReplacedAfterFill(t *testing.T) {
	cfg := testConfig()
	cfg.MaxInventory = 0.001
	cfg.AutoFlatten = true
	cfg.FlattenAggression = 0.001

	ex := &mockExchange{state: exchange.StateSnapshot{
		Inventory: 0.002,
		Mid:       ptr(100_000),
	}}

	mm := New(cfg, ex, testMarket())

	// Cycle 1: places flatten order "order-1".
	if err := mm.runCycle(context.Background(), 1); err != nil {
		t.Fatalf("cycle 1: %v", err)
	}
	if len(ex.placed) != 1 {
		t.Fatalf("cycle 1: expected 1 PlaceOrder, got %d", len(ex.placed))
	}

	// Cycle 2: order is gone from OpenOrders (filled) → new flatten placed.
	ex.state.OpenOrders = nil // empty — order was filled
	if err := mm.runCycle(context.Background(), 1); err != nil {
		t.Fatalf("cycle 2: %v", err)
	}
	if len(ex.placed) != 2 {
		t.Errorf("expected 2 total PlaceOrder calls (fill + re-place), got %d", len(ex.placed))
	}
	if mm.lastFlattenOrderID != ex.placedIDs[1] {
		t.Errorf("lastFlattenOrderID should be updated to %q, got %q",
			ex.placedIDs[1], mm.lastFlattenOrderID)
	}
}

// When inventory recovers below MaxInventory, lastFlattenOrderID must be cleared
// so a fresh flatten is placed the next time inventory hits the limit.
func TestFlattenIDClearedOnInventoryRecovery(t *testing.T) {
	cfg := testConfig()
	cfg.MaxInventory = 0.001
	cfg.AutoFlatten = true
	cfg.FlattenAggression = 0.001

	ex := &mockExchange{state: exchange.StateSnapshot{
		Inventory: 0.002, // at limit
		Mid:       ptr(100_000),
	}}

	mm := New(cfg, ex, testMarket())

	// Cycle 1: place flatten, get an order ID.
	if err := mm.runCycle(context.Background(), 1); err != nil {
		t.Fatalf("cycle 1: %v", err)
	}
	if mm.lastFlattenOrderID == "" {
		t.Fatal("lastFlattenOrderID should be set after placing flatten order")
	}

	// Inventory recovers.
	ex.state.Inventory = 0.0005 // below MaxInventory
	ex.state.OpenOrders = nil
	if err := mm.runCycle(context.Background(), 1); err != nil {
		t.Fatalf("cycle 2 (recovery): %v", err)
	}

	if mm.lastFlattenOrderID != "" {
		t.Errorf("lastFlattenOrderID should be cleared on inventory recovery, got %q",
			mm.lastFlattenOrderID)
	}
}

// The size-rounds-to-zero path (second call site) also deduplicates flatten orders.
func TestFlattenDeduplicatesSizeZeroPath(t *testing.T) {
	cfg := testConfig()
	cfg.MaxInventory = 1.0   // high limit so invExceeded = false
	cfg.OrderSize = 0.000001 // tiny order size rounds to zero lots
	cfg.AutoFlatten = true
	cfg.FlattenAggression = 0.001

	market := testMarket()
	market.LotSize = 0.001 // large lot makes size round to zero
	market.MinSize = 0.001

	ex := &mockExchange{state: exchange.StateSnapshot{
		Inventory: 0.5, // has position but below the (high) MaxInventory
		Mid:       ptr(100_000),
	}}

	mm := New(cfg, ex, market)

	// Cycle 1: quotes == nil (size rounds to 0), flatten placed.
	if err := mm.runCycle(context.Background(), 1); err != nil {
		t.Fatalf("cycle 1: %v", err)
	}
	if len(ex.placed) != 1 {
		t.Fatalf("cycle 1: expected 1 PlaceOrder, got %d", len(ex.placed))
	}
	flattenID := ex.placedIDs[0]

	// Simulate order still resting for cycles 2–3.
	ex.state.OpenOrders = []exchange.OpenOrder{{OrderID: flattenID, MarketID: market.MarketID}}

	for cycle := 2; cycle <= 3; cycle++ {
		if err := mm.runCycle(context.Background(), 1); err != nil {
			t.Fatalf("cycle %d: %v", cycle, err)
		}
	}

	if len(ex.placed) != 1 {
		t.Errorf("expected exactly 1 PlaceOrder across 3 cycles (size-zero path), got %d",
			len(ex.placed))
	}
}

// State update includes PrevInventory so the notification layer can compute entry price.
func TestStateUpdateIncludesPrevInventory(t *testing.T) {
	cfg := testConfig()
	ex := &mockExchange{state: exchange.StateSnapshot{Inventory: 0.001, Mid: ptr(100_000)}}

	mm := New(cfg, ex, testMarket())

	// Cycle 1: establishes lastInventory = 0.001.
	if err := mm.runCycle(context.Background(), 1); err != nil {
		t.Fatal(err)
	}
	if mm.lastInventory != 0.001 {
		t.Errorf("lastInventory should be updated to 0.001 after cycle, got %.6f", mm.lastInventory)
	}

	// Cycle 2: inventory changes; prevInventory in state update should be 0.001.
	ex.state.Inventory = 0.003
	if err := mm.runCycle(context.Background(), 1); err != nil {
		t.Fatal(err)
	}

	// BotState stores PrevInventory; verify lastInventory tracks correctly.
	if mm.lastInventory != 0.003 {
		t.Errorf("lastInventory should track current inventory, got %.6f", mm.lastInventory)
	}
}

// Graceful shutdown only calls CancelBulkOrders, never CancelOrder.
func TestShutdownCancelQuotes_BulkOnlyNoCancelOrder(t *testing.T) {
	cfg := testConfig()
	ex := &mockExchange{state: exchange.StateSnapshot{Inventory: 0.0, Mid: ptr(100_000)}}
	mm := New(cfg, ex, testMarket())

	if err := mm.shutdownCancelQuotes(); err != nil {
		t.Fatalf("shutdownCancelQuotes: %v", err)
	}
	if ex.bulkCancelCalls != 1 {
		t.Errorf("expected 1 CancelBulkOrders, got %d", ex.bulkCancelCalls)
	}
	if ex.cancelOrderCalls != 0 {
		t.Errorf("expected 0 CancelOrder calls, got %d", ex.cancelOrderCalls)
	}
}

func TestShutdownCancelQuotes_AllRetriesFail(t *testing.T) {
	cfg := testConfig()
	wantErr := errors.New("bulk cancel failed")
	ex := &mockExchange{bulkCancelErr: wantErr}
	mm := New(cfg, ex, testMarket())

	err := mm.shutdownCancelQuotes()
	if err == nil {
		t.Fatal("expected error when CancelBulkOrders always fails")
	}
	if ex.bulkCancelCalls != shutdownBulkCancelMaxAttempts {
		t.Errorf("expected %d CancelBulkOrders attempts, got %d", shutdownBulkCancelMaxAttempts, ex.bulkCancelCalls)
	}
	if !errors.Is(err, wantErr) {
		t.Errorf("expected error chain to contain %v, got %v", wantErr, err)
	}
}

func TestRun_OnContextCancelRunsCleanup(t *testing.T) {
	cfg := testConfig()
	cfg.RefreshInterval = 0.01
	cfg.RefreshIntervalJitterS = 0
	ex := &mockExchange{state: exchange.StateSnapshot{Inventory: 0.0, Mid: ptr(100_000)}}
	mm := New(cfg, ex, testMarket())
	ctx, cancel := context.WithCancel(context.Background())

	runErr := make(chan error, 1)
	go func() { runErr <- mm.Run(ctx) }()

	time.Sleep(50 * time.Millisecond)
	cancel()

	select {
	case err := <-runErr:
		if err != nil {
			t.Fatalf("Run: %v", err)
		}
	case <-time.After(3 * time.Second):
		t.Fatal("Run did not exit after context cancel")
	}

	if ex.bulkCancelCalls != 1 {
		t.Errorf("expected exactly 1 CancelBulkOrders on shutdown, got %d", ex.bulkCancelCalls)
	}
	if ex.cancelOrderCalls != 0 {
		t.Errorf("shutdown must not call CancelOrder, got %d calls", ex.cancelOrderCalls)
	}
}
