// Package decibel implements the exchange.Exchange interface for the Decibel DEX
// on Aptos. It composes the existing api and aptos packages.
package decibel

import (
	"context"
	"fmt"
	"log/slog"
	"math"
	"sync"
	"time"

	aptossdk "github.com/aptos-labs/aptos-go-sdk"

	"decibel-mm-bot/api"
	"decibel-mm-bot/aptos"
	"decibel-mm-bot/config"
	"decibel-mm-bot/exchange"
)

// DecibelExchange implements exchange.Exchange for Decibel DEX on Aptos.
type DecibelExchange struct {
	cfg         *config.Config
	apiClient   *api.Client
	aptosNode   *aptos.NodeClient
	aptosSigner *aptossdk.Account
	market      *exchange.MarketConfig
	walletAddr  string
	dryRun      bool

	// bulkSeq is the last sequence_number used in a live place_bulk_orders call (or max from
	// GET /bulk_orders after sync). Each live PlaceBulkOrders does bulkSeq++ before submit.
	// It is never decremented so retries never reuse a consumed or ambiguous seq.
	bulkSeq uint64

	bulkMu        sync.Mutex
	bulkSeqSynced bool
}

// New creates a DecibelExchange, initialising the Decibel REST client, Aptos
// node client, and signing account from the provided config.
func New(cfg *config.Config) (*DecibelExchange, error) {
	apiClient := api.NewClient(cfg.RestAPIBase, cfg.BearerToken)

	aptosNode, err := aptos.NewNodeClient(cfg.AptosFullnodeURL, cfg.NodeKey(), aptos.ChainIDForNetwork(cfg.Network))
	if err != nil {
		return nil, fmt.Errorf("create aptos node client: %w", err)
	}
	aptosSigner, err := aptos.ParseAccount(cfg.PrivateKey)
	if err != nil {
		return nil, fmt.Errorf("parse aptos signing account: %w", err)
	}

	addr := aptosSigner.AccountAddress()
	slog.Info("derived sender address", "address", addr.String())

	return &DecibelExchange{
		cfg:         cfg,
		apiClient:   apiClient,
		aptosNode:   aptosNode,
		aptosSigner: aptosSigner,
		walletAddr:  addr.String(),
		dryRun:      cfg.DryRun,
	}, nil
}

// ── exchange.Exchange implementation ─────────────────────────────────────────

// FindMarket discovers market metadata by name from the Decibel REST API.
func (d *DecibelExchange) FindMarket(ctx context.Context, name string) (*exchange.MarketConfig, error) {
	if d.cfg.MarketAddrOverride != "" {
		m, err := d.apiClient.FindMarket(ctx, name)
		if err != nil {
			slog.Warn("could not fetch market metadata, using override with defaults", "err", err)
			return &exchange.MarketConfig{
				MarketID:   d.cfg.MarketAddrOverride,
				MarketName: name,
				TickSize:   1.0,
				LotSize:    0.00001,
				MinSize:    0.00002,
				PxDecimals: 2,
				SzDecimals: 5,
			}, nil
		}
		mc := apiMarketToExchange(m)
		mc.MarketID = d.cfg.MarketAddrOverride
		return mc, nil
	}

	m, err := d.apiClient.FindMarket(ctx, name)
	if err != nil {
		return nil, err
	}
	return apiMarketToExchange(m), nil
}

// MarketsCatalog implements exchange.Exchange: full venue market list from GET /markets
// (cached on the underlying api.Client after the first successful fetch).
func (d *DecibelExchange) MarketsCatalog(ctx context.Context) ([]api.MarketConfig, error) {
	return d.apiClient.FetchMarkets(ctx)
}

// SetMarket configures the target market for subsequent FetchState / order calls.
func (d *DecibelExchange) SetMarket(m *exchange.MarketConfig) {
	d.bulkMu.Lock()
	defer d.bulkMu.Unlock()
	d.market = m
	d.bulkSeqSynced = false
	d.bulkSeq = 0
}

