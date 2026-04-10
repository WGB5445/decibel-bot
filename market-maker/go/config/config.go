// Package config loads bot parameters from environment variables and CLI flags.
// CLI flags override environment variables when explicitly set.
package config

import (
	"encoding/hex"
	"flag"
	"fmt"
	"os"
	"strconv"
	"strings"
)

// ── Network profiles ──────────────────────────────────────────────────────────

// NetworkProfile holds the per-network URL and package address presets.
type NetworkProfile struct {
	RestAPIBase      string
	AptosFullnodeURL string
	PackageAddress   string
}

// networkProfiles maps network names to their URL / address presets.
var networkProfiles = map[string]NetworkProfile{
	"testnet": {
		RestAPIBase:      "https://api.testnet.aptoslabs.com/decibel/api/v1",
		AptosFullnodeURL: "https://api.testnet.aptoslabs.com/v1",
		PackageAddress:   "0xe7da2794b1d8af76532ed95f38bfdf1136abfd8ea3a240189971988a83101b7f",
	},
	"mainnet": {
		RestAPIBase:      "https://api.mainnet.aptoslabs.com/decibel/api/v1",
		AptosFullnodeURL: "https://api.mainnet.aptoslabs.com/v1",
		PackageAddress:   "0x50ead22afd6ffd9769e3b3d6e0e64a2a350d68e8b102c4e72e33d0b8cfdfdb06",
	},
}

// ── Config ────────────────────────────────────────────────────────────────────

// Config holds all runtime parameters for the market-maker bot.
type Config struct {
	// ── Network ──────────────────────────────────────────────────────────────
	Network string // "testnet" | "mainnet"

	// ── Trading parameters ────────────────────────────────────────────────────
	MarketName        string
	Spread            float64
	OrderSize         float64
	MaxInventory      float64
	SkewPerUnit       float64
	MaxMarginUsage    float64
	RefreshInterval   float64
	CooldownS         float64
	CancelResyncS     float64
	AutoFlatten       bool
	FlattenAggression float64
	DryRun            bool

	// ── Adaptive spread ───────────────────────────────────────────────────────
	AutoSpread         bool
	SpreadMin          float64
	SpreadMax          float64
	SpreadNoFillCycles int
	SpreadStep         float64

	// ── Credentials / addresses ───────────────────────────────────────────────
	BearerToken        string
	SubaccountAddress  string
	PrivateKey         string // hex, 0x-prefix optional
	PackageAddress     string
	NodeAPIKey         string // falls back to BearerToken
	AptosFullnodeURL   string
	MarketAddrOverride string // skips API discovery
	RestAPIBase        string

	// ── Telegram ─────────────────────────────────────────────────────────────
	TGBotToken               string // TG_BOT_TOKEN or -tg-token
	TGAdminID                int64  // TG_ADMIN_ID  or -tg-admin-id
	TGAlertInventory         bool   // TG_ALERT_INVENTORY or -tg-alert-inventory
	TGAlertInventoryInterval int    // TG_ALERT_INVENTORY_INTERVAL_MIN or -tg-alert-interval
}

// TelegramEnabled reports whether the Telegram bot should be started.
func (c *Config) TelegramEnabled() bool {
	return c.TGBotToken != "" && c.TGAdminID != 0
}

