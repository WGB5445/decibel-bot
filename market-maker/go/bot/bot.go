// Package bot implements the main market-maker loop.
package bot

import (
	"context"
	"fmt"
	"log/slog"
	"math"
	"time"

	"decibel-mm-bot/api"
	"decibel-mm-bot/aptos"
	"decibel-mm-bot/config"
	"decibel-mm-bot/pricing"
)

// Run is the top-level entry point: validates config, discovers market, then loops.
func Run(ctx context.Context, cfg *config.Config) error {
	slog.Info("starting Decibel Market Maker",
		"market", cfg.MarketName,
		"spread", cfg.Spread,
		"order_size", cfg.OrderSize,
		"max_inventory", cfg.MaxInventory,
		"dry_run", cfg.DryRun,
	)
	slog.Info("perp engine", "address", cfg.PerpEngineGlobalAddress)

	seed, err := cfg.ParsePrivateKey()
	if err != nil {
		return fmt.Errorf("parse private key: %w", err)
	}

	apiClient := api.NewClient(cfg.RestAPIBase, cfg.BearerToken)
	aptosClient := aptos.NewClient(cfg.AptosFullnodeURL, cfg.NodeKey(), seed)

	slog.Info("derived sender address", "address", aptosClient.SenderAddress())
	slog.Info("using subaccount", "address", cfg.SubaccountAddress)

	// ── Market discovery ──────────────────────────────────────────────────────
	market, err := discoverMarket(ctx, apiClient, cfg)
	if err != nil {
		return fmt.Errorf("discover market: %w", err)
	}

	slog.Info("market config loaded",
		"market_addr", market.MarketAddr,
		"tick_size", market.TickSize,
		"lot_size", market.LotSize,
		"px_decimals", market.PxDecimals,
		"sz_decimals", market.SzDecimals,
	)

	b := &bot{
		cfg:             cfg,
		api:             apiClient,
		aptos:           aptosClient,
		market:          market,
		effectiveSpread: cfg.Spread,
		firstCycle:      true,
	}
	return b.runLoop(ctx)
}

// ─────────────────────────────────────────────────────────────────────────────

type bot struct {
	cfg    *config.Config
	api    *api.Client
	aptos  *aptos.Client
	market *api.MarketConfig
	// adaptive spread state
	effectiveSpread float64
	lastInventory   float64
	noFillCycles    int
	firstCycle      bool
}

func (b *bot) runLoop(ctx context.Context) error {
	for cycle := uint64(1); ; cycle++ {
		slog.Info("─── cycle start ────────────────────────────────────", "cycle", cycle)

		if err := b.runCycle(ctx); err != nil {
			slog.Error("cycle failed", "cycle", cycle, "err", err)
		}

		slog.Info("sleeping", "seconds", b.cfg.RefreshInterval)
		select {
		case <-ctx.Done():
			slog.Info("shutting down")
			return nil
		case <-time.After(time.Duration(b.cfg.RefreshInterval * float64(time.Second))):
		}
	}
}

