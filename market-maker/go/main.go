package main

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"log/slog"
	"math"
	"os"
	"os/signal"
	"path/filepath"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/joho/godotenv"
	"golang.org/x/sync/errgroup"

	"decibel-mm-bot/api"
	"decibel-mm-bot/aptos"
	"decibel-mm-bot/botstate"
	"decibel-mm-bot/config"
	"decibel-mm-bot/exchange"
	decibelExchange "decibel-mm-bot/exchange/decibel"
	"decibel-mm-bot/logging"
	"decibel-mm-bot/notify"
	"decibel-mm-bot/notify/telegram"
	"decibel-mm-bot/strategy"
)

func main() {
	envLoaded := godotenv.Load() == nil

	args := config.NormalizeBoolCLIArgsForInit(os.Args)
	if out, ok := config.ParseInitConfigFlag(args); ok {
		logging.Setup(os.Stderr, nil, nil)
		if err := config.WriteInitConfigYAML(out); err != nil {
			slog.Error("init-config failed", "err", err)
			os.Exit(1)
		}
		fmt.Fprintf(os.Stdout, "wrote default YAML config to %s\n", out)
		os.Exit(0)
	}

	cfg, err := config.Load()
	if err != nil {
		if errors.Is(err, flag.ErrHelp) {
			os.Exit(0)
		}
		logging.Setup(os.Stderr, nil, nil)
		slog.Error("config error", "err", err)
		os.Exit(1)
	}
	logging.Setup(os.Stderr, cfg, nil)
	if envLoaded {
		slog.Info("loaded .env file")
	}
	slog.Info("network profile",
		"network", cfg.Network,
		"rest_api", cfg.RestAPIBase,
		"fullnode", cfg.AptosFullnodeURL,
	)

	// Graceful shutdown on SIGINT / SIGTERM.
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()

	var logTeeF *os.File
	defer func() {
		if logTeeF != nil {
			_ = logTeeF.Close()
		}
	}()

	// ── 1. Exchange layer ────────────────────────────────────────────────────
	slog.Info("starting Decibel Market Maker",
		"market", cfg.MarketName,
		"spread", cfg.Spread,
		"order_size", cfg.OrderSize,
		"max_inventory", cfg.MaxInventory,
		"dry_run", cfg.DryRun,
	)

	if perpGlobal, err := aptos.CreatePerpEngineGlobalAddress(cfg.PackageAddress); err != nil {
		slog.Warn("could not derive GlobalPerpEngine address for logging", "err", err)
	} else {
		slog.Info("perp engine global (derived)", "address", perpGlobal)
	}

	ex, err := decibelExchange.New(cfg)
	if err != nil {
		slog.Error("exchange init failed", "err", err)
		os.Exit(1)
	}

	market, err := ex.FindMarket(ctx, cfg.MarketName)
	if err != nil {
		slog.Error("discover market failed", "err", err)
		os.Exit(1)
	}
	ex.SetMarket(market)

	if teePath, err := resolveLogTeePath(cfg, market); err != nil {
		slog.Error("log tee path invalid", "err", err)
		os.Exit(1)
	} else if teePath != "" {
		if d := filepath.Dir(teePath); d != "." && d != "" {
			if err := os.MkdirAll(d, 0o755); err != nil {
				slog.Error("log tee mkdir", "dir", d, "err", err)
				os.Exit(1)
			}
		}
		f, err := os.OpenFile(teePath, os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o644)
		if err != nil {
			slog.Error("log tee open file", "path", teePath, "err", err)
			os.Exit(1)
		}
		logTeeF = f
		logging.Setup(os.Stderr, cfg, f)
		slog.Info("log tee enabled", "path", teePath)
	}

	slog.Info("market config loaded",
		"market_id", market.MarketID,
		"tick_size", market.TickSize,
		"lot_size", market.LotSize,
		"px_decimals", market.PxDecimals,
		"sz_decimals", market.SzDecimals,
	)
	slog.Info("using subaccount", "address", cfg.SubaccountAddress)

	// Global market catalog: on-chain market address -> name from GET /markets (multi-market
	// display, notifications, etc.). Uses the same REST client as the exchange; cached in api.Client.
	nameLookup := map[string]string(nil)
	if mkts, err := ex.MarketsCatalog(ctx); err != nil {
		slog.Warn("fetch markets catalog failed", "err", err)
	} else {
		nameLookup = buildMarketNameLookup(mkts)
	}
	apiCatalog := decibelExchange.APIClient(ex)

	// ── 2. Strategy layer ────────────────────────────────────────────────────
	mm := strategy.New(cfg, ex, market)

	// ── 3. Notification layer (optional) ─────────────────────────────────────
	if cfg.TelegramEnabled() {
		tgCfg := telegram.Config{
			BotToken:               cfg.TGBotToken,
			AdminID:                cfg.TGAdminID,
			AlertInventory:         cfg.TGAlertInventory,
			AlertInventoryInterval: cfg.TGAlertInventoryInterval,
			Locale:                 cfg.Locale,
		}
		info := &infoAdapter{mm: mm, ex: ex, cfg: cfg, apiClient: apiCatalog, marketNames: nameLookup}
		tg, err := telegram.New(tgCfg, info)
		if err != nil {
			if cfg.TGStrictStart {
				slog.Error("telegram bot init failed (tg-strict-start)", "err", err)
				os.Exit(1)
			}
			slog.Warn("telegram bot init failed, continuing without it", "err", err)
		} else {
			if err := tg.Ready(ctx); err != nil {
				if cfg.TGStrictStart {
					slog.Error("telegram ready failed (tg-strict-start)", "err", err)
					os.Exit(1)
				}
				slog.Warn("telegram ready failed, continuing without bot", "err", err)
			} else {
				if err := tg.SetBotCommands(ctx); err != nil {
					if cfg.TGStrictStart {
						slog.Error("telegram set commands failed (tg-strict-start)", "err", err)
						os.Exit(1)
					}
					slog.Warn("telegram set commands failed", "err", err)
				}
				slog.Info("telegram bot starting update loop")
				go func() {
					if err := tg.Run(ctx); err != nil && ctx.Err() == nil {
						slog.Error("telegram bot exited with error", "err", err)
					}
				}()
			}
		}
	}

	// ── Run strategy ─────────────────────────────────────────────────────────
	if err := mm.Run(ctx); err != nil {
		slog.Error("strategy exited with error", "err", err)
		os.Exit(1)
	}
}

