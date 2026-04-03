package main

import (
	"context"
	"log/slog"
	"os"
	"os/signal"
	"syscall"

	"github.com/joho/godotenv"

	"decibel-mm-bot/bot"
	"decibel-mm-bot/config"
)

func main() {
	// Load .env before config so all env vars are visible to flag defaults.
	// Silently ignored when the file does not exist.
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

	if err := bot.Run(ctx, cfg); err != nil {
		slog.Error("bot exited with error", "err", err)
		os.Exit(1)
	}
}