func (b *bot) runCycle(ctx context.Context) error {
	// ── 1. Parallel state fetch ───────────────────────────────────────────────
	state, err := b.api.FetchState(ctx, b.cfg.SubaccountAddress, b.market.MarketAddr)
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

	// ── 2. Adaptive spread: fill detection ───────────────────────────────────
	if !b.firstCycle {
		invChange := math.Abs(state.Inventory - b.lastInventory)
		fillDetected := invChange > b.market.LotSize*0.5
		if fillDetected {
			// Fill happened — spread is working; widen slightly back toward initial
			b.noFillCycles = 0
			newSpread := math.Min(b.effectiveSpread+b.cfg.SpreadStep*0.5, b.cfg.Spread)
			if newSpread > b.effectiveSpread {
				b.effectiveSpread = newSpread
				slog.Info("fill detected, spread widened slightly",
					"inv_change", invChange, "spread", b.effectiveSpread)
			} else {
				slog.Info("fill detected", "inv_change", invChange, "spread", b.effectiveSpread)
			}
		} else {
			b.noFillCycles++
			if b.noFillCycles >= b.cfg.SpreadNoFillCycles {
				suggested := math.Max(b.effectiveSpread-b.cfg.SpreadStep, b.cfg.SpreadMin)
				if suggested < b.effectiveSpread {
					if b.cfg.AutoSpread {
						b.effectiveSpread = suggested
						b.noFillCycles = 0
						slog.Warn("auto-spread narrowed (no fill)",
							"spread", b.effectiveSpread,
							"no_fill_cycles", b.cfg.SpreadNoFillCycles)
					} else {
						slog.Warn("suggestion: spread may be too wide",
							"current_spread", b.effectiveSpread,
							"suggested_spread", suggested,
							"no_fill_cycles", b.noFillCycles,
							"tip", "add --auto-spread to automate")
					}
				}
			}
		}
	}
	b.lastInventory = state.Inventory
	b.firstCycle = false

	// ── 3. Risk guard: margin ─────────────────────────────────────────────────
	if state.MarginUsage > b.cfg.MaxMarginUsage {
		slog.Warn("PAUSED: margin usage too high",
			"margin_usage", state.MarginUsage,
			"threshold", b.cfg.MaxMarginUsage,
		)
		return nil
	}

	// ── 3. Risk guard: no price ───────────────────────────────────────────────
	if state.Mid == nil {
		slog.Warn("PAUSED: no mid price available")
		return nil
	}
	mid := *state.Mid

	// ── 4. Compute quotes ─────────────────────────────────────────────────────
	quotes, err := pricing.ComputeQuotes(
		mid,
		state.Inventory,
		b.effectiveSpread,
		b.cfg.SkewPerUnit,
		b.cfg.MaxInventory,
		b.market.TickSize,
		b.market.LotSize,
		b.market.MinSize,
		b.cfg.OrderSize,
	)
	if err != nil {
		return fmt.Errorf("compute quotes: %w", err)
	}

	if quotes == nil {
		// Inventory at limit.
		slog.Info("inventory at limit, cancelling all orders",
			"inventory", state.Inventory, "max", b.cfg.MaxInventory)

		if _, _, err := b.cancelAllOrders(ctx, state.OpenOrders); err != nil {
			return fmt.Errorf("cancel all orders: %w", err)
		}
		if b.cfg.AutoFlatten {
			if err := b.placeFlattenOrder(ctx, state.Inventory, mid); err != nil {
				return fmt.Errorf("flatten order: %w", err)
			}
		} else {
			slog.Warn("at max inventory; manually flatten or enable --auto-flatten")
		}
		return nil
	}

	slog.Info("computed quotes", "bid", quotes.Bid, "ask", quotes.Ask, "size", quotes.Size)

	// ── 5. Cancel all resting orders ──────────────────────────────────────────
	nOK, nFail, err := b.cancelAllOrders(ctx, state.OpenOrders)
	if err != nil {
		return fmt.Errorf("cancel all orders: %w", err)
	}
	if nOK > 0 || nFail > 0 {
		slog.Info("cancel results", "cancelled", nOK, "failed", nFail)
	}

	if nFail > 0 {
		slog.Warn("failed cancels, waiting for chain resync", "wait_s", b.cfg.CancelResyncS)
		select {
		case <-ctx.Done():
			return ctx.Err()
		case <-time.After(time.Duration(b.cfg.CancelResyncS * float64(time.Second))):
		}

		freshOrders, err := b.api.FetchOpenOrders(ctx, b.cfg.SubaccountAddress)
		if err != nil {
			return fmt.Errorf("re-fetch open orders: %w", err)
		}
		var stillOpen []api.OpenOrder
		for _, o := range freshOrders {
			if api.AddrEqual(o.MarketAddr, b.market.MarketAddr) {
				stillOpen = append(stillOpen, o)
			}
		}
		if len(stillOpen) > 0 {
			slog.Warn("still have open orders after resync, skipping cycle",
				"count", len(stillOpen))
			return nil
		}
	}

	// ── 6. Place bid ──────────────────────────────────────────────────────────
	if err := b.placePostOnlyOrder(ctx, quotes.Bid, quotes.Size, true); err != nil {
		return fmt.Errorf("place bid: %w", err)
	}

	// ── 7. Cooldown ───────────────────────────────────────────────────────────
	select {
	case <-ctx.Done():
		return ctx.Err()
	case <-time.After(time.Duration(b.cfg.CooldownS * float64(time.Second))):
	}

	// ── 8. Place ask ──────────────────────────────────────────────────────────
	if err := b.placePostOnlyOrder(ctx, quotes.Ask, quotes.Size, false); err != nil {
		return fmt.Errorf("place ask: %w", err)
	}

	slog.Info("cycle complete")
	return nil
}

// ── Order management ──────────────────────────────────────────────────────────

