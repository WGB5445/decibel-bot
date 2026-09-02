// Package strategy implements the market-making cycle logic, decoupled from
// any specific exchange or notification system.
package strategy

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"math"
	"strings"
	"sync"
	"time"

	"decibel-mm-bot/api"
	"decibel-mm-bot/botstate"
	"decibel-mm-bot/config"
	"decibel-mm-bot/exchange"
	"decibel-mm-bot/logctx"
	"decibel-mm-bot/logging"
	"decibel-mm-bot/pricing"
)

// ErrNoPositionToFlatten is returned when there is no target-market position
// large enough to place a reduce-only flatten order (idempotent no-op).
var ErrNoPositionToFlatten = errors.New("no position to flatten")

// marginStuckThreshold is the number of consecutive cycles spent above
// MaxMarginUsage before a CRITICAL alert is logged.
const marginStuckThreshold = 100

// shutdownBulkCancelMaxAttempts limits REST/Aptos retries for CancelBulkOrders during shutdown.
const shutdownBulkCancelMaxAttempts = 3

// maxOpenOrderIDsLogged is the max number of open orders for which we log order_ids (comma-separated).
const maxOpenOrderIDsLogged = 5

// MarketMaker runs the inventory-skew market-making strategy.
type MarketMaker struct {
	cfg    *config.Config
	ex     exchange.Exchange
	market *exchange.MarketConfig
	state  *botstate.BotState

	flattenMu sync.Mutex // serializes Telegram / manual flatten attempts

	// adaptive spread state
	effectiveSpread float64
	lastInventory   float64
	noFillCycles    int
	firstCycle      bool

	// When |inventory| >= MaxInventory we stop quoting; after one successful
	// CancelBulkOrders we skip further on-chain cancels until inventory recovers,
	// so we do not burn gas re-submitting noop cancels every cycle.
	invLimitBulkCancelDone bool

	// Circuit breaker for PlaceBulkOrders: track consecutive failures and apply
	// exponential backoff (extra sleep within runCycle) to prevent thrashing.
	bulkOrderFailures     int
	bulkOrderBackoffUntil time.Time

	// Margin recovery tracking: count cycles spent above MaxMarginUsage.
	// After marginStuckThreshold cycles we log a CRITICAL alert.
	marginHighCycles int

	// lastFlattenOrderID records the most recently placed reduce-only flatten order.
	// If this order is still present in state.OpenOrders we skip placing a new one,
	// preventing unbounded accumulation of POST_ONLY flatten orders while inventory stays
	// at the limit. Reset to "" when inventory recovers below MaxInventory.
	lastFlattenOrderID string

	// flattenStallCycles counts consecutive cycles where lastFlattenOrderID is still resting
	// and we skipped placing; used when FlattenRepriceStallCycles > 0 to trigger cancel+re-place.
	flattenStallCycles int

	// flattenStuckSince records when the current at-limit auto-flatten episode began (first
	// cycle inventory hit the limit and needed a flatten order). Zero means not currently stuck.
	// Reset only when inventory recovers below MaxInventory (see runCycle) — NOT on individual
	// reprice/force-close attempts, so a partially-filled forced close keeps escalating instead
	// of getting a fresh grace period. Drives FlattenForceSeconds escalation: resting POST_ONLY
	// aggression shrinks toward 0 as elapsed approaches the limit, then a marketable IOC
	// reduce-only order is forced to guarantee exit.
	flattenStuckSince time.Time

	// forceCloseCount is the cumulative number of forced IOC flatten closes. Only ever
	// mutated on the runCycle goroutine; safe to read here because it is copied into
	// botstate.BotState (itself mutex-protected) at the end of each cycle.
	forceCloseCount int

	// pauseMu guards paused/pauseCancelResting, which are written from a different
	// goroutine (e.g. the Telegram command handler) and read every cycle.
	pauseMu            sync.RWMutex
	paused             bool
	pauseCancelResting bool
}

// New creates a MarketMaker with the given exchange and market config.
func New(cfg *config.Config, ex exchange.Exchange, market *exchange.MarketConfig) *MarketMaker {
	return &MarketMaker{
		cfg:             cfg,
		ex:              ex,
		market:          market,
		effectiveSpread: cfg.Spread,
		firstCycle:      true,
		state:           botstate.New(market.MarketID, market.MarketName),
	}
}

// State returns the shared BotState for use by the notification layer.
func (m *MarketMaker) State() *botstate.BotState { return m.state }

