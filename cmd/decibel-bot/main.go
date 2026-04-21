package main

import (
	"context"
	"fmt"
	"net/http"
	"os"
	"os/signal"
	"syscall"
	"time"

	"github.com/bujih/decibel-mm-go/internal/config"
	"github.com/bujih/decibel-mm-go/internal/decibel"
	"github.com/bujih/decibel-mm-go/internal/engine"
	"github.com/bujih/decibel-mm-go/internal/models"
	"github.com/bujih/decibel-mm-go/internal/strategy"
	"github.com/prometheus/client_golang/prometheus/promhttp"
	"github.com/shopspring/decimal"
	"github.com/urfave/cli/v2"
	"go.uber.org/zap"
)

func main() {
	cli.AppHelpTemplate = `NAME:
   {{.Name}} - {{.Usage}}

USAGE:
   {{.HelpName}} {{if .VisibleFlags}}[全局选项]{{end}} {{if .Commands}}命令 [命令选项]{{end}}
{{if len .Authors}}
AUTHOR:
   {{range .Authors}}{{ . }}{{end}}
   {{end}}{{if .Commands}}
COMMANDS:
{{range .Commands}}{{if not .HideHelp}}   {{join .Names ", "}}{{ "\t" }}{{.Usage}}{{ "\n" }}{{end}}{{end}}{{end}}{{if .VisibleFlags}}
全局选项:
   {{range .VisibleFlags}}{{.}}
   {{end}}{{end}}
`

	cli.CommandHelpTemplate = `NAME:
   {{.HelpName}} - {{.Usage}}

USAGE:
   {{if .UsageText}}{{.UsageText}}{{else}}{{.HelpName}}{{if .VisibleFlags}} [命令选项]{{end}} {{if .ArgsUsage}}{{.ArgsUsage}}{{else}}[参数...]{{end}}{{end}}{{if .Category}}

CATEGORY:
   {{.Category}}{{end}}{{if .Description}}

DESCRIPTION:
   {{.Description}}{{end}}{{if .VisibleFlags}}

OPTIONS:
   {{range .VisibleFlags}}{{.}}
   {{end}}{{end}}
`

	app := &cli.App{
		Name:    "decibel-bot",
		Usage:   "Decibel 永续 DEX 做市机器人（基于 Aptos 链）",
		Version: "1.0.0",
		Flags: []cli.Flag{
			&cli.StringFlag{
				Name:    "config",
				Aliases: []string{"c"},
				EnvVars: []string{"DECIBEL_CONFIG"},
				Value:   "configs/config.yaml",
				Usage:   "配置文件路径（默认: configs/config.yaml）",
			},
		},
		Action: runBot,
	}

	// 覆盖内置的 help 和 version 描述为中文
	cli.HelpFlag = &cli.BoolFlag{
		Name:    "help",
		Aliases: []string{"h"},
		Usage:   "显示帮助信息",
	}
	cli.VersionFlag = &cli.BoolFlag{
		Name:    "version",
		Aliases: []string{"v"},
		Usage:   "显示版本号",
	}

	if err := app.Run(os.Args); err != nil {
		fmt.Fprintf(os.Stderr, "error: %v\n", err)
		os.Exit(1)
	}
}