// cancelAllOrders cancels every order in the list.
// Returns (nSucceeded, nFailed, error).  Individual cancel failures are counted
// in nFailed rather than returned as errors.
func (b *bot) cancelAllOrders(ctx context.Context, orders []api.OpenOrder) (nOK, nFail int, _ error) {
	if len(orders) == 0 {
		return 0, 0, nil
	}
	slog.Info("cancelling all resting orders", "count", len(orders))
	for _, o := range orders {
		if err := b.cancelSingleOrder(ctx, o.OrderID); err != nil {
			slog.Warn("cancel failed", "order_id", o.OrderID, "err", err)
			nFail++
		} else {
			nOK++
		}
	}
	return nOK, nFail, nil
}

func (b *bot) cancelSingleOrder(ctx context.Context, orderID string) error {
	fn := b.cfg.PackageAddress + "::dex_accounts_entry::cancel_order_to_subaccount"

	slog.Info("cancelling order", "order_id", orderID, "dry_run", b.cfg.DryRun)
	if b.cfg.DryRun {
		return nil
	}

	result, err := b.aptos.SubmitEntryFunction(ctx, fn, nil, []any{
		b.cfg.SubaccountAddress, // 1. subaccount_addr
		orderID,                 // 2. order_id (u128 as string)
		b.market.MarketAddr,     // 3. market_addr
	})
	if err != nil {
		slog.Error("cancel transaction failed", "order_id", orderID, "err", err, "sender", b.aptos.SenderAddress())
		return err
	}
	if !result.CancelSucceeded() {
		slog.Error("cancel rejected", "order_id", orderID, "vm_status", result.VMStatus, "sender", b.aptos.SenderAddress())
		return fmt.Errorf("cancel rejected: vm_status=%s", result.VMStatus)
	}
	slog.Debug("cancel accepted", "order_id", orderID, "vm_status", result.VMStatus, "sender", b.aptos.SenderAddress())
	return nil
}

// placePostOnlyOrder places a POST_ONLY limit order (time_in_force = 1).
func (b *bot) placePostOnlyOrder(ctx context.Context, price, size float64, isBuy bool) error {
	side := "ASK"
	if isBuy {
		side = "BID"
	}
	priceInt := scalePrice(price, b.market.PxDecimals)
	sizeInt := scaleSize(size, b.market.SzDecimals)

	slog.Info("placing POST_ONLY order",
		"side", side, "price", price, "size", size,
		"price_int", priceInt, "size_int", sizeInt,
		"dry_run", b.cfg.DryRun,
	)
	if b.cfg.DryRun {
		return nil
	}

	fn := b.cfg.PackageAddress + "::dex_accounts_entry::place_order_to_subaccount"
	result, err := b.aptos.SubmitEntryFunction(ctx, fn, nil,
		buildPlaceOrderArgs(
			b.cfg.SubaccountAddress,
			b.market.MarketAddr,
			priceInt, sizeInt,
			isBuy,
			1,     // POST_ONLY
			false, // not reduce-only
		),
	)
	if err != nil {
		slog.Error("place order submit failed",
			"side", side, "price", price, "size", size,
			"price_int", priceInt, "size_int", sizeInt,
			"err", err, "sender", b.aptos.SenderAddress(),
		)
		return err
	}
	if !result.Success {
		slog.Error("place order rejected",
			"side", side, "price", price, "size", size,
			"vm_status", result.VMStatus, "tx_hash", result.Hash, "sender", b.aptos.SenderAddress(),
		)
		return fmt.Errorf("place order failed: side=%s vm_status=%s", side, result.VMStatus)
	}
	slog.Info("order placed",
		"side", side, "price", price, "size", size, "tx_hash", result.Hash, "sender", b.aptos.SenderAddress(),
	)
	return nil
}