// FetchState fetches the full state snapshot for one cycle.
func (d *DecibelExchange) FetchState(ctx context.Context) (*exchange.StateSnapshot, error) {
	if d.market == nil {
		return nil, fmt.Errorf("market not set: call SetMarket first")
	}
	snap, err := d.apiClient.FetchState(ctx, d.cfg.SubaccountAddress, d.market.MarketID)
	if err != nil {
		return nil, fmt.Errorf("fetch state: %w", err)
	}
	return apiStateToExchange(snap), nil
}

// FetchOpenOrders fetches open orders (used for resync after cancel failures).
func (d *DecibelExchange) FetchOpenOrders(ctx context.Context) ([]exchange.OpenOrder, error) {
	if d.market == nil {
		return nil, fmt.Errorf("market not set: call SetMarket first")
	}
	orders, err := d.apiClient.FetchOpenOrders(ctx, d.cfg.SubaccountAddress)
	if err != nil {
		return nil, err
	}
	// Filter to target market.
	var result []exchange.OpenOrder
	for _, o := range orders {
		if api.AddrEqual(o.MarketAddr, d.market.MarketID) {
			result = append(result, exchange.OpenOrder{
				OrderID:  o.OrderID,
				MarketID: o.MarketAddr,
			})
		}
	}
	return result, nil
}

// PlaceOrder places a limit order on Decibel via Aptos entry function.
func (d *DecibelExchange) PlaceOrder(ctx context.Context, req exchange.PlaceOrderRequest) (exchange.PlaceOrderOutcome, error) {
	if d.market == nil {
		return exchange.PlaceOrderOutcome{}, fmt.Errorf("market not set: call SetMarket first")
	}
	side := "ASK"
	if req.IsBuy {
		side = "BID"
	}
	priceInt := scalePrice(req.Price, d.market.PxDecimals)
	sizeInt := scaleSize(req.Size, d.market.SzDecimals)

	label := "POST_ONLY"
	if req.ReduceOnly {
		label = "reduce-only GTC"
	}
	slog.Info(fmt.Sprintf("placing %s order", label),
		"side", side, "price", req.Price, "size", req.Size,
		"price_int", priceInt, "size_int", sizeInt,
		"dry_run", d.dryRun,
	)
	if d.dryRun {
		return exchange.PlaceOrderOutcome{}, nil
	}

	fn := d.cfg.PackageAddress + "::dex_accounts_entry::place_order_to_subaccount"
	result, err := d.aptosNode.SubmitEntryFunction(ctx, d.aptosSigner, fn, nil,
		buildPlaceOrderArgs(
			d.cfg.SubaccountAddress,
			d.market.MarketID,
			priceInt, sizeInt,
			req.IsBuy,
			uint8(req.TimeInForce),
			req.ReduceOnly,
		),
	)
	if err != nil {
		if result != nil && result.Hash != "" {
			slog.Warn("place order: tx submitted but confirmation incomplete", "tx_hash", result.Hash, "err", err)
		}
		return exchange.PlaceOrderOutcome{}, err
	}
	if !result.Success {
		return exchange.PlaceOrderOutcome{}, fmt.Errorf("place order failed: side=%s vm_status=%s", side, result.VMStatus)
	}
	oid := aptos.OrderIDFromEvents(result.Events)
	if oid != "" {
		slog.Info("order placed", "side", side, "price", req.Price, "size", req.Size, "tx_hash", result.Hash, "order_id", oid)
	} else {
		slog.Info("order placed", "side", side, "price", req.Price, "size", req.Size, "tx_hash", result.Hash)
	}
	return exchange.PlaceOrderOutcome{TxHash: result.Hash, OrderID: oid}, nil
}

// FetchTradeHistory implements exchange.Exchange.
func (d *DecibelExchange) FetchTradeHistory(ctx context.Context, p api.TradeHistoryParams) ([]api.TradeHistoryItem, error) {
	return d.apiClient.FetchTradeHistory(ctx, p)
}

