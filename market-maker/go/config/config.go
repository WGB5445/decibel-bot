// Package config loads bot parameters from environment variables and CLI flags.
// Environment variables set the defaults; flags override them.
package config

import (
	"encoding/hex"
	"flag"
	"fmt"
	"os"
	"strconv"
	"strings"

	"decibel-mm-bot/aptos"
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
		PackageAddress:   "0x2a4e9bee4b09f5b8e9c996a489c6993abe1e9e45e61e81bb493e38e53a3e7e3d",
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
	BearerToken             string
	SubaccountAddress       string
	PrivateKey              string // hex, 0x-prefix optional
	PerpEngineGlobalAddress string
	PackageAddress          string
	NodeAPIKey              string // falls back to BearerToken
	AptosFullnodeURL        string
	MarketAddrOverride      string // skips API discovery
	RestAPIBase             string
}

// Load reads env vars (defaults) then parses CLI flags (overrides), then validates.
//
// Priority for network-dependent URLs (highest → lowest):
//
//	CLI flag  >  explicit env var  >  network profile  >  (testnet default)
func Load() (*Config, error) {
	// ── Step 1: resolve network and apply profile as URL defaults ─────────────
	network := envStr("NETWORK", "testnet")
	profile, ok := networkProfiles[network]
	if !ok {
		return nil, fmt.Errorf("unknown network %q; valid values: testnet, mainnet", network)
	}

	// Explicit env vars beat the network profile; flags (parsed below) beat both.
	cfg := &Config{
		Network: network,

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

		BearerToken:             os.Getenv("BEARER_TOKEN"),
		SubaccountAddress:       os.Getenv("SUBACCOUNT_ADDRESS"),
		PrivateKey:              os.Getenv("PRIVATE_KEY"),
		PerpEngineGlobalAddress: os.Getenv("PERP_ENGINE_GLOBAL_ADDRESS"),
		NodeAPIKey:              os.Getenv("NODE_API_KEY"),
		MarketAddrOverride:      os.Getenv("MARKET_ADDR"),

		// URL fields: explicit env var wins over profile, flag wins over both.
		RestAPIBase:      envStr("REST_API_BASE", profile.RestAPIBase),
		AptosFullnodeURL: envStr("APTOS_FULLNODE_URL", profile.AptosFullnodeURL),
		PackageAddress:   envStr("PACKAGE_ADDRESS", profile.PackageAddress),
	}

	// ── Step 2: register CLI flags (override everything set above) ────────────
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
	flag.Parse()

	// DRY_RUN is intentionally controlled explicitly by the operator via the
	// `DRY_RUN` environment variable or the `-dry-run` CLI flag. No automatic
	// testnet/mainnet defaults are applied here.

	// ── Step 3: re-apply profile if --network flag changed the network ─────────
	// Only overwrite URL fields that weren't set via an explicit env var, so the
	// priority chain (flag > env var > profile) is preserved.
	if cfg.Network != network {
		p2, ok := networkProfiles[cfg.Network]
		if !ok {
			return nil, fmt.Errorf("unknown network %q; valid values: testnet, mainnet", cfg.Network)
		}
		if !isExplicitEnv("REST_API_BASE") {
			cfg.RestAPIBase = p2.RestAPIBase
		}
		if !isExplicitEnv("APTOS_FULLNODE_URL") {
			cfg.AptosFullnodeURL = p2.AptosFullnodeURL
		}
		if !isExplicitEnv("PACKAGE_ADDRESS") {
			cfg.PackageAddress = p2.PackageAddress
		}
	}

	// No automatic dry-run defaults; operator must set `DRY_RUN` explicitly.

	return cfg, cfg.validate()
}

func (c *Config) validate() error {
	var missing []string
	if c.BearerToken == "" {
		missing = append(missing, "BEARER_TOKEN")
	}
	if c.SubaccountAddress == "" {
		missing = append(missing, "SUBACCOUNT_ADDRESS")
	}
	if c.PrivateKey == "" {
		missing = append(missing, "PRIVATE_KEY")
	}
	if c.PerpEngineGlobalAddress == "" {
		if c.PackageAddress != "" {
			// Try to derive the global perp engine address automatically from
			// the package address. This avoids requiring users to paste the
			// value in environments where PACKAGE_ADDRESS is known.
			addr, err := aptos.CreatePerpEngineGlobalAddress(c.PackageAddress)
			if err != nil {
				return fmt.Errorf("derive PERP_ENGINE_GLOBAL_ADDRESS: %w", err)
			}
			c.PerpEngineGlobalAddress = addr
		} else {
			missing = append(missing, "PERP_ENGINE_GLOBAL_ADDRESS")
		}
	}
	if len(missing) > 0 {
		return fmt.Errorf("missing required environment variables: %s", strings.Join(missing, ", "))
	}

	// No automatic mainnet guard; operator must control `DRY_RUN` explicitly.

	return nil
}

// ParsePrivateKey decodes and validates the 32-byte Ed25519 seed.
// Supported inputs:
//   - raw hex (with or without 0x prefix)
//   - AIP-80 formatted keys: ed25519-priv-<hex> or secp256k1-priv-<hex>
//
// For ed25519 (or unspecified) the first 32 bytes are returned as the seed.
// secp256k1 keys are recognised but not usable for signing in this client.
func (c *Config) ParsePrivateKey() ([32]byte, error) {
	var seed [32]byte
	s := strings.TrimSpace(c.PrivateKey)
	if s == "" {
		return seed, fmt.Errorf("PRIVATE_KEY is empty")
	}

	algo := ""
	// Accept AIP-80 style prefix like "ed25519-priv-" or "secp256k1-priv-".
	if strings.Contains(s, "-priv-") {
		parts := strings.SplitN(s, "-priv-", 2)
		algo = strings.ToLower(parts[0])
		s = parts[1]
	}

	// Strip optional 0x prefix.
	s = strings.TrimPrefix(s, "0x")
	s = strings.TrimSpace(s)

	b, err := hex.DecodeString(s)
	if err != nil {
		return seed, fmt.Errorf("PRIVATE_KEY is not valid hex: %w", err)
	}
	if len(b) < 32 {
		return seed, fmt.Errorf("PRIVATE_KEY must be at least 32 bytes, got %d", len(b))
	}

	switch algo {
	case "", "ed25519":
		copy(seed[:], b[:32])
		return seed, nil
	case "secp256k1":
		return seed, fmt.Errorf("secp256k1 private keys are not supported by this client; please provide an ed25519 seed (ed25519-priv-... or raw hex)")
	default:
		// Unknown algorithm prefix: fall back to interpreting as raw hex.
		copy(seed[:], b[:32])
		return seed, nil
	}
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

// isExplicitEnv returns true when the environment variable was explicitly set
// (non-empty), meaning it should take priority over any network profile default.
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