// resolveLogTeePath returns an absolute or cwd-relative log file path, or "" if tee is disabled.
func resolveLogTeePath(cfg *config.Config, market *exchange.MarketConfig) (string, error) {
	raw := strings.TrimSpace(cfg.LogTeeFile)
	if raw == "" {
		return "", nil
	}
	if strings.EqualFold(raw, "auto") {
		name := strings.TrimSpace(market.MarketName)
		if name == "" {
			name = cfg.MarketName
		}
		return logging.TeeAutoPath(cfg.LogTeeFileDir, cfg.SubaccountAddress, name)
	}
	return filepath.Clean(raw), nil
}

func buildMarketNameLookup(markets []api.MarketConfig) map[string]string {
	m := make(map[string]string, len(markets))
	for _, mk := range markets {
		k := api.NormalizeAddr(mk.MarketAddr)
		if k == "" || mk.MarketName == "" {
			continue
		}
		m[k] = mk.MarketName
	}
	return m
}

// infoAdapter bridges the strategy and exchange layers to implement
// notify.InfoProvider for the notification layer.
type infoAdapter struct {
	mm          *strategy.MarketMaker
	ex          exchange.Exchange
	cfg         *config.Config
	apiClient   *api.Client
	marketNames map[string]string // api.NormalizeAddr -> market_name
}

var _ notify.InfoProvider = (*infoAdapter)(nil)

func (a *infoAdapter) GetSnapshot() botstate.Snapshot {
	return a.mm.State().Get()
}

