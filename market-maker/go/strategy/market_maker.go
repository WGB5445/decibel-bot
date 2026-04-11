// Package strategy implements the market-making cycle logic, decoupled from
// any specific exchange or notification system.
package strategy

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"math"
	"sync"
	"time"

	"decibel-mm-bot/botstate"
	"decibel-mm-bot/config"
	"decibel-mm-bot/exchange"
	"decibel-mm-bot/pricing"
)

// ErrNoPositionToFlatten is returned when there is no target-market position
// large enough to place a reduce-only flatten order (idempotent no-op).
var ErrNoPositionToFlatten = errors.New("no position to flatten")

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

	return m.placeFlattenOrder(ctx, inv, *state.Mid)
}

// Run starts the main market-making loop. Blocks until ctx is cancelled.
func (m *MarketMaker) Run(ctx context.Context) error {
	for cycle := uint64(1); ; cycle++ {
		slog.Info("─── cycle start ────────────────────────────────────", "cycle", cycle)

		if err := m.runCycle(ctx); err != nil {
			slog.Error("cycle failed", "cycle", cycle, "err", err)
		}

		sleep := cycleSleepDuration(m.cfg.RefreshInterval, m.cfg.RefreshIntervalJitterS)
		slog.Info("sleeping",
			"refresh_interval", m.cfg.RefreshInterval,
			"refresh_interval_jitter", m.cfg.RefreshIntervalJitterS,
			"sleep_seconds", sleep.Seconds(),
		)
		select {
		case <-ctx.Done():
			slog.Info("shutting down")
			return nil
		case <-time.After(sleep):
		}
	}
}

func (m *MarketMaker) runCycle(ctx context.Context) error {
	// ── 1. Parallel state fetch ───────────────────────────────────────────────
	state, err := m.ex.FetchState(ctx)
	if err != nil {
		return fmt.Errorf("fetch state: %w", err)
	}

	midStr := "<nil>"
	if state.Mid != nil {
		midStr = fmt.Sprintf("%.4f", *state.Mid)
	}
	slog.Info("state snapshot",
		"equity", state.Equity,
		"margin_usage", state.MarginUsage,
		"inventory", state.Inventory,
		"open_orders", len(state.OpenOrders),
		"mid", midStr,
	)

	// Share state with notification layer before lastInventory is updated this cycle.
	m.state.Update(botstate.StateUpdate{
		Equity:        state.Equity,
		MarginUsage:   state.MarginUsage,
		Inventory:     state.Inventory,
		Mid:           state.Mid,
		AllPositions:  exchangePositionsToBotstate(state.AllPositions),
		PrevInventory: m.lastInventory,
	})

	if math.Abs(state.Inventory) < m.cfg.MaxInventory {
		m.invLimitBulkCancelDone = false
	}

	// ── 2. First-cycle recovery ───────────────────────────────────────────────
	if m.firstCycle {
		slog.Info("first cycle: recovering state from chain",
			"inventory", state.Inventory,
			"open_orders", len(state.OpenOrders),
		)
	}

	// ── 2b. Adaptive spread: fill detection ──────────────────────────────────
	if !m.firstCycle {
		invChange := math.Abs(state.Inventory - m.lastInventory)
		fillDetected := invChange > m.market.LotSize*0.5
		if fillDetected {
			m.noFillCycles = 0
			newSpread := math.Min(m.effectiveSpread+m.cfg.SpreadStep*0.5, m.cfg.Spread)
			if newSpread > m.effectiveSpread {
				m.effectiveSpread = newSpread
				slog.Info("fill detected, spread widened slightly",
					"inv_change", invChange, "spread", m.effectiveSpread)
			} else {
				slog.Info("fill detected", "inv_change", invChange, "spread", m.effectiveSpread)
			}
		} else {
			m.noFillCycles++
			if m.noFillCycles >= m.cfg.SpreadNoFillCycles {
				suggested := math.Max(m.effectiveSpread-m.cfg.SpreadStep, m.cfg.SpreadMin)
				if suggested < m.effectiveSpread {
					if m.cfg.AutoSpread {
						m.effectiveSpread = suggested
						m.noFillCycles = 0
						slog.Warn("auto-spread narrowed (no fill)",
							"spread", m.effectiveSpread,
							"no_fill_cycles", m.cfg.SpreadNoFillCycles)
					} else {
						slog.Warn("suggestion: spread may be too wide",
							"current_spread", m.effectiveSpread,
							"suggested_spread", suggested,
							"no_fill_cycles", m.noFillCycles,
							"tip", "add --auto-spread to automate")
					}
				}
			}
		}
	}
	m.lastInventory = state.Inventory
	m.firstCycle = false

	// ── 3. Risk guard: margin ─────────────────────────────────────────────────
	if state.MarginUsage > m.cfg.MaxMarginUsage {
		slog.Warn("PAUSED: margin usage too high",
			"margin_usage", state.MarginUsage,
			"threshold", m.cfg.MaxMarginUsage,
		)
		return nil
	}

	// ── 3b. Risk guard: no price ──────────────────────────────────────────────
	if state.Mid == nil {
		slog.Warn("PAUSED: no mid price available")
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
					"inventory", state.Inventory, "max", m.cfg.MaxInventory)
				if err := m.ex.CancelBulkOrders(ctx); err != nil {
					return fmt.Errorf("cancel bulk orders: %w", err)
				}
				m.invLimitBulkCancelDone = true
			} else {
				slog.Info("inventory at limit; skipping cancel bulk until inventory recovers",
					"inventory", state.Inventory, "max", m.cfg.MaxInventory)
			}
			if m.cfg.AutoFlatten {
				if _, err := m.placeFlattenOrder(ctx, state.Inventory, mid); err != nil {
					return fmt.Errorf("flatten order: %w", err)
				}
			} else {
				slog.Warn("at max inventory; manually flatten or enable --auto-flatten")
			}
			return nil
		}

		slog.Info("cannot quote, cancelling bulk orders",
			"inventory", state.Inventory, "reason", "size rounds to zero or below min_size")
		if err := m.ex.CancelBulkOrders(ctx); err != nil {
			return fmt.Errorf("cancel bulk orders: %w", err)
		}
		if m.cfg.AutoFlatten {
			if _, err := m.placeFlattenOrder(ctx, state.Inventory, mid); err != nil {
				return fmt.Errorf("flatten order: %w", err)
			}
		}
		return nil
	}

	slog.Info("computed quotes", "bid", quotes.Bid, "ask", quotes.Ask, "size", quotes.Size)

	// ── 5. Atomically replace bulk quotes (bid + ask in one transaction) ──────
	if err := m.ex.PlaceBulkOrders(ctx,
		[]exchange.BulkOrderEntry{{Price: quotes.Bid, Size: quotes.Size}},
		[]exchange.BulkOrderEntry{{Price: quotes.Ask, Size: quotes.Size}},
	); err != nil {
		return fmt.Errorf("place bulk orders: %w", err)
	}

	slog.Info("cycle complete")
	return nil
}