// Load reads env vars, applies network defaults, parses CLI flags (overrides), then validates.
//
// Effective priority (highest → lowest):
//
//	CLI flag (if set on command line)  >  explicit env var  >  network profile  >  built-in default
func Load() (*Config, error) {
	// Step 1: network from env only (baseline before flags).
	networkFromEnv := envStr("NETWORK", "testnet")
	profile, ok := networkProfiles[networkFromEnv]
	if !ok {
		return nil, fmt.Errorf("unknown network %q; valid values: testnet, mainnet", networkFromEnv)
	}

	cfg := &Config{
		Network: networkFromEnv,

		MarketName:        envStr("MARKET_NAME", "BTC/USD"),
		Spread:            envFloat("SPREAD", 0.001),
		OrderSize:         envFloat("ORDER_SIZE", 0.001),
		MaxInventory:      envFloat("MAX_INVENTORY", 0.005),
		SkewPerUnit:       envFloat("SKEW_PER_UNIT", 0.0001),
		MaxMarginUsage:    envFloat("MAX_MARGIN_USAGE", 0.5),
		RefreshInterval:   envFloat("REFRESH_INTERVAL", 20.0),
		CooldownS:         envFloat("COOLDOWN_S", 1.5),
		CancelResyncS:     envFloat("CANCEL_RESYNC_S", 8.0),
		AutoFlatten:       envBool("AUTO_FLATTEN", false),
		FlattenAggression: envFloat("FLATTEN_AGGRESSION", 0.001),
		DryRun:            envBool("DRY_RUN", false),

		AutoSpread:         envBool("AUTO_SPREAD", false),
		SpreadMin:          envFloat("SPREAD_MIN", 0.0004),
		SpreadMax:          envFloat("SPREAD_MAX", 0.02),
		SpreadNoFillCycles: int(envFloat("SPREAD_NO_FILL_CYCLES", 3)),
		SpreadStep:         envFloat("SPREAD_STEP", 0.0002),

		BearerToken:        os.Getenv("BEARER_TOKEN"),
		SubaccountAddress:  os.Getenv("SUBACCOUNT_ADDRESS"),
		PrivateKey:         os.Getenv("PRIVATE_KEY"),
		NodeAPIKey:         os.Getenv("NODE_API_KEY"),
		MarketAddrOverride: os.Getenv("MARKET_ADDR"),

		RestAPIBase:      envStr("REST_API_BASE", profile.RestAPIBase),
		AptosFullnodeURL: envStr("APTOS_FULLNODE_URL", profile.AptosFullnodeURL),
		PackageAddress:   envStr("PACKAGE_ADDRESS", profile.PackageAddress),

		TGBotToken:               os.Getenv("TG_BOT_TOKEN"),
		TGAdminID:                envInt64("TG_ADMIN_ID", 0),
		TGAlertInventory:         envBool("TG_ALERT_INVENTORY", false),
		TGAlertInventoryInterval: int(envFloat("TG_ALERT_INVENTORY_INTERVAL_MIN", 30)),
	}

	flag.StringVar(&cfg.Network, "network", cfg.Network,
		"Network preset: testnet | mainnet  (sets default URLs and package address)")
	flag.StringVar(&cfg.MarketName, "market-name", cfg.MarketName, "Market symbol (e.g. BTC/USD)")
	flag.Float64Var(&cfg.Spread, "spread", cfg.Spread, "Total spread fraction (0.001 = 0.1%)")
	flag.Float64Var(&cfg.OrderSize, "order-size", cfg.OrderSize, "Base units per side per quote")
	flag.Float64Var(&cfg.MaxInventory, "max-inventory", cfg.MaxInventory, "Stop quoting when abs(position) >= this")
	flag.Float64Var(&cfg.SkewPerUnit, "skew-per-unit", cfg.SkewPerUnit, "Extra half-spread per unit of inventory")
	flag.Float64Var(&cfg.MaxMarginUsage, "max-margin-usage", cfg.MaxMarginUsage, "Pause when cross_margin_ratio > this")
	flag.Float64Var(&cfg.RefreshInterval, "refresh-interval", cfg.RefreshInterval, "Seconds between cycles")
	flag.Float64Var(&cfg.CooldownS, "cooldown-s", cfg.CooldownS, "Seconds between placing bid and ask")
	flag.Float64Var(&cfg.CancelResyncS, "cancel-resync-s", cfg.CancelResyncS, "Seconds to wait before re-checking failed cancels")
	flag.BoolVar(&cfg.AutoFlatten, "auto-flatten", cfg.AutoFlatten, "Auto reduce-only order when inventory hits limit")
	flag.Float64Var(&cfg.FlattenAggression, "flatten-aggression", cfg.FlattenAggression, "Flatten order price offset from mid")
	flag.BoolVar(&cfg.DryRun, "dry-run", cfg.DryRun, "Log without sending transactions")
	flag.BoolVar(&cfg.AutoSpread, "auto-spread", cfg.AutoSpread, "Automatically narrow spread after spread-no-fill-cycles cycles with no fill")
	flag.Float64Var(&cfg.SpreadMin, "spread-min", cfg.SpreadMin, "Minimum spread the auto-adjuster will narrow to")
	flag.Float64Var(&cfg.SpreadMax, "spread-max", cfg.SpreadMax, "Maximum spread ceiling")
	flag.IntVar(&cfg.SpreadNoFillCycles, "spread-no-fill-cycles", cfg.SpreadNoFillCycles, "Cycles without fill before adjusting spread")
	flag.Float64Var(&cfg.SpreadStep, "spread-step", cfg.SpreadStep, "Amount to narrow spread per adjustment step")
	flag.StringVar(&cfg.PackageAddress, "package-address", cfg.PackageAddress, "Move package address (overrides network profile)")
	flag.StringVar(&cfg.AptosFullnodeURL, "fullnode-url", cfg.AptosFullnodeURL, "Aptos fullnode URL (overrides network profile)")
	flag.StringVar(&cfg.RestAPIBase, "api-base", cfg.RestAPIBase, "Decibel REST API base URL (overrides network profile)")

	flag.StringVar(&cfg.BearerToken, "bearer-token", cfg.BearerToken, "Decibel REST bearer token (overrides BEARER_TOKEN)")
	flag.StringVar(&cfg.SubaccountAddress, "subaccount", cfg.SubaccountAddress, "Subaccount object address (overrides SUBACCOUNT_ADDRESS)")
	flag.StringVar(&cfg.PrivateKey, "private-key", cfg.PrivateKey, "Ed25519 private key hex or AIP-80 (overrides PRIVATE_KEY); visible in process list")
	flag.StringVar(&cfg.NodeAPIKey, "node-api-key", cfg.NodeAPIKey, "Fullnode API key (overrides NODE_API_KEY; falls back to bearer token)")

	// Telegram
	flag.StringVar(&cfg.TGBotToken, "tg-token", cfg.TGBotToken,
		"Telegram bot token (overrides TG_BOT_TOKEN)")
	flag.Int64Var(&cfg.TGAdminID, "tg-admin-id", cfg.TGAdminID,
		"Telegram admin user ID (overrides TG_ADMIN_ID)")
	flag.BoolVar(&cfg.TGAlertInventory, "tg-alert-inventory", cfg.TGAlertInventory,
		"Enable Telegram alert when position exceeds max-inventory (overrides TG_ALERT_INVENTORY)")
	flag.IntVar(&cfg.TGAlertInventoryInterval, "tg-alert-interval", cfg.TGAlertInventoryInterval,
		"Minutes between repeated inventory-limit Telegram alerts (overrides TG_ALERT_INVENTORY_INTERVAL_MIN)")

	flag.Parse()

	flagsSet := make(map[string]struct{})
	flag.Visit(func(f *flag.Flag) {
		flagsSet[f.Name] = struct{}{}
	})

	// Step 3: if --network changed the preset, refresh URL/package from the new profile
	// unless the operator set them via CLI or env.
	if cfg.Network != networkFromEnv {
		p2, ok := networkProfiles[cfg.Network]
		if !ok {
			return nil, fmt.Errorf("unknown network %q; valid values: testnet, mainnet", cfg.Network)
		}
		if _, cli := flagsSet["api-base"]; !cli && !isExplicitEnv("REST_API_BASE") {
			cfg.RestAPIBase = p2.RestAPIBase
		}
		if _, cli := flagsSet["fullnode-url"]; !cli && !isExplicitEnv("APTOS_FULLNODE_URL") {
			cfg.AptosFullnodeURL = p2.AptosFullnodeURL
		}
		if _, cli := flagsSet["package-address"]; !cli && !isExplicitEnv("PACKAGE_ADDRESS") {
			cfg.PackageAddress = p2.PackageAddress
		}
	}

	return cfg, cfg.validate()
}