// FlattenPosition places a reduce-only order to close the current position.
// Uses a live exchange snapshot (not BotState) so repeated calls are idempotent
// once the chain reflects the closed position. Serialized with flattenMu.
func (m *MarketMaker) FlattenPosition(ctx context.Context) (exchange.PlaceOrderOutcome, error) {
	m.flattenMu.Lock()
	defer m.flattenMu.Unlock()

	state, err := m.ex.FetchState(ctx)
	if err != nil {
		return exchange.PlaceOrderOutcome{}, fmt.Errorf("fetch state for flatten: %w", err)
	}
	if state.Mid == nil {
		return exchange.PlaceOrderOutcome{}, fmt.Errorf("cannot flatten position: mid price unavailable")
	}

	inv := state.Inventory
	absInv := math.Abs(inv)
	size := math.Round(absInv/m.market.LotSize) * m.market.LotSize
	if size <= 0 || size < m.market.MinSize {
		return exchange.PlaceOrderOutcome{}, ErrNoPositionToFlatten
	}

	return m.placeFlattenOrder(ctx, inv, *state.Mid, m.cfg.FlattenAggression)
}

// Pause stops placing new bulk quotes starting from the next cycle. Risk controls
// (margin guard, inventory-limit auto-flatten and its escalation) keep running
// regardless of pause state — pause only affects normal two-sided quoting.
// When cancelResting is true, also cancels any resting bulk quotes immediately.
func (m *MarketMaker) Pause(ctx context.Context, cancelResting bool) error {
	m.pauseMu.Lock()
	m.paused = true
	m.pauseCancelResting = cancelResting
	m.pauseMu.Unlock()

	if cancelResting {
		if err := m.ex.CancelBulkOrders(ctx); err != nil {
			return fmt.Errorf("pause: cancel bulk orders: %w", err)
		}
	}
	return nil
}

// Resume clears a previously-set pause, allowing normal quoting to resume next cycle.
func (m *MarketMaker) Resume() {
	m.pauseMu.Lock()
	defer m.pauseMu.Unlock()
	m.paused = false
	m.pauseCancelResting = false
}

// PauseState reports whether trading is currently paused and, if so, whether the
// pause also cancelled resting bulk quotes.
func (m *MarketMaker) PauseState() (paused, cancelResting bool) {
	m.pauseMu.RLock()
	defer m.pauseMu.RUnlock()
	return m.paused, m.pauseCancelResting
}

// Run starts the main market-making loop. Blocks until ctx is cancelled.
func (m *MarketMaker) Run(ctx context.Context) error {
	for cycle := uint64(1); ; cycle++ {
		logging.Cycle("─── cycle start ────────────────────────────────────", "cycle", cycle)

		if err := m.runCycle(ctx, cycle); err != nil {
			slog.Error("cycle failed", "cycle", cycle, "err", err)
		}

		sleep := cycleSleepDuration(m.cfg.RefreshInterval, m.cfg.RefreshIntervalJitterS)
		logging.Cycle("sleeping",
			"refresh_interval", m.cfg.RefreshInterval,
			"refresh_interval_jitter", m.cfg.RefreshIntervalJitterS,
			"sleep_seconds", sleep.Seconds(),
		)
		select {
		case <-ctx.Done():
			logging.Cycle("shutting down", "reason", ctx.Err())
			if err := m.shutdownCancelQuotes(); err != nil {
				return fmt.Errorf("shutdown cancel: %w", err)
			}
			return nil
		case <-time.After(sleep):
		}
	}
}

