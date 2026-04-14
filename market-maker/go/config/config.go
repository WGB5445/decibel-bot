// Package config loads bot parameters from environment variables, optional
// JSON/YAML/TOML config files, and CLI flags. See Load / LoadWith for precedence.
package config

import (
	"encoding/hex"
	"fmt"
	"os"
	"strconv"
	"strings"
	"time"
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
	MarketName      string
	Spread          float64
	OrderSize       float64
	MaxInventory    float64
	SkewPerUnit     float64
	MaxMarginUsage  float64
	RefreshInterval float64
	// RefreshIntervalJitterS is half-width in seconds for uniform jitter around RefreshInterval
	// (sleep Uniform[interval−jitter, interval+jitter]; 0 disables). See README.
	RefreshIntervalJitterS float64
	// ShutdownCancelTimeoutS is the time budget (seconds) for graceful shutdown bulk cancel
	// (CancelBulkOrders only). Clamped in validate to at least 5s; 0 or negative becomes default 60s.
	ShutdownCancelTimeoutS float64
	AutoFlatten            bool
	FlattenAggression      float64
	// FlattenMaxDeviation bounds passive POST_ONLY flatten prices vs mid (e.g. 0.05 = 5%).
	// Long (sell): price capped at mid×(1+dev). Short (buy): price floored at mid×(1−dev).
	// 0 disables the bound.
	FlattenMaxDeviation float64
	DryRun              bool

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

	// Locale is UI language for bot-facing copy: "zh" (default) or "en". Set via LOCALE / BOT_LOCALE, config file, or -locale.
	Locale string

	// LogLevel is slog level: debug | info | warn | error (default info). LOG_LEVEL / -log-level.
	LogLevel string
	// LogFormat is text (default, ANSI when TTY) or json (one JSON object per line). LOG_FORMAT / -log-format.
	LogFormat string
	// LogCycleJSON emits a single structured JSON line after each successful bulk quote cycle. LOG_CYCLE_JSON or LOG_TRACE / -log-cycle-json.
	LogCycleJSON bool
	// LogVerbose enables extra REST DEBUG lines when LogLevel is debug. LOG_VERBOSE / -log-verbose.
	LogVerbose bool

	// LogTeeFile mirrors logs to a file: empty = off; "auto" = LOG_TEE_FILE_DIR + subaccount_market.log; else path. LOG_TEE_FILE / -log-tee-file.
	LogTeeFile string
	// LogTeeFileDir is the directory used when LogTeeFile is "auto" (default "."). LOG_TEE_FILE_DIR / -log-tee-file-dir.
	LogTeeFileDir string

	// ── Telegram ─────────────────────────────────────────────────────────────
	TGBotToken               string // TG_BOT_TOKEN or -tg-token
	TGAdminID                int64  // TG_ADMIN_ID or -tg-admin-id
	TGAlertInventory         bool   // TG_ALERT_INVENTORY or -tg-alert-inventory
	TGAlertInventoryInterval int    // TG_ALERT_INVENTORY_INTERVAL_MIN or -tg-alert-interval
	TGStrictStart            bool   // TG_STRICT_START or -tg-strict-start
}

// TelegramEnabled reports whether the Telegram bot should be started.
func (c *Config) TelegramEnabled() bool {
	return c.TGBotToken != "" && c.TGAdminID != 0
}