func (c *Config) validate() error {
	// Clamp TGAlertInventoryInterval to avoid time.NewTicker(0) panic.
	if c.TGAlertInventoryInterval <= 0 {
		c.TGAlertInventoryInterval = 30
	}

	var missing []string
	if c.BearerToken == "" {
		missing = append(missing, "BEARER_TOKEN (-bearer-token)")
	}
	if c.SubaccountAddress == "" {
		missing = append(missing, "SUBACCOUNT_ADDRESS (-subaccount)")
	}
	if c.PrivateKey == "" {
		missing = append(missing, "PRIVATE_KEY (-private-key)")
	}
	if c.PackageAddress == "" {
		missing = append(missing, "PACKAGE_ADDRESS (-package-address) or NETWORK preset")
	}
	if len(missing) > 0 {
		return fmt.Errorf("missing required configuration: %s", strings.Join(missing, ", "))
	}
	return nil
}

// ParsePrivateKey decodes the 32-byte Ed25519 seed from the configured private key string.
// Supports raw hex (with optional 0x), 64-byte seed||pubkey hex, and ed25519 AIP-80.
// secp256k1 keys are rejected.
func (c *Config) ParsePrivateKey() ([32]byte, error) {
	var zero [32]byte
	s := strings.TrimSpace(c.PrivateKey)
	if s == "" {
		return zero, fmt.Errorf("PRIVATE_KEY is empty")
	}
	if strings.Contains(s, "-priv-") {
		parts := strings.SplitN(s, "-priv-", 2)
		algo := strings.ToLower(parts[0])
		s = parts[1]
		if algo == "secp256k1" {
			return zero, fmt.Errorf("secp256k1 private keys are not supported")
		}
	}
	s = strings.TrimPrefix(strings.TrimSpace(s), "0x")
	b, err := hex.DecodeString(s)
	if err != nil {
		return zero, fmt.Errorf("PRIVATE_KEY is not valid hex: %w", err)
	}
	if len(b) < 32 {
		return zero, fmt.Errorf("PRIVATE_KEY must be at least 32 bytes, got %d", len(b))
	}
	copy(zero[:], b[:32])
	return zero, nil
}