// shutdownCancelQuotes revokes bulk market-making quotes for the configured market only.
// It uses a fresh timeout context because the main ctx is already cancelled on SIGINT/SIGTERM.
// Single resting orders (e.g. POST_ONLY auto-flatten) are not cancelled here by design.
func (m *MarketMaker) shutdownCancelQuotes() error {
	shutdownCtx, cancel := context.WithTimeout(context.Background(), m.cfg.ShutdownCancelTimeout())
	defer cancel()

	slog.Info("shutdown: cancelling bulk quotes for this market only (single resting orders, e.g. POST_ONLY auto-flatten, are not cancelled)",
		"market", m.market.MarketName,
		"timeout", m.cfg.ShutdownCancelTimeout(),
	)
	slog.Warn("do not kill -9 the process; forced kill may leave resting orders on the book")

	if m.ex.DryRun() {
		slog.Warn("dry-run: no on-chain bulk cancel transactions will be sent; orders will not be cancelled")
	}

	var lastErr error
	for attempt := 1; attempt <= shutdownBulkCancelMaxAttempts; attempt++ {
		if err := shutdownCtx.Err(); err != nil {
			if lastErr != nil {
				return fmt.Errorf("shutdown cancel deadline: %w (last_err=%v)", err, lastErr)
			}
			return fmt.Errorf("shutdown cancel deadline: %w", err)
		}
		err := m.ex.CancelBulkOrders(shutdownCtx)
		if err == nil {
			logging.Success("shutdown cleanup complete: bulk cancel succeeded or no bulk quotes to cancel")
			return nil
		}
		lastErr = err
		slog.Warn("bulk cancel failed, retrying",
			"attempt", attempt,
			"max_attempts", shutdownBulkCancelMaxAttempts,
			"err", err,
		)
		if attempt == shutdownBulkCancelMaxAttempts {
			break
		}
		backoff := time.Duration(attempt*attempt) * time.Second
		select {
		case <-shutdownCtx.Done():
			return fmt.Errorf("shutdown cancel deadline: %w (last_err=%v)", shutdownCtx.Err(), lastErr)
		case <-time.After(backoff):
		}
	}
	return fmt.Errorf("CancelBulkOrders retries exhausted: %w", lastErr)
}

