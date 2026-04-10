// Package strategy implements the market-making cycle logic, decoupled from
// any specific exchange or notification system.
package strategy

import (
	"context"
	"fmt"
	"log/slog"
	"math"
	"time"

	"decibel-mm-bot/botstate"
	"decibel-mm-bot/config"
	"decibel-mm-bot/exchange"
	"decibel-mm-bot/pricing"
)

// MarketMaker runs the inventory-skew market-making strategy.
type MarketMaker struct {
	cfg    *config.Config
	ex     exchange.Exchange
	market *exchange.MarketConfig
	state  *botstate.BotState

	// adaptive spread state
	effectiveSpread float64
	lastInventory   float64
	noFillCycles    int
	firstCycle      bool
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
// Safe to call from any goroutine (reads state via snapshot).
func (m *MarketMaker) FlattenPosition(ctx context.Context) error {
	snap := m.state.Get()
	mid := 0.0
	if snap.Mid != nil {
		mid = *snap.Mid
	}
	return m.placeFlattenOrder(ctx, snap.Inventory, mid)
}

// Run starts the main market-making loop. Blocks until ctx is cancelled.
func (m *MarketMaker) Run(ctx context.Context) error {
	for cycle := uint64(1); ; cycle++ {
		slog.Info("─── cycle start ────────────────────────────────────", "cycle", cycle)

		if err := m.runCycle(ctx); err != nil {
			slog.Error("cycle failed", "cycle", cycle, "err", err)
		}

		slog.Info("sleeping", "seconds", m.cfg.RefreshInterval)
		select {
		case <-ctx.Done():
			slog.Info("shutting down")
			return nil
		case <-time.After(time.Duration(m.cfg.RefreshInterval * float64(time.Second))):
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

	// ── 2. Adaptive spread: fill detection ───────────────────────────────────
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
		slog.Info("inventory at limit, cancelling all orders",
			"inventory", state.Inventory, "max", m.cfg.MaxInventory)

		if _, _, err := m.cancelAllOrders(ctx, state.OpenOrders); err != nil {
			return fmt.Errorf("cancel all orders: %w", err)
		}
		if m.cfg.AutoFlatten {
			if err := m.placeFlattenOrder(ctx, state.Inventory, mid); err != nil {
				return fmt.Errorf("flatten order: %w", err)
			}
		} else {
			slog.Warn("at max inventory; manually flatten or enable --auto-flatten")
		}
		return nil
	}

	slog.Info("computed quotes", "bid", quotes.Bid, "ask", quotes.Ask, "size", quotes.Size)

	// ── 5. Cancel all resting orders ──────────────────────────────────────────
	nOK, nFail, err := m.cancelAllOrders(ctx, state.OpenOrders)
	if err != nil {
		return fmt.Errorf("cancel all orders: %w", err)
	}
	if nOK > 0 || nFail > 0 {
		slog.Info("cancel results", "cancelled", nOK, "failed", nFail)
	}

	if nFail > 0 {
		slog.Warn("failed cancels, waiting for chain resync", "wait_s", m.cfg.CancelResyncS)
		select {
		case <-ctx.Done():
			return ctx.Err()
		case <-time.After(time.Duration(m.cfg.CancelResyncS * float64(time.Second))):
		}

		freshOrders, err := m.ex.FetchOpenOrders(ctx)
		if err != nil {
			return fmt.Errorf("re-fetch open orders: %w", err)
		}
		if len(freshOrders) > 0 {
			slog.Warn("still have open orders after resync, skipping cycle",
				"count", len(freshOrders))
			return nil
		}
	}

	// ── 6. Place bid ──────────────────────────────────────────────────────────
	if err := m.ex.PlaceOrder(ctx, exchange.PlaceOrderRequest{
		MarketID:    m.market.MarketID,
		Price:       quotes.Bid,
		Size:        quotes.Size,
		IsBuy:       true,
		TimeInForce: 1, // POST_ONLY
		ReduceOnly:  false,
	}); err != nil {
		return fmt.Errorf("place bid: %w", err)
	}

	// ── 7. Cooldown ───────────────────────────────────────────────────────────
	select {
	case <-ctx.Done():
		return ctx.Err()
	case <-time.After(time.Duration(m.cfg.CooldownS * float64(time.Second))):
	}

	// ── 8. Place ask ──────────────────────────────────────────────────────────
	if err := m.ex.PlaceOrder(ctx, exchange.PlaceOrderRequest{
		MarketID:    m.market.MarketID,
		Price:       quotes.Ask,
		Size:        quotes.Size,
		IsBuy:       false,
		TimeInForce: 1, // POST_ONLY
		ReduceOnly:  false,
	}); err != nil {
		return fmt.Errorf("place ask: %w", err)
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

func (m *MarketMaker) placeFlattenOrder(ctx context.Context, inventory, mid float64) error {
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
		return nil
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
			MarketID: p.MarketID,
			Size:     p.Size,
		}
	}
	return result
}