// syncBulkSeqFromREST sets d.bulkSeq to max(sequence_number) from GET /bulk_orders
// so the next live PlaceBulkOrders uses max+1. Caller must hold d.bulkMu.
// Retries up to 3 times with quadratic backoff (1s, 4s, 9s) on transient failures.
func (d *DecibelExchange) syncBulkSeqFromREST(ctx context.Context) error {
	const maxAttempts = 3
	var lastErr error
	for attempt := 1; attempt <= maxAttempts; attempt++ {
		rows, err := d.apiClient.FetchBulkOrders(ctx, d.cfg.SubaccountAddress, d.market.MarketID)
		if err != nil {
			lastErr = err
			backoff := time.Duration(attempt*attempt) * time.Second
			slog.Warn("bulk sequence sync failed, retrying",
				"attempt", attempt,
				"max_attempts", maxAttempts,
				"backoff_s", backoff.Seconds(),
				"err", err,
			)
			select {
			case <-ctx.Done():
				return ctx.Err()
			case <-time.After(backoff):
			}
			continue
		}
		var maxSeq uint64
		for _, r := range rows {
			if r.SequenceNumber > maxSeq {
				maxSeq = r.SequenceNumber
			}
		}
		d.bulkSeq = maxSeq
		d.bulkSeqSynced = true
		slog.Info("bulk sequence synced from REST",
			"max_sequence_number", maxSeq,
			"next_bulk_seq", maxSeq+1,
			"rows", len(rows),
		)
		return nil
	}
	return fmt.Errorf("sync bulk sequence failed after %d attempts: %w", maxAttempts, lastErr)
}

// PlaceBulkOrders atomically replaces all bulk quotes for the target market.
// Bulk orders are POST_ONLY. Empty bids/asks clears that side.
func (d *DecibelExchange) PlaceBulkOrders(ctx context.Context, bids, asks []exchange.BulkOrderEntry) error {
	if d.market == nil {
		return fmt.Errorf("market not set: call SetMarket first")
	}

	d.bulkMu.Lock()
	defer d.bulkMu.Unlock()

	if !d.bulkSeqSynced {
		if err := d.syncBulkSeqFromREST(ctx); err != nil {
			return fmt.Errorf("sync bulk sequence from REST: %w", err)
		}
	}

	bidPrices := make([]string, len(bids))
	bidSizes := make([]string, len(bids))
	for i, b := range bids {
		bidPrices[i] = fmt.Sprintf("%d", scalePrice(b.Price, d.market.PxDecimals))
		bidSizes[i] = fmt.Sprintf("%d", scaleSize(b.Size, d.market.SzDecimals))
	}
	askPrices := make([]string, len(asks))
	askSizes := make([]string, len(asks))
	for i, a := range asks {
		askPrices[i] = fmt.Sprintf("%d", scalePrice(a.Price, d.market.PxDecimals))
		askSizes[i] = fmt.Sprintf("%d", scaleSize(a.Size, d.market.SzDecimals))
	}

	if d.dryRun {
		slog.Info("placing bulk orders (dry run)",
			"bids", len(bids), "asks", len(asks),
			"next_bulk_seq", d.bulkSeq+1,
		)
		return nil
	}

	d.bulkSeq++
	slog.Info("placing bulk orders",
		"bids", len(bids), "asks", len(asks),
		"bulk_seq", d.bulkSeq,
		"dry_run", d.dryRun,
	)

	fn := d.cfg.PackageAddress + "::dex_accounts_entry::place_bulk_orders_to_subaccount"
	slog.Info("submitting bulk orders", "bids", len(bids), "asks", len(asks), "bulk_seq", d.bulkSeq)
	result, err := d.aptosNode.SubmitEntryFunction(ctx, d.aptosSigner, fn, nil, []any{
		d.cfg.SubaccountAddress, //  1. subaccount
		d.market.MarketID,       //  2. market
		d.bulkSeq,               //  3. sequence_number (u64)
		bidPrices,               //  4. bid_prices vector<u64>
		bidSizes,                //  5. bid_sizes  vector<u64>
		askPrices,               //  6. ask_prices vector<u64>
		askSizes,                //  7. ask_sizes  vector<u64>
		nil,                     //  8. builder_address Option<address>
		nil,                     //  9. builder_fees    Option<u64>
	})
	if err != nil {
		if result != nil && result.Hash != "" {
			slog.Error("bulk orders: tx submitted but confirmation incomplete",
				"tx_hash", result.Hash, "bids", len(bids), "asks", len(asks), "err", err)
			return err
		}
		slog.Error("bulk orders submission failed", "bids", len(bids), "asks", len(asks), "err", err)
		return err
	}
	slog.Info("bulk orders submitted", "tx_hash", result.Hash)
	if !result.Success {
		slog.Error("bulk orders execution failed", "vm_status", result.VMStatus)
		return fmt.Errorf("place bulk orders failed: vm_status=%s", result.VMStatus)
	}
	slog.Info("bulk orders placed", "bids", len(bids), "asks", len(asks), "tx_hash", result.Hash)
	return nil
}