func (a *infoAdapter) FetchLiveSnapshot(ctx context.Context) (botstate.Snapshot, error) {
	state, err := a.ex.FetchState(ctx)
	if err != nil {
		return botstate.Snapshot{}, err
	}
	base := a.mm.State().Get()

	positions := make([]botstate.Position, len(state.AllPositions))
	for i, p := range state.AllPositions {
		positions[i] = botstate.Position{
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

	var midCopy *float64
	if state.Mid != nil {
		v := *state.Mid
		midCopy = &v
	}
	midByMarket := a.fetchPositionMids(ctx, positions, base.TargetMarketID, midCopy)

	return botstate.Snapshot{
		Equity:           state.Equity,
		MarginUsage:      state.MarginUsage,
		Inventory:        state.Inventory,
		Mid:              midCopy,
		MidByMarket:      midByMarket,
		AllPositions:     positions,
		EntryPrice:       base.EntryPrice,
		TargetMarketName: base.TargetMarketName,
		TargetMarketID:   base.TargetMarketID,
		LastCycleAt:      time.Now(),
	}, nil
}

const maxLiveSnapshotPriceFetches = 8

func (a *infoAdapter) fetchPositionMids(
	ctx context.Context,
	positions []botstate.Position,
	targetMarketID string,
	targetMid *float64,
) map[string]float64 {
	mids := make(map[string]float64)
	targetKey := api.NormalizeAddr(targetMarketID)
	if targetKey != "" && targetMid != nil {
		mids[targetKey] = *targetMid
	}
	if a.apiClient == nil {
		return mids
	}

	var markets []string
	seen := make(map[string]struct{})
	if targetKey != "" {
		seen[targetKey] = struct{}{}
	}
	for _, p := range positions {
		if p.IsDeleted || math.Abs(p.Size) < 1e-9 {
			continue
		}
		key := api.NormalizeAddr(p.MarketID)
		if key == "" {
			continue
		}
		if _, ok := seen[key]; ok {
			continue
		}
		seen[key] = struct{}{}
		markets = append(markets, p.MarketID)
	}
	if len(markets) == 0 {
		return mids
	}

	var mu sync.Mutex
	g, gctx := errgroup.WithContext(ctx)
	g.SetLimit(maxLiveSnapshotPriceFetches)
	for _, marketID := range markets {
		marketID := marketID
		g.Go(func() error {
			price, err := a.apiClient.FetchPrice(gctx, marketID)
			if err != nil {
				if gctx.Err() == nil {
					slog.Warn("fetch live mid for display failed", "market", marketID, "err", err)
				}
				return nil
			}
			mid := price.Mid()
			if mid == nil {
				return nil
			}
			mu.Lock()
			mids[api.NormalizeAddr(marketID)] = *mid
			mu.Unlock()
			return nil
		})
	}
	_ = g.Wait()
	return mids
}

func (a *infoAdapter) FlattenPosition(ctx context.Context) (exchange.PlaceOrderOutcome, error) {
	return a.mm.FlattenPosition(ctx)
}

func (a *infoAdapter) DryRun() bool {
	return a.ex.DryRun()
}

func (a *infoAdapter) FetchTradeHistoryByOrder(ctx context.Context, marketAddr, orderID string) ([]api.TradeHistoryItem, error) {
	return a.ex.FetchTradeHistory(ctx, api.TradeHistoryParams{
		Account: a.cfg.SubaccountAddress,
		Market:  marketAddr,
		OrderID: orderID,
		Limit:   100,
	})
}

func (a *infoAdapter) FetchRecentTrades(ctx context.Context, limit int) ([]api.TradeHistoryItem, error) {
	market := a.mm.State().Get().TargetMarketID
	if market == "" {
		return nil, fmt.Errorf("target market not set")
	}
	if limit <= 0 {
		limit = 10
	}
	if limit > 200 {
		limit = 200
	}
	p := api.TradeHistoryParams{
		Account: a.cfg.SubaccountAddress,
		Market:  market,
		Limit:   limit,
		SortKey: "timestamp",
		SortDir: "DESC",
	}
	items, err := a.ex.FetchTradeHistory(ctx, p)
	if err != nil {
		p.SortKey, p.SortDir = "", ""
		items, err = a.ex.FetchTradeHistory(ctx, p)
	}
	return items, err
}

func (a *infoAdapter) GasBalance(ctx context.Context) (float64, string, error) {
	return a.ex.GasBalance(ctx)
}

func (a *infoAdapter) WalletAddress() string {
	return a.ex.WalletAddress()
}

func (a *infoAdapter) MaxInventory() float64 {
	return a.cfg.MaxInventory
}

func (a *infoAdapter) MarketDisplayName(addr string) string {
	if strings.TrimSpace(addr) == "" {
		return ""
	}
	key := api.NormalizeAddr(addr)
	if a.marketNames != nil {
		if n, ok := a.marketNames[key]; ok && n != "" {
			return n
		}
	}
	return notify.ShortAddrForDisplay(addr)
}