// NodeKey returns the effective fullnode API key (NODE_API_KEY or BEARER_TOKEN fallback).
func (c *Config) NodeKey() string {
	if c.NodeAPIKey != "" {
		return c.NodeAPIKey
	}
	return c.BearerToken
}

// NormalizedMarketName returns the market name normalized for comparison.
func (c *Config) NormalizedMarketName() string {
	return NormalizeMarket(c.MarketName)
}

// NormalizeMarket upper-cases and replaces "/" with "-".
func NormalizeMarket(name string) string {
	return strings.ToUpper(strings.ReplaceAll(name, "/", "-"))
}

// ── Env helpers ──────────────────────────────────────────────────────────────

func isExplicitEnv(key string) bool {
	return os.Getenv(key) != ""
}

func envStr(key, def string) string {
	if v := os.Getenv(key); v != "" {
		return v
	}
	return def
}

func envFloat(key string, def float64) float64 {
	if v := os.Getenv(key); v != "" {
		if f, err := strconv.ParseFloat(v, 64); err == nil {
			return f
		}
	}
	return def
}

func envBool(key string, def bool) bool {
	if v := os.Getenv(key); v != "" {
		if b, err := strconv.ParseBool(v); err == nil {
			return b
		}
	}
	return def
}

func envInt64(key string, def int64) int64 {
	if v := os.Getenv(key); v != "" {
		if i, err := strconv.ParseInt(v, 10, 64); err == nil {
			return i
		}
	}
	return def
}