func (m *MarketMaker) runCycle(ctx context.Context, cycle uint64) error {
	ctx = logctx.WithCycle(ctx, cycle)

	// ── 1. Parallel state fetch ───────────────────────────────────────────────
	state, err := m.ex.FetchState(ctx)
	if err != nil {
		return fmt.Errorf("fetch state: %w", err)
	}

	midStr := "<nil>"
	var midVal float64
	hasMid := false
	if state.Mid != nil {
		midVal = *state.Mid
		midStr = fmt.Sprintf("%.4f", midVal)
		hasMid = true
	}
	ss := []any{
		"cycle", cycle,
		"equity", state.Equity,
		"margin_usage", state.MarginUsage,
		"inventory", state.Inventory,
		"mid", midStr,
		"market_name", m.market.MarketName,
		"market_id", api.AddrSuffix(m.market.MarketID),
		"max_inventory", m.cfg.MaxInventory,
		"order_size", m.cfg.OrderSize,
		"cfg_spread", m.cfg.Spread,
		"effective_spread", m.effectiveSpread,
	}
	if hasMid {
		ss = append(ss, "mid_f", midVal)
	}
	ss = append(ss, openOrderSummaryArgs(state.OpenOrders)...)
	slog.Info("state_snapshot", ss...)

	// Share state with notification layer before lastInventory is updated this cycle.
	// EffectiveSpread/FlattenStuckSeconds reflect the end of the PREVIOUS cycle (this
	// cycle's adjustments/escalation happen further below) — an acceptable ~1-cycle lag
	// for a monitoring display, matching the existing PrevInventory convention above.
	flattenStuckSeconds := 0.0
	if !m.flattenStuckSince.IsZero() {
		flattenStuckSeconds = time.Since(m.flattenStuckSince).Seconds()
	}
	paused, pauseCancelResting := m.PauseState()
	m.state.Update(botstate.StateUpdate{
		Equity:              state.Equity,
		MarginUsage:         state.MarginUsage,
		Inventory:           state.Inventory,
		Mid:                 state.Mid,
		AllPositions:        exchangePositionsToBotstate(state.AllPositions),
		PrevInventory:       m.lastInventory,
		EffectiveSpread:     m.effectiveSpread,
		Paused:              paused,
		PauseCancelResting:  pauseCancelResting,
		FlattenStuckSeconds: flattenStuckSeconds,
		ForceCloseCount:     m.forceCloseCount,
	})

	if math.Abs(state.Inventory) < m.cfg.MaxInventory {
		if m.invLimitBulkCancelDone {
			// Inventory has just recovered from the limit — reset both flags so we
			// re-arm for the next limit event and allow a fresh flatten if needed.
			m.lastFlattenOrderID = ""
			m.flattenStallCycles = 0
			m.flattenStuckSince = time.Time{}
		}
		m.invLimitBulkCancelDone = false
	}

	// ── 2. First-cycle recovery ───────────────────────────────────────────────
	if m.firstCycle {
		slog.Info("first cycle: recovering state from chain",
			"cycle", cycle,
			"inventory", state.Inventory,
			"open_orders", len(state.OpenOrders),
		)
	}

	// ── 2b. Adaptive spread: fill detection ──────────────────────────────────
	if !m.firstCycle {
		invAtLimit := math.Abs(state.Inventory) >= m.cfg.MaxInventory
		invChange := math.Abs(state.Inventory - m.lastInventory)
		fillDetected := invChange > m.market.LotSize*0.5
		if fillDetected {
			m.noFillCycles = 0
			newSpread := math.Min(m.effectiveSpread+m.cfg.SpreadStep*0.5, m.cfg.Spread)
			if newSpread > m.effectiveSpread {
				m.effectiveSpread = newSpread
				slog.Info("fill detected, spread widened slightly",
					"cycle", cycle,
					"inv_change", invChange, "spread", m.effectiveSpread)
			} else {
				slog.Info("fill detected", "cycle", cycle, "inv_change", invChange, "spread", m.effectiveSpread)
			}
		} else {
			// At max inventory we are not placing bulk quotes (ComputeQuotes returns nil);
			// do not count "no fill" cycles toward auto-spread narrowing.
			if invAtLimit {
				m.noFillCycles = 0
			} else {
				m.noFillCycles++
				if m.noFillCycles >= m.cfg.SpreadNoFillCycles {
					suggested := math.Max(m.effectiveSpread-m.cfg.SpreadStep, m.cfg.SpreadMin)
					if suggested < m.effectiveSpread {
						if m.cfg.AutoSpread {
							m.effectiveSpread = suggested
							m.noFillCycles = 0
							slog.Warn("auto-spread narrowed (no fill)",
								"cycle", cycle,
								"spread", m.effectiveSpread,
								"no_fill_cycles", m.cfg.SpreadNoFillCycles)
						} else {
							slog.Warn("suggestion: spread may be too wide",
								"cycle", cycle,
								"current_spread", m.effectiveSpread,
								"suggested_spread", suggested,
								"no_fill_cycles", m.noFillCycles,
								"tip", "add --auto-spread to automate")
						}
					}
				}
			}
		}
	}
	m.lastInventory = state.Inventory
	m.firstCycle = false

	// ── 3. Risk guard: margin ─────────────────────────────────────────────────
	if state.MarginUsage > m.cfg.MaxMarginUsage {
		m.marginHighCycles++
		if m.marginHighCycles >= marginStuckThreshold {
			slog.Error("CRITICAL: margin has been too high for many cycles — manual intervention may be required",
				"cycle", cycle,
				"margin_usage", state.MarginUsage,
				"threshold", m.cfg.MaxMarginUsage,
				"cycles_paused", m.marginHighCycles,
			)
		} else {
			slog.Warn("PAUSED: margin usage too high",
				"cycle", cycle,
				"margin_usage", state.MarginUsage,
				"threshold", m.cfg.MaxMarginUsage,
				"cycles_paused", m.marginHighCycles,
			)
		}
		return nil
	}
	m.marginHighCycles = 0

	// ── 3b. Risk guard: no price ──────────────────────────────────────────────
	if state.Mid == nil {
		slog.Warn("PAUSED: no mid price available", "cycle", cycle)
		return nil
	}
	mid := *state.Mid

	// ── 4. Compute quotes ─────────────────────────────────────────────────────
	quotes, err := pricing.ComputeQuotes(
		mid,
		state.Inventory,
		m.effectiveSpread,
		m.cfg.SkewPerUnit,
		m.cfg.MaxInventory,
		m.market.TickSize,
		m.market.LotSize,
		m.market.MinSize,
		m.cfg.OrderSize,
	)
	if err != nil {
		return fmt.Errorf("compute quotes: %w", err)
	}

	if quotes == nil {
		invExceeded := math.Abs(state.Inventory) >= m.cfg.MaxInventory
		if invExceeded {
			if !m.invLimitBulkCancelDone {
				slog.Info("inventory at limit, cancelling bulk orders",
					"cycle", cycle,
					"inventory", state.Inventory, "max", m.cfg.MaxInventory)
				if err := m.ex.CancelBulkOrders(ctx); err != nil {
					return fmt.Errorf("cancel bulk orders: %w", err)
				}
				m.invLimitBulkCancelDone = true
			} else {
				slog.Info("inventory at limit; skipping cancel bulk until inventory recovers",
					"cycle", cycle,
					"inventory", state.Inventory, "max", m.cfg.MaxInventory)
			}
			if m.cfg.AutoFlatten {
				if err := m.handleAutoFlattenAtLimit(ctx, cycle, mid, state.Inventory, state.OpenOrders); err != nil {
					return err
				}
			} else {
				slog.Warn("at max inventory; manually flatten or enable --auto-flatten", "cycle", cycle)
			}
			return nil
		}

		slog.Info("cannot quote, cancelling bulk orders",
			"cycle", cycle,
			"inventory", state.Inventory, "reason", "size rounds to zero or below min_size")
		if err := m.ex.CancelBulkOrders(ctx); err != nil {
			return fmt.Errorf("cancel bulk orders: %w", err)
		}
		if m.cfg.AutoFlatten {
			if err := m.handleAutoFlattenAtLimit(ctx, cycle, mid, state.Inventory, state.OpenOrders); err != nil {
				return err
			}
		}
		return nil
	}

	slog.Info("computed quotes",
		"cycle", cycle, "bid", quotes.Bid, "ask", quotes.Ask, "size", quotes.Size)

	// ── 4b. Pause guard: skip new quote placement while paused ────────────────
	// Risk controls (margin guard above, auto-flatten escalation in the quotes==nil
	// branch above) are NOT gated by pause — only normal two-sided quoting is.
	if paused, _ := m.PauseState(); paused {
		slog.Info("paused: skipping bulk quote placement", "cycle", cycle)
		return nil
	}

	// ── 5. Atomically replace bulk quotes (bid + ask in one transaction) ──────

	// Circuit breaker: if we're in a backoff window, skip placing and warn.
	if time.Now().Before(m.bulkOrderBackoffUntil) {
		slog.Warn("circuit breaker active, skipping PlaceBulkOrders",
			"cycle", cycle,
			"failures", m.bulkOrderFailures,
			"backoff_remaining_s", math.Round(time.Until(m.bulkOrderBackoffUntil).Seconds()),
		)
		return nil
	}

	slog.Info("mm_place_bulk",
		"cycle", cycle,
		"market", m.market.MarketName,
		"market_id", api.AddrSuffix(m.market.MarketID),
		"bid_price", quotes.Bid,
		"ask_price", quotes.Ask,
		"size", quotes.Size,
		"tif", "POST_ONLY",
		"dry_run", m.ex.DryRun(),
	)

	if err := m.ex.PlaceBulkOrders(ctx,
		[]exchange.BulkOrderEntry{{Price: quotes.Bid, Size: quotes.Size}},
		[]exchange.BulkOrderEntry{{Price: quotes.Ask, Size: quotes.Size}},
	); err != nil {
		m.bulkOrderFailures++
		// Exponential backoff: 2^failures × RefreshInterval, capped at 5 minutes.
		backoff := time.Duration(math.Min(
			math.Pow(2, float64(m.bulkOrderFailures))*m.cfg.RefreshInterval,
			300,
		)) * time.Second
		m.bulkOrderBackoffUntil = time.Now().Add(backoff)
		slog.Error("PlaceBulkOrders failed, circuit breaker engaged",
			"cycle", cycle,
			"failures", m.bulkOrderFailures,
			"backoff_s", backoff.Seconds(),
		)
		return fmt.Errorf("place bulk orders: %w", err)
	}

	// Success — reset circuit breaker.
	m.bulkOrderFailures = 0
	m.bulkOrderBackoffUntil = time.Time{}

	if m.cfg.LogCycleJSON {
		trace := struct {
			Msg             string  `json:"msg"`
			Cycle           uint64  `json:"cycle"`
			Market          string  `json:"market"`
			Mid             float64 `json:"mid"`
			Inventory       float64 `json:"inventory"`
			Bid             float64 `json:"bid"`
			Ask             float64 `json:"ask"`
			Size            float64 `json:"size"`
			DryRun          bool    `json:"dry_run"`
			EffectiveSpread float64 `json:"effective_spread"`
			CfgSpread       float64 `json:"cfg_spread"`
		}{
			Msg: "mm_cycle", Cycle: cycle, Market: m.market.MarketName,
			Mid: mid, Inventory: state.Inventory,
			Bid: quotes.Bid, Ask: quotes.Ask, Size: quotes.Size,
			DryRun: m.ex.DryRun(), EffectiveSpread: m.effectiveSpread, CfgSpread: m.cfg.Spread,
		}
		if b, err := json.Marshal(trace); err == nil {
			slog.Info("cycle_trace_json", "cycle", cycle, "payload", string(b))
		}
	}

	logging.Cycle("cycle complete")
	return nil
}