// CancelBulkOrders removes all bulk quotes for the target market in one transaction.
func (d *DecibelExchange) CancelBulkOrders(ctx context.Context) error {
	if d.market == nil {
		return fmt.Errorf("market not set: call SetMarket first")
	}

	rows, err := d.apiClient.FetchBulkOrders(ctx, d.cfg.SubaccountAddress, d.market.MarketID)
	if err != nil {
		return fmt.Errorf("fetch bulk orders before cancel: %w", err)
	}
	active := false
	for i := range rows {
		if rows[i].HasRestingQuotes() {
			active = true
			break
		}
	}
	if !active {
		slog.Info("skipping cancel bulk orders: no active bulk quotes on REST", "rows", len(rows))
		return nil
	}

	slog.Info("cancelling bulk orders", "dry_run", d.dryRun)
	if d.dryRun {
		return nil
	}

	fn := d.cfg.PackageAddress + "::dex_accounts_entry::cancel_bulk_order_to_subaccount"
	slog.Info("submitting cancel bulk orders")
	result, err := d.aptosNode.SubmitEntryFunction(ctx, d.aptosSigner, fn, nil, []any{
		d.cfg.SubaccountAddress,
		d.market.MarketID,
	})
	if err != nil {
		if result != nil && result.Hash != "" {
			slog.Warn("cancel bulk orders: tx submitted but confirmation incomplete", "tx_hash", result.Hash, "err", err)
		} else {
			slog.Error("cancel bulk orders submission failed", "err", err)
		}
		return err
	}
	slog.Info("cancel bulk orders submitted", "tx_hash", result.Hash)
	if !result.Success {
		slog.Error("cancel bulk orders execution failed", "vm_status", result.VMStatus)
		return fmt.Errorf("cancel bulk orders failed: vm_status=%s", result.VMStatus)
	}
	slog.Info("bulk orders cancelled", "tx_hash", result.Hash)
	return nil
}

// CancelOrder cancels a single order on Decibel via Aptos entry function.
func (d *DecibelExchange) CancelOrder(ctx context.Context, orderID string) error {
	if d.market == nil {
		return fmt.Errorf("market not set: call SetMarket first")
	}
	fn := d.cfg.PackageAddress + "::dex_accounts_entry::cancel_order_to_subaccount"

	slog.Info("cancelling order", "order_id", orderID, "dry_run", d.dryRun)
	if d.dryRun {
		return nil
	}

	result, err := d.aptosNode.SubmitEntryFunction(ctx, d.aptosSigner, fn, nil, []any{
		d.cfg.SubaccountAddress,
		orderID,
		d.market.MarketID,
	})
	if err != nil {
		if result != nil && result.Hash != "" {
			slog.Warn("cancel order: tx submitted but confirmation incomplete", "tx_hash", result.Hash, "order_id", orderID, "err", err)
		}
		return err
	}
	if !result.CancelSucceeded() {
		return fmt.Errorf("cancel rejected: vm_status=%s", result.VMStatus)
	}
	slog.Debug("cancel accepted", "order_id", orderID, "vm_status", result.VMStatus)
	return nil
}

