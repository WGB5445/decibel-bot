package main

import (
	"context"
	"log/slog"
	"os"
	"os/signal"
	"syscall"

	"github.com/joho/godotenv"

	"decibel-mm-bot/aptos"
	"decibel-mm-bot/botstate"
	"decibel-mm-bot/config"
	"decibel-mm-bot/exchange"
	decibelExchange "decibel-mm-bot/exchange/decibel"
	"decibel-mm-bot/notify"
	"decibel-mm-bot/notify/telegram"
	"decibel-mm-bot/strategy"
)

func main() {
	// Load .env before config so all env vars are visible to flag defaults.
	if err := godotenv.Load(); err == nil {
		slog.Info("loaded .env file")
	}

	cfg, err := config.Load()
	if err != nil {
		slog.Error("config error", "err", err)
		os.Exit(1)
	}
	slog.Info("network profile",
		"network", cfg.Network,
		"rest_api", cfg.RestAPIBase,
		"fullnode", cfg.AptosFullnodeURL,
	)

	// Graceful shutdown on SIGINT / SIGTERM.
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()

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

	slog.Info("market config loaded",
		"market_id", market.MarketID,
		"tick_size", market.TickSize,
		"lot_size", market.LotSize,
		"px_decimals", market.PxDecimals,
		"sz_decimals", market.SzDecimals,
	)
	slog.Info("using subaccount", "address", cfg.SubaccountAddress)

	// ── 2. Strategy layer ────────────────────────────────────────────────────
	mm := strategy.New(cfg, ex, market)

	// ── 3. Notification layer (optional) ─────────────────────────────────────
	if cfg.TelegramEnabled() {
		tgCfg := telegram.Config{
			BotToken:               cfg.TGBotToken,
			AdminID:                cfg.TGAdminID,
			AlertInventory:         cfg.TGAlertInventory,
			AlertInventoryInterval: cfg.TGAlertInventoryInterval,
		}
		info := &infoAdapter{mm: mm, ex: ex, cfg: cfg}
		tg, err := telegram.New(tgCfg, info)
		if err != nil {
			slog.Warn("telegram bot init failed, continuing without it", "err", err)
		} else {
			slog.Info("telegram bot started")
			go tg.Run(ctx)
		}
	}

	// ── Run strategy ─────────────────────────────────────────────────────────
	if err := mm.Run(ctx); err != nil {
		slog.Error("strategy exited with error", "err", err)
		os.Exit(1)
	}
}

// infoAdapter bridges the strategy and exchange layers to implement
// notify.InfoProvider for the notification layer.
type infoAdapter struct {
	mm  *strategy.MarketMaker
	ex  exchange.Exchange
	cfg *config.Config
}

var _ notify.InfoProvider = (*infoAdapter)(nil)

func (a *infoAdapter) GetSnapshot() botstate.Snapshot {
	return a.mm.State().Get()
}

func (a *infoAdapter) FlattenPosition(ctx context.Context) error {
	return a.mm.FlattenPosition(ctx)
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
