package config

import (
	"os"
	"path/filepath"
	"testing"
	"time"

	"github.com/shopspring/decimal"
)

func TestLoadDefaults(t *testing.T) {
	// Create a minimal temp config file
	tmpDir := t.TempDir()
	cfgPath := filepath.Join(tmpDir, "config.yaml")
	content := `
decibel:
  bearer_token: "test-token"
  api_wallet_private_key: "0x1234"
  api_wallet_address: "0xabcd"
  subaccount_address: "0xsub"
  market_name: "BTC-USD"
`
	if err := os.WriteFile(cfgPath, []byte(content), 0644); err != nil {
		t.Fatal(err)
	}

	cfg, err := Load(cfgPath)
	if err != nil {
		t.Fatalf("load config failed: %v", err)
	}

	if cfg.Env != "testnet" {
		t.Fatalf("expected env testnet, got %s", cfg.Env)
	}
	if cfg.Decibel.MarketName != "BTC-USD" {
		t.Fatalf("expected market BTC-USD, got %s", cfg.Decibel.MarketName)
	}
	if !cfg.Strategy.BidSpread.Equal(decimal.NewFromFloat(0.01)) {
		t.Fatalf("expected bid_spread 0.01, got %s", cfg.Strategy.BidSpread.String())
	}
	if cfg.Strategy.OrderRefreshTime != 30*time.Second {
		t.Fatalf("expected order_refresh_time 30s, got %v", cfg.Strategy.OrderRefreshTime)
	}
}

func TestBaseURLs(t *testing.T) {
	cfg := &Config{Env: "testnet"}
	rest, ws, fullnode := cfg.BaseURLs()
	if rest != "https://api.testnet.aptoslabs.com/decibel" {
		t.Fatalf("unexpected rest url: %s", rest)
	}
	if ws != "wss://api.testnet.aptoslabs.com/decibel/ws" {
		t.Fatalf("unexpected ws url: %s", ws)
	}
	if fullnode != "https://fullnode.testnet.aptoslabs.com/v1" {
		t.Fatalf("unexpected fullnode url: %s", fullnode)
	}
}