// ── Order management ─────────────────────────────────────────────────────────

func (m *MarketMaker) cancelAllOrders(ctx context.Context, orders []exchange.OpenOrder) (nOK, nFail int, _ error) {
	if len(orders) == 0 {
		return 0, 0, nil
	}
	slog.Info("cancelling all resting orders", logctx.AppendAttrs(ctx, "count", len(orders))...)
	for _, o := range orders {
		if err := m.ex.CancelOrder(ctx, o.OrderID); err != nil {
			slog.Warn("cancel failed", logctx.AppendAttrs(ctx, "order_id", o.OrderID, "err", err)...)
			nFail++
		} else {
			nOK++
		}
	}
	return nOK, nFail, nil
}

// shouldSkipFlatten returns true if the last flatten order is still resting,
// meaning we should not place another one this cycle. If the order is gone
// (filled or externally cancelled) it clears lastFlattenOrderID and returns false.
func (m *MarketMaker) shouldSkipFlatten(openOrders []exchange.OpenOrder) bool {
	if m.lastFlattenOrderID == "" {
		return false
	}
	for _, o := range openOrders {
		if o.OrderID == m.lastFlattenOrderID {
			return true // still resting
		}
	}
	// Order is gone — allow re-placement.
	m.lastFlattenOrderID = ""
	m.flattenStallCycles = 0
	return false
}