// WalletAddress returns the Aptos signing wallet address.
func (d *DecibelExchange) WalletAddress() string { return d.walletAddr }

// GasBalance returns the APT balance of the signing wallet.
func (d *DecibelExchange) GasBalance(ctx context.Context) (float64, string, error) {
	bal, err := d.aptosNode.APTBalance(ctx, d.walletAddr)
	return bal, "APT", err
}

// DryRun reports whether the exchange is in dry-run mode.
func (d *DecibelExchange) DryRun() bool { return d.dryRun }

// ── Internal helpers ─────────────────────────────────────────────────────────

// buildPlaceOrderArgs constructs the 15 ABI arguments for place_order_to_subaccount.
func buildPlaceOrderArgs(
	subaccountAddr, marketAddr string,
	priceInt, sizeInt uint64,
	isBuy bool,
	timeInForce uint8,
	isReduceOnly bool,
) []any {
	return []any{
		subaccountAddr,              //  1. subaccount_addr
		marketAddr,                  //  2. market_addr
		fmt.Sprintf("%d", priceInt), //  3. price (u64 as string)
		fmt.Sprintf("%d", sizeInt),  //  4. size  (u64 as string)
		isBuy,                       //  5. is_buy
		timeInForce,                 //  6. time_in_force (u8)
		isReduceOnly,                //  7. is_reduce_only
		nil,                         //  8. client_order_id  Option<String>
		nil,                         //  9. stop_price       Option<u64>
		nil,                         // 10. tp_trigger        Option<u64>
		nil,                         // 11. tp_limit          Option<u64>
		nil,                         // 12. sl_trigger        Option<u64>
		nil,                         // 13. sl_limit          Option<u64>
		nil,                         // 14. builder_addr     Option<address>
		nil,                         // 15. builder_fees      Option<u64>
	}
}

func scalePrice(price float64, pxDecimals int) uint64 {
	return uint64(math.Round(price * math.Pow10(pxDecimals)))
}

func scaleSize(size float64, szDecimals int) uint64 {
	return uint64(math.Round(size * math.Pow10(szDecimals)))
}

// apiMarketToExchange converts an api.MarketConfig to exchange.MarketConfig.
func apiMarketToExchange(m *api.MarketConfig) *exchange.MarketConfig {
	return &exchange.MarketConfig{
		MarketID:                m.MarketAddr,
		MarketName:              m.MarketName,
		TickSize:                m.TickSize,
		LotSize:                 m.LotSize,
		MinSize:                 m.MinSize,
		PxDecimals:              m.PxDecimals,
		SzDecimals:              m.SzDecimals,
		MaxLeverage:             m.MaxLeverage,
		Mode:                    m.Mode,
		MaxOpenInterest:         m.MaxOpenInterest,
		UnrealizedPnlHaircutBps: m.UnrealizedPnlHaircutBps,
	}
}

// apiStateToExchange converts an api.StateSnapshot to exchange.StateSnapshot.
func apiStateToExchange(s *api.StateSnapshot) *exchange.StateSnapshot {
	positions := make([]exchange.Position, len(s.AllPositions))
	for i, p := range s.AllPositions {
		positions[i] = exchange.Position{
			MarketID:                  p.Market,
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
	orders := make([]exchange.OpenOrder, len(s.OpenOrders))
	for i, o := range s.OpenOrders {
		orders[i] = exchange.OpenOrder{
			OrderID:  o.OrderID,
			MarketID: o.MarketAddr,
		}
	}
	return &exchange.StateSnapshot{
		Equity:       s.Equity,
		MarginUsage:  s.MarginUsage,
		Inventory:    s.Inventory,
		Mid:          s.Mid,
		OpenOrders:   orders,
		AllPositions: positions,
	}
}

// APIClient returns the shared REST client used by ex when it is a *DecibelExchange; otherwise nil.
func APIClient(ex exchange.Exchange) *api.Client {
	d, ok := ex.(*DecibelExchange)
	if !ok || d == nil {
		return nil
	}
	return d.apiClient
}