func runBot(c *cli.Context) error {
	logger, _ := zap.NewProduction()
	defer logger.Sync()

	cfgPath := c.String("config")
	cfg, err := config.Load(cfgPath)
	if err != nil {
		logger.Fatal("failed to load config", zap.Error(err))
	}

	restBase, wsBase, fullnode := cfg.BaseURLs()
	if cfg.Aptos.FullnodeURL != "" {
		fullnode = cfg.Aptos.FullnodeURL
	}

	logger.Info("starting decibel market maker bot",
		zap.String("env", cfg.Env),
		zap.String("market", cfg.Decibel.MarketName),
		zap.String("subaccount", cfg.Decibel.SubaccountAddr),
	)

	// Clients
	readClient := decibel.NewReadClient(restBase, cfg.Decibel.BearerToken)
	signer, err := decibel.ParseAccount(cfg.Decibel.ApiWalletPrivKey)
	if err != nil {
		logger.Fatal("failed to parse aptos signing account", zap.Error(err))
	}

	// 从私钥自动推导钱包地址，无需用户手动填写
	acctAddr := signer.AccountAddress()
	derivedAddr := acctAddr.String()
	if cfg.Decibel.ApiWalletAddress != "" {
		if !decibel.AddrEqual(cfg.Decibel.ApiWalletAddress, derivedAddr) {
			logger.Warn("config api_wallet_address does not match derived address from private key",
				zap.String("config_address", cfg.Decibel.ApiWalletAddress),
				zap.String("derived_address", derivedAddr),
			)
		}
	} else {
		cfg.Decibel.ApiWalletAddress = derivedAddr
		logger.Info("api_wallet_address auto-derived from private key",
			zap.String("address", derivedAddr),
		)
	}

	nodeClient, err := decibel.NewNodeClient(fullnode, cfg.Decibel.BearerToken, decibel.ChainIDForNetwork(cfg.Env))
	if err != nil {
		logger.Fatal("failed to create aptos node client", zap.Error(err))
	}

	packageAddr := decibel.PackageAddrForNetwork(cfg.Env)
	writeClient, err := decibel.NewWriteClient(nodeClient, signer, packageAddr)
	if err != nil {
		logger.Fatal("failed to create write client", zap.Error(err))
	}

	// Health check: fetch markets and resolve target market
	ctx := context.Background()
	markets, err := readClient.GetMarkets(ctx)
	if err != nil {
		logger.Fatal("failed to fetch markets", zap.Error(err))
	}

	var targetMarket *decibel.Market
	for i := range markets {
		if markets[i].Name == cfg.Decibel.MarketName {
			targetMarket = &markets[i]
			break
		}
	}
	if targetMarket == nil {
		logger.Fatal("target market not found", zap.String("market", cfg.Decibel.MarketName))
	}
	logger.Info("target market resolved",
		zap.String("market_addr", targetMarket.MarketAddr),
		zap.Int("price_decimals", targetMarket.PriceDecimals),
		zap.Int("size_decimals", targetMarket.SizeDecimals),
	)

	// Fetch initial account state
	overview, _ := readClient.GetAccountOverview(ctx, cfg.Decibel.SubaccountAddr)
	if overview != nil {
		logger.Info("account overview",
			zap.Float64("equity", overview.Equity),
			zap.Float64("available_margin", overview.AvailableMargin),
		)
	}
	positions, _ := readClient.GetPositions(ctx, cfg.Decibel.SubaccountAddr)
	logger.Info("initial positions", zap.Int("count", len(positions)))
	openOrders, _ := readClient.GetOpenOrders(ctx, cfg.Decibel.SubaccountAddr)
	logger.Info("initial open orders", zap.Int("count", len(openOrders)))

	// Engine components
	bus := engine.NewEventBus()
	orderMgr := engine.NewOrderManager(logger)
	positionMgr := engine.NewPositionManager(logger)

	// Seed initial state from REST
	for _, o := range openOrders {
		price, _ := decimal.NewFromString(o.Price)
		size, _ := decimal.NewFromString(o.Size)
		side := models.SideBuy
		if o.Side == "sell" || o.Side == "SELL" {
			side = models.SideSell
		}
		orderMgr.ConfirmOrder(o.OrderID, o.OrderID)
		lo := orderMgr.GetOrder(o.OrderID)
		if lo != nil {
			lo.MarketAddr = o.MarketAddr
			lo.Price = price
			lo.Size = size
			lo.Side = side
			lo.IsReduceOnly = o.IsReduceOnly
		}
	}

	// Transaction tracker
	txTracker := engine.NewTxTracker(nodeClient, bus, cfg.Strategy.TxPollInterval, cfg.Strategy.MaxPendingTx, logger)

	// Strategy
	strat := strategy.NewDecibelMM(cfg, targetMarket.MarketAddr, readClient, writeClient, orderMgr, positionMgr, bus, logger)

	// WebSocket client
	wsClient := decibel.NewWSClient(wsBase, cfg.Decibel.BearerToken, logger)
	wsClient.SubscribeChannel(fmt.Sprintf("depth:%s", targetMarket.MarketAddr))
	wsClient.SubscribeChannel(fmt.Sprintf("market_price:%s", targetMarket.MarketAddr))
	wsClient.SubscribeChannel(fmt.Sprintf("order_updates:%s", cfg.Decibel.SubaccountAddr))
	wsClient.SubscribeChannel(fmt.Sprintf("account_positions:%s", cfg.Decibel.SubaccountAddr))
	wsClient.SubscribeChannel(fmt.Sprintf("user_trades:%s", cfg.Decibel.SubaccountAddr))

	// Scheduler
	tickInterval := 5 * time.Second
	if cfg.Strategy.OrderRefreshTime > 0 && cfg.Strategy.OrderRefreshTime < 60*time.Second {
		tickInterval = cfg.Strategy.OrderRefreshTime
	}
	scheduler := engine.NewScheduler(tickInterval, bus)

	// Event dispatcher: fan-out from bus to all consumers
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	go txTracker.Start(ctx)
	go scheduler.Start(ctx)
	go wsClient.Start(ctx)

	// Event consumer loop
	go func() {
		evCh := bus.Subscribe()
		for ev := range evCh {
			// Update managers first, then strategy
			switch ev.Type {
			case models.EventDepthUpdate:
				if d, ok := ev.Data.(models.DepthUpdate); ok {
					// handle depth
					_ = d
				}
			case models.EventOrderUpdate:
				if o, ok := ev.Data.(models.OrderUpdate); ok {
					orderMgr.UpdateFromWS(o)
				}
			case models.EventPositionUpdate:
				if p, ok := ev.Data.(models.PositionUpdate); ok {
					positionMgr.UpdateFromWS(p)
				}
			case models.EventTradeFill:
				if t, ok := ev.Data.(models.TradeFill); ok {
					positionMgr.UpdateFromTradeFill(t)
				}
			}
			strat.HandleEvent(ev)
		}
	}()

	// WS event bridge
	go func() {
		for ev := range wsClient.Events() {
			bus.Publish(ev)
		}
	}()

	// Start metrics server
	go func() {
		http.Handle("/metrics", promhttp.Handler())
		if err := http.ListenAndServe(":2112", nil); err != nil {
			logger.Error("metrics server error", zap.Error(err))
		}
	}()
	logger.Info("metrics server started on :2112")

	logger.Info("bot running", zap.String("market", cfg.Decibel.MarketName))

	// Graceful shutdown
	sigCh := make(chan os.Signal, 1)
	signal.Notify(sigCh, syscall.SIGINT, syscall.SIGTERM)
	<-sigCh

	logger.Info("shutting down gracefully")
	cancel()
	wsClient.Stop()
	txTracker.Stop()
	bus.Close()

	// Wait a bit for goroutines to exit
	time.Sleep(500 * time.Millisecond)
	logger.Info("goodbye")
	return nil
}