// handleAutoFlattenAtLimit maintains a reduce-only flatten while at inventory / quoting
// limits, escalating over wall-clock time (see flattenStuckSince):
//
//  1. Reprice: when FlattenRepriceStallCycles > 0, after that many consecutive cycles
//     with the same resting POST_ONLY order, cancel and re-place at the current mid.
//     N == 0 preserves legacy behavior (never cancel for reprice).
//  2. Escalate: each reprice uses an aggression that shrinks linearly from
//     FlattenAggression toward 0 as elapsed time approaches FlattenForceSeconds, so the
//     resting order creeps closer to mid (higher fill probability) the longer we're stuck.
//  3. Force: once stuck for FlattenForceSeconds, cancel the resting order and submit a
//     marketable IOC reduce-only order (FlattenForceDeviation from mid) to guarantee exit,
//     accepting taker fee + slippage. Repeats every cycle until inventory recovers.
//
// FlattenForceSeconds == 0 disables step 3 (legacy: passive-only, may stay stuck indefinitely).
func (m *MarketMaker) handleAutoFlattenAtLimit(ctx context.Context, cycle uint64, mid, inventory float64, openOrders []exchange.OpenOrder) error {
	if m.flattenStuckSince.IsZero() {
		m.flattenStuckSince = time.Now()
	}
	elapsed := time.Since(m.flattenStuckSince)

	if m.cfg.FlattenForceSeconds > 0 && elapsed.Seconds() >= m.cfg.FlattenForceSeconds {
		return m.forceCloseFlatten(ctx, cycle, mid, inventory, openOrders, elapsed)
	}

	n := m.cfg.FlattenRepriceStallCycles
	aggression := m.escalatedFlattenAggression(elapsed)

	if m.shouldSkipFlatten(openOrders) {
		if n <= 0 {
			slog.Info("flatten order already resting, skipping",
				"cycle", cycle, "order_id", m.lastFlattenOrderID)
			return nil
		}
		m.flattenStallCycles++
		if m.flattenStallCycles < n {
			slog.Info("flatten order resting, awaiting stall cycles before reprice",
				"cycle", cycle, "order_id", m.lastFlattenOrderID,
				"stall_cycles", m.flattenStallCycles, "stall_limit", n,
				"stuck_seconds", elapsed.Seconds())
			return nil
		}
		slog.Info("flatten reprice: cancel resting order after stall cycles",
			"cycle", cycle, "order_id", m.lastFlattenOrderID, "stall_limit", n,
			"escalated_aggression", aggression, "stuck_seconds", elapsed.Seconds())
		oid := m.lastFlattenOrderID
		if err := m.ex.CancelOrder(ctx, oid); err != nil {
			slog.Warn("flatten reprice: cancel order failed",
				"cycle", cycle, "order_id", oid, "err", err)
			return nil
		}
		m.lastFlattenOrderID = ""
		m.flattenStallCycles = 0
	}
	outcome, err := m.placeFlattenOrder(ctx, inventory, mid, aggression)
	if err != nil && !errors.Is(err, ErrNoPositionToFlatten) {
		return fmt.Errorf("flatten order: %w", err)
	}
	if err == nil {
		m.lastFlattenOrderID = outcome.OrderID
		m.flattenStallCycles = 0
	}
	return nil
}