// placeFlattenOrder places a reduce-only GTC limit order to close an oversized position.
func (b *bot) placeFlattenOrder(ctx context.Context, inventory, mid float64) error {
	isBuy := inventory < 0 // buy to close a short
	absInv := math.Abs(inventory)

	var rawPrice float64
	if isBuy {
		p := mid * (1.0 + b.cfg.FlattenAggression)
		rawPrice = math.Ceil(p/b.market.TickSize) * b.market.TickSize
	} else {
		p := mid * (1.0 - b.cfg.FlattenAggression)
		rawPrice = math.Floor(p/b.market.TickSize) * b.market.TickSize
	}

	size := math.Round(absInv/b.market.LotSize) * b.market.LotSize
	if size <= 0 || size < b.market.MinSize {
		slog.Warn("flatten size too small, skipping",
			"size", size, "min_size", b.market.MinSize)
		return nil
	}

	priceInt := scalePrice(rawPrice, b.market.PxDecimals)
	sizeInt := scaleSize(size, b.market.SzDecimals)

	side := "SELL"
	if isBuy {
		side = "BUY"
	}
	slog.Info("placing reduce-only GTC flatten order",
		"side", side, "price", rawPrice, "size", size,
		"dry_run", b.cfg.DryRun,
	)
	if b.cfg.DryRun {
		return nil
	}

	fn := b.cfg.PackageAddress + "::dex_accounts_entry::place_order_to_subaccount"
	result, err := b.aptos.SubmitEntryFunction(ctx, fn, nil,
		buildPlaceOrderArgs(
			b.cfg.SubaccountAddress,
			b.market.MarketAddr,
			priceInt, sizeInt,
			isBuy,
			0,    // GTC
			true, // reduce-only
		),
	)
	if err != nil {
		slog.Error("flatten submit failed",
			"side", side, "price", rawPrice, "size", size,
			"err", err, "sender", b.aptos.SenderAddress(),
		)
		return err
	}
	if !result.Success {
		slog.Error("flatten order rejected",
			"side", side, "price", rawPrice, "size", size,
			"vm_status", result.VMStatus, "tx_hash", result.Hash, "sender", b.aptos.SenderAddress(),
		)
		return fmt.Errorf("flatten order failed: vm_status=%s", result.VMStatus)
	}
	slog.Info("flatten order placed", "side", side, "price", rawPrice, "size", size, "tx_hash", result.Hash, "sender", b.aptos.SenderAddress())
	return nil
}

// ─────────────────────────────────────────────────────────────────────────────
// ABI argument builder for place_order_to_subaccount
// ─────────────────────────────────────────────────────────────────────────────

// buildPlaceOrderArgs constructs the 15 ABI arguments for place_order_to_subaccount.
// All Option<u64> / Option<address> fields use the {"vec": []} encoding.
func buildPlaceOrderArgs(
	subaccountAddr, marketAddr string,
	priceInt, sizeInt uint64,
	isBuy bool,
	timeInForce uint8, // 0=GTC, 1=POST_ONLY, 2=IOC
	isReduceOnly bool,
) []any {
	none := func() map[string][]any { return map[string][]any{"vec": {}} }

	return []any{
		subaccountAddr,              //  1. subaccount_addr
		marketAddr,                  //  2. market_addr
		fmt.Sprintf("%d", priceInt), //  3. price (u64 as string)
		fmt.Sprintf("%d", sizeInt),  //  4. size  (u64 as string)
		isBuy,                       //  5. is_buy
		int(timeInForce),            //  6. time_in_force (u8 as integer)
		isReduceOnly,                //  7. is_reduce_only
		none(),                      //  8. client_order_id  Option<String>
		none(),                      //  9. stop_price       Option<u64>
		none(),                      // 10. tp_trigger        Option<u64>
		none(),                      // 11. tp_limit           Option<u64>
		none(),                      // 12. sl_trigger         Option<u64>
		none(),                      // 13. sl_limit            Option<u64>
		none(),                      // 14. builder_addr    Option<address>
		none(),                      // 15. builder_fees       Option<u64>
	}
}

// ─────────────────────────────────────────────────────────────────────────────
// Scaling helpers
// ─────────────────────────────────────────────────────────────────────────────

func scalePrice(price float64, pxDecimals int) uint64 {
	return uint64(math.Round(price * math.Pow10(pxDecimals)))
}

func scaleSize(size float64, szDecimals int) uint64 {
	return uint64(math.Round(size * math.Pow10(szDecimals)))
}

// ─────────────────────────────────────────────────────────────────────────────
// Startup helpers
// ─────────────────────────────────────────────────────────────────────────────

func discoverMarket(ctx context.Context, apiClient *api.Client, cfg *config.Config) (*api.MarketConfig, error) {
	if cfg.MarketAddrOverride != "" {
		m, err := apiClient.FindMarket(ctx, cfg.MarketName)
		if err != nil {
			slog.Warn("could not fetch market metadata, using override with defaults", "err", err)
			return &api.MarketConfig{
				MarketAddr: cfg.MarketAddrOverride,
				MarketName: cfg.MarketName,
				TickSize:   1.0,
				LotSize:    0.00001,
				MinSize:    0.00002,
				PxDecimals: 2,
				SzDecimals: 5,
			}, nil
		}
		m.MarketAddr = cfg.MarketAddrOverride
		return m, nil
	}
	return apiClient.FindMarket(ctx, cfg.MarketName)
}