// ── Order management ─────────────────────────────────────────────────────────

func (m *MarketMaker) cancelAllOrders(ctx context.Context, orders []exchange.OpenOrder) (nOK, nFail int, _ error) {
	if len(orders) == 0 {
		return 0, 0, nil
	}
	slog.Info("cancelling all resting orders", "count", len(orders))
	for _, o := range orders {
		if err := m.ex.CancelOrder(ctx, o.OrderID); err != nil {
			slog.Warn("cancel failed", "order_id", o.OrderID, "err", err)
			nFail++
		} else {
			nOK++
		}
	}
	return nOK, nFail, nil
}

func (m *MarketMaker) placeFlattenOrder(ctx context.Context, inventory, mid float64) (exchange.PlaceOrderOutcome, error) {
	isBuy := inventory < 0
	absInv := math.Abs(inventory)

	var rawPrice float64
	if isBuy {
		p := mid * (1.0 + m.cfg.FlattenAggression)
		rawPrice = math.Ceil(p/m.market.TickSize) * m.market.TickSize
	} else {
		p := mid * (1.0 - m.cfg.FlattenAggression)
		rawPrice = math.Floor(p/m.market.TickSize) * m.market.TickSize
	}

	size := math.Round(absInv/m.market.LotSize) * m.market.LotSize
	if size <= 0 || size < m.market.MinSize {
		slog.Warn("flatten size too small, skipping",
			"size", size, "min_size", m.market.MinSize)
		return exchange.PlaceOrderOutcome{}, ErrNoPositionToFlatten
	}

	return m.ex.PlaceOrder(ctx, exchange.PlaceOrderRequest{
		MarketID:    m.market.MarketID,
		Price:       rawPrice,
		Size:        size,
		IsBuy:       isBuy,
		TimeInForce: 0, // GTC
		ReduceOnly:  true,
	})
}

// ── Helpers ──────────────────────────────────────────────────────────────────

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