// escalatedFlattenAggression linearly shrinks FlattenAggression toward 0 as elapsed
// approaches FlattenForceSeconds, so resting flatten orders creep toward mid (higher
// fill probability) the longer inventory stays stuck, right up to the forced IOC close.
// FlattenForceSeconds <= 0 disables escalation (always returns the base aggression).
func (m *MarketMaker) escalatedFlattenAggression(elapsed time.Duration) float64 {
	base := m.cfg.FlattenAggression
	if m.cfg.FlattenForceSeconds <= 0 {
		return base
	}
	progress := elapsed.Seconds() / m.cfg.FlattenForceSeconds
	if progress <= 0 {
		return base
	}
	if progress >= 1 {
		return 0
	}
	return base * (1 - progress)
}

// forceCloseFlatten cancels any resting POST_ONLY flatten order and submits a marketable
// IOC reduce-only order to guarantee exit after FlattenForceSeconds of being stuck at the
// inventory limit. Logged at Error level (CRITICAL) since this is an unplanned taker exit —
// worth surfacing even without watching logs. Does not reset flattenStuckSince itself: a
// partial fill (still over the limit next cycle) will force-close again immediately rather
// than getting a fresh grace period; only the top-of-cycle inventory-recovery check clears it.
func (m *MarketMaker) forceCloseFlatten(ctx context.Context, cycle uint64, mid, inventory float64, openOrders []exchange.OpenOrder, elapsed time.Duration) error {
	if m.lastFlattenOrderID != "" {
		for _, o := range openOrders {
			if o.OrderID == m.lastFlattenOrderID {
				if err := m.ex.CancelOrder(ctx, m.lastFlattenOrderID); err != nil {
					slog.Warn("force close: cancel resting flatten order failed",
						"cycle", cycle, "order_id", m.lastFlattenOrderID, "err", err)
				}
				break
			}
		}
		m.lastFlattenOrderID = ""
		m.flattenStallCycles = 0
	}

	dev := m.cfg.FlattenForceDeviation
	if dev <= 0 {
		dev = m.cfg.FlattenMaxDeviation
	}

	isBuy := inventory < 0
	absInv := math.Abs(inventory)
	size := math.Round(absInv/m.market.LotSize) * m.market.LotSize
	if size <= 0 || size < m.market.MinSize {
		return nil
	}

	var rawPrice float64
	if isBuy {
		rawPrice = math.Ceil(mid*(1.0+dev)/m.market.TickSize) * m.market.TickSize
	} else {
		rawPrice = math.Floor(mid*(1.0-dev)/m.market.TickSize) * m.market.TickSize
	}

	slog.Error("CRITICAL: forced IOC flatten — stuck at inventory limit past FLATTEN_FORCE_SECONDS, taking liquidity to guarantee exit",
		"cycle", cycle,
		"inventory", inventory,
		"mid", mid,
		"stuck_seconds", elapsed.Seconds(),
		"force_seconds_limit", m.cfg.FlattenForceSeconds,
		"price", rawPrice,
		"size", size,
		"deviation", dev,
	)

	outcome, err := m.ex.PlaceOrder(ctx, exchange.PlaceOrderRequest{
		MarketID:    m.market.MarketID,
		Price:       rawPrice,
		Size:        size,
		IsBuy:       isBuy,
		TimeInForce: exchange.TimeInForceIOC,
		ReduceOnly:  true,
	})
	if err != nil {
		slog.Error("forced IOC flatten failed", "cycle", cycle, "err", err)
		return fmt.Errorf("forced IOC flatten: %w", err)
	}

	slog.Warn("forced IOC flatten submitted", "cycle", cycle, "order_id", outcome.OrderID, "tx_hash", outcome.TxHash)
	m.forceCloseCount++
	return nil
}