func (c *Config) validate() error {
	c.Locale = normalizeBotLocale(c.Locale)

	c.LogLevel = strings.ToLower(strings.TrimSpace(c.LogLevel))
	if c.LogLevel == "" {
		c.LogLevel = "info"
	}
	switch c.LogLevel {
	case "debug", "info", "warn", "warning", "error":
	default:
		c.LogLevel = "info"
	}
	c.LogFormat = strings.ToLower(strings.TrimSpace(c.LogFormat))
	if c.LogFormat == "" {
		c.LogFormat = "text"
	}
	if c.LogFormat != "text" && c.LogFormat != "json" {
		c.LogFormat = "text"
	}

	if strings.TrimSpace(c.LogTeeFileDir) == "" {
		c.LogTeeFileDir = "."
	}

	// Clamp TGAlertInventoryInterval to avoid time.NewTicker(0) panic.
	if c.TGAlertInventoryInterval <= 0 {
		c.TGAlertInventoryInterval = 30
	}

	if c.RefreshIntervalJitterS < 0 {
		return fmt.Errorf("REFRESH_INTERVAL_JITTER_S (-refresh-interval-jitter) must be >= 0, got %v", c.RefreshIntervalJitterS)
	}

	if c.ShutdownCancelTimeoutS <= 0 {
		c.ShutdownCancelTimeoutS = 60
	}
	if c.ShutdownCancelTimeoutS < 5 {
		c.ShutdownCancelTimeoutS = 5
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

// ShutdownCancelTimeout returns the time budget for graceful shutdown bulk cancel (CancelBulkOrders).
// Values are normalized like validate: default 60s, minimum 5s.
func (c *Config) ShutdownCancelTimeout() time.Duration {
	s := c.ShutdownCancelTimeoutS
	if s <= 0 {
		s = 60
	}
	if s < 5 {
		s = 5
	}
	return time.Duration(s * float64(time.Second))
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

// boolCLIFlagNames are flags registered with flag.BoolVar in Load. Only these
// get "-name value" merged into "-name=value" before Parse (see normalizeBoolCLIArgs).
// When adding a new BoolVar in Load, add the name here and document it in README.md (Boolean flags).
var boolCLIFlagNames = map[string]struct{}{
	"auto-flatten":       {},
	"dry-run":            {},
	"auto-spread":        {},
	"log-cycle-json":     {},
	"log-verbose":        {},
	"tg-alert-inventory": {},
	"tg-strict-start":    {},
}

// normalizeBoolCLIArgs returns a copy of args with "-boolflag literal" rewritten
// to "-boolflag=literal" when literal is a boolean token and boolflag is known.
// After "--", args are left unchanged.
func normalizeBoolCLIArgs(args []string) []string {
	if len(args) <= 1 {
		return args
	}
	out := make([]string, 0, len(args))
	out = append(out, args[0])
	pastTerminator := false
	for i := 1; i < len(args); i++ {
		a := args[i]
		if pastTerminator {
			out = append(out, a)
			continue
		}
		if a == "--" {
			pastTerminator = true
			out = append(out, a)
			continue
		}
		name, ok := leadingFlagName(a)
		if !ok || strings.Contains(a, "=") {
			out = append(out, a)
			continue
		}
		if _, isBool := boolCLIFlagNames[name]; !isBool {
			out = append(out, a)
			continue
		}
		if i+1 < len(args) {
			next := args[i+1]
			if isBoolToken(next) && !strings.HasPrefix(next, "-") {
				out = append(out, a+"="+next)
				i++
				continue
			}
		}
		out = append(out, a)
	}
	return out
}

// leadingFlagName returns the flag name for "-name" or "--name" without "=value".
func leadingFlagName(arg string) (name string, ok bool) {
	if arg == "" || arg == "-" {
		return "", false
	}
	if arg[0] != '-' {
		return "", false
	}
	s := strings.TrimLeft(arg, "-")
	if s == "" {
		return "", false
	}
	if i := strings.IndexByte(s, '='); i >= 0 {
		s = s[:i]
	}
	if s == "" {
		return "", false
	}
	return s, true
}

func isBoolToken(s string) bool {
	switch strings.ToLower(strings.TrimSpace(s)) {
	case "true", "false", "t", "f", "0", "1", "yes", "no":
		return true
	default:
		return false
	}
}

// normalizeBotLocale returns a stable UI language tag: "zh" or "en".
func normalizeBotLocale(s string) string {
	switch strings.ToLower(strings.TrimSpace(s)) {
	case "en", "english":
		return "en"
	default:
		return "zh"
	}
}

// ── Env helpers ──────────────────────────────────────────────────────────────

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