func (m *MarketMaker) placeFlattenOrder(ctx context.Context, inventory, mid, aggression float64) (exchange.PlaceOrderOutcome, error) {
	isBuy := inventory < 0
	absInv := math.Abs(inventory)

	// POST_ONLY: price on the passive side of mid so the order rests as maker
	// (we do not have BBO in StateSnapshot — mid-based passive quote is the best proxy).
	// Long → sell above mid; short → buy below mid.
	var rawPrice float64
	if isBuy {
		p := mid * (1.0 - aggression)
		if m.cfg.FlattenMaxDeviation > 0 {
			floor := mid * (1.0 - m.cfg.FlattenMaxDeviation)
			if p < floor {
				slog.Warn("flatten buy price floored by FlattenMaxDeviation",
					logctx.AppendAttrs(ctx, "uncapped", p, "floored", floor)...,
				)
				p = floor
			}
		}
		rawPrice = math.Floor(p/m.market.TickSize) * m.market.TickSize
	} else {
		p := mid * (1.0 + aggression)
		if m.cfg.FlattenMaxDeviation > 0 {
			cap := mid * (1.0 + m.cfg.FlattenMaxDeviation)
			if p > cap {
				slog.Warn("flatten sell price capped by FlattenMaxDeviation",
					logctx.AppendAttrs(ctx, "uncapped", p, "capped", cap)...,
				)
				p = cap
			}
		}
		rawPrice = math.Ceil(p/m.market.TickSize) * m.market.TickSize
	}

	if m.cfg.FlattenMaxDeviation > 0 {
		tick := m.market.TickSize
		if isBuy {
			minPx := mid * (1.0 - m.cfg.FlattenMaxDeviation)
			if rawPrice < minPx {
				rawPrice = math.Ceil(minPx/tick) * tick
			}
		} else {
			maxPx := mid * (1.0 + m.cfg.FlattenMaxDeviation)
			if rawPrice > maxPx {
				rawPrice = math.Floor(maxPx/tick) * tick
			}
		}
	}

	size := math.Round(absInv/m.market.LotSize) * m.market.LotSize
	if size <= 0 || size < m.market.MinSize {
		slog.Warn("flatten size too small, skipping",
			logctx.AppendAttrs(ctx, "size", size, "min_size", m.market.MinSize)...,
		)
		return exchange.PlaceOrderOutcome{}, ErrNoPositionToFlatten
	}

	slog.Info("flatten_intent",
		logctx.AppendAttrs(ctx,
			"inventory", inventory,
			"mid", mid,
			"raw_price", rawPrice,
			"size", size,
			"is_buy", isBuy,
			"tif", exchange.TimeInForcePostOnly,
			"reduce_only", true,
			"flatten_aggression", aggression,
			"flatten_max_deviation", m.cfg.FlattenMaxDeviation,
			"market", m.market.MarketName,
			"market_id", api.AddrSuffix(m.market.MarketID),
		)...,
	)

	return m.ex.PlaceOrder(ctx, exchange.PlaceOrderRequest{
		MarketID:    m.market.MarketID,
		Price:       rawPrice,
		Size:        size,
		IsBuy:       isBuy,
		TimeInForce: exchange.TimeInForcePostOnly,
		ReduceOnly:  true,
	})
}

// ── Helpers ──────────────────────────────────────────────────────────────────

func shortOrderID(id string) string {
	id = strings.TrimSpace(id)
	if len(id) <= 16 {
		return id
	}
	return id[len(id)-10:]
}

func openOrderSummaryArgs(orders []exchange.OpenOrder) []any {
	n := len(orders)
	out := []any{"open_orders", n}
	if n > 0 && n <= maxOpenOrderIDsLogged {
		ids := make([]string, n)
		for i, o := range orders {
			ids[i] = shortOrderID(o.OrderID)
		}
		out = append(out, "order_ids", strings.Join(ids, ","))
	}
	return out
}

func exchangePositionsToBotstate(positions []exchange.Position) []botstate.Position {
	result := make([]botstate.Position, len(positions))
	for i, p := range positions {
		result[i] = botstate.Position{
			MarketID:                  p.MarketID,
			Size:                      p.Size,
			EntryPrice:                p.EntryPrice,
			UserLeverage:              p.UserLeverage,
			UnrealizedFunding:         p.UnrealizedFunding,
			EstimatedLiquidationPrice: p.EstimatedLiquidationPrice,
			IsIsolated:                p.IsIsolated,
			TransactionVersion:        p.TransactionVersion,
			IsDeleted:                 p.IsDeleted,
		}
	}
	return result
}
