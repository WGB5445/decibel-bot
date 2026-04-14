package config

import (
	"flag"
	"fmt"
	"os"
	"path/filepath"
	"strconv"
	"strings"
)

// NormalizeBoolCLIArgsForInit applies the same boolean CLI normalization as LoadWith
// (for early flags such as -init-config that are parsed before Load).
func NormalizeBoolCLIArgsForInit(args []string) []string {
	return normalizeBoolCLIArgs(args)
}

// LoadOptions controls config loading (tests may set Args / ConfigData).
type LoadOptions struct {
	// Args is the full argv slice including program name at index 0.
	// If nil, os.Args is used.
	Args []string

	// ConfigData, if non-empty, is decoded as ConfigExt instead of reading a file from disk.
	ConfigData []byte
	ConfigExt  string
}

// Load reads configuration with priority (highest → lowest):
//
//	CLI flags  >  config file (CONFIG_FILE / -config)  >  environment variables  >  network preset / built-in defaults
//
// A .env file is not a separate layer: godotenv (in main) injects into the environment first.
func Load() (*Config, error) {
	return LoadWith(LoadOptions{})
}

// LoadWith loads configuration like Load but accepts options for tests.
func LoadWith(opts LoadOptions) (*Config, error) {
	args := opts.Args
	if len(args) == 0 {
		args = os.Args
	}
	if len(args) == 0 {
		return nil, fmt.Errorf("missing argv (program name)")
	}

	args = normalizeBoolCLIArgs(args)

	networkEnv := envStr("NETWORK", "testnet")
	profile, ok := networkProfiles[networkEnv]
	if !ok {
		return nil, fmt.Errorf("unknown network %q; valid values: testnet, mainnet", networkEnv)
	}

	cfg := newConfigFromEnvProfile(profile, networkEnv)
	networkAfterEnv := cfg.Network
	explicitEnv := explicitEnvKeys()

	var explicitFile map[string]bool
	if len(opts.ConfigData) > 0 {
		ext := strings.ToLower(strings.TrimSpace(opts.ConfigExt))
		if ext == "" {
			return nil, fmt.Errorf("LoadOptions.ConfigExt is required when ConfigData is set")
		}
		fc, err := decodeFileConfig(opts.ConfigData, ext)
		if err != nil {
			return nil, fmt.Errorf("config data (%s): %w", ext, err)
		}
		explicitFile = make(map[string]bool)
		applyFileConfig(cfg, fc, explicitFile)
	} else {
		path := strings.TrimSpace(os.Getenv("CONFIG_FILE"))
		path = configPathFromCLI(path, args)
		if path != "" {
			abs, err := filepath.Abs(path)
			if err != nil {
				abs = path
			}
			ext, err := configExtFromPath(path)
			if err != nil {
				return nil, err
			}
			data, err := os.ReadFile(path)
			if err != nil {
				return nil, fmt.Errorf("read config file %s: %w", abs, err)
			}
			fc, err := decodeFileConfig(data, ext)
			if err != nil {
				return nil, fmt.Errorf("config file %s: %w", abs, err)
			}
			explicitFile = make(map[string]bool)
			applyFileConfig(cfg, fc, explicitFile)
		}
	}

	if explicitFile == nil {
		explicitFile = map[string]bool{}
	}

	prog := filepath.Base(args[0])
	fs := flag.NewFlagSet(prog, flag.ContinueOnError)
	fs.SetOutput(os.Stderr)
	registerAllFlags(fs, cfg)

	if err := fs.Parse(args[1:]); err != nil {
		return nil, err
	}

	flagsSet := make(map[string]struct{})
	fs.Visit(func(f *flag.Flag) {
		flagsSet[f.Name] = struct{}{}
	})

	if cfg.Network != networkAfterEnv {
		p2, ok := networkProfiles[cfg.Network]
		if !ok {
			return nil, fmt.Errorf("unknown network %q; valid values: testnet, mainnet", cfg.Network)
		}
		if !urlExplicit(flagsSet, explicitFile, explicitEnv, "api-base", "REST_API_BASE") {
			cfg.RestAPIBase = p2.RestAPIBase
		}
		if !urlExplicit(flagsSet, explicitFile, explicitEnv, "fullnode-url", "APTOS_FULLNODE_URL") {
			cfg.AptosFullnodeURL = p2.AptosFullnodeURL
		}
		if !urlExplicit(flagsSet, explicitFile, explicitEnv, "package-address", "PACKAGE_ADDRESS") {
			cfg.PackageAddress = p2.PackageAddress
		}
	}

	return cfg, cfg.validate()
}

// envLocale reads BOT_LOCALE first, then LOCALE, default "zh".
func envLocale() string {
	if v := strings.TrimSpace(os.Getenv("BOT_LOCALE")); v != "" {
		return v
	}
	return envStr("LOCALE", "zh")
}

func newConfigFromEnvProfile(profile NetworkProfile, networkEnv string) *Config {
	return &Config{
		Network: networkEnv,

		Locale: envLocale(),

		MarketName:             envStr("MARKET_NAME", "BTC/USD"),
		Spread:                 envFloat("SPREAD", 0.001),
		OrderSize:              envFloat("ORDER_SIZE", 0.001),
		MaxInventory:           envFloat("MAX_INVENTORY", 0.005),
		SkewPerUnit:            envFloat("SKEW_PER_UNIT", 0.0001),
		MaxMarginUsage:         envFloat("MAX_MARGIN_USAGE", 0.5),
		RefreshInterval:        envFloat("REFRESH_INTERVAL", 20.0),
		RefreshIntervalJitterS: envFloat("REFRESH_INTERVAL_JITTER_S", 0),
		ShutdownCancelTimeoutS: envFloat("SHUTDOWN_CANCEL_TIMEOUT", 60.0),
		AutoFlatten:            envBool("AUTO_FLATTEN", false),
		FlattenAggression:      envFloat("FLATTEN_AGGRESSION", 0.001),
		FlattenMaxDeviation:    envFloat("FLATTEN_MAX_DEVIATION", 0.05),
		DryRun:                 envBool("DRY_RUN", false),

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
		TGStrictStart:            envBool("TG_STRICT_START", false),
	}
}

// explicitEnvKeys returns env vars considered explicitly set for URL / merge tracing.
// String keys use the same rule as the historical isExplicitEnv: non-empty getenv.
// Numeric / bool keys: non-empty getenv and successful parse (matches envFloat/envBool).
func explicitEnvKeys() map[string]bool {
	m := make(map[string]bool)
	if os.Getenv("LOCALE") != "" {
		m["LOCALE"] = true
	}
	if os.Getenv("BOT_LOCALE") != "" {
		m["BOT_LOCALE"] = true
	}
	if os.Getenv("NETWORK") != "" {
		m["NETWORK"] = true
	}
	if os.Getenv("MARKET_NAME") != "" {
		m["MARKET_NAME"] = true
	}
	if v := os.Getenv("SPREAD"); v != "" {
		if _, err := strconv.ParseFloat(v, 64); err == nil {
			m["SPREAD"] = true
		}
	}
	if v := os.Getenv("ORDER_SIZE"); v != "" {
		if _, err := strconv.ParseFloat(v, 64); err == nil {
			m["ORDER_SIZE"] = true
		}
	}
	if v := os.Getenv("MAX_INVENTORY"); v != "" {
		if _, err := strconv.ParseFloat(v, 64); err == nil {
			m["MAX_INVENTORY"] = true
		}
	}
	if v := os.Getenv("SKEW_PER_UNIT"); v != "" {
		if _, err := strconv.ParseFloat(v, 64); err == nil {
			m["SKEW_PER_UNIT"] = true
		}
	}
	if v := os.Getenv("MAX_MARGIN_USAGE"); v != "" {
		if _, err := strconv.ParseFloat(v, 64); err == nil {
			m["MAX_MARGIN_USAGE"] = true
		}
	}
	if v := os.Getenv("REFRESH_INTERVAL"); v != "" {
		if _, err := strconv.ParseFloat(v, 64); err == nil {
			m["REFRESH_INTERVAL"] = true
		}
	}
	if v := os.Getenv("REFRESH_INTERVAL_JITTER_S"); v != "" {
		if _, err := strconv.ParseFloat(v, 64); err == nil {
			m["REFRESH_INTERVAL_JITTER_S"] = true
		}
	}
	if v := os.Getenv("SHUTDOWN_CANCEL_TIMEOUT"); v != "" {
		if _, err := strconv.ParseFloat(v, 64); err == nil {
			m["SHUTDOWN_CANCEL_TIMEOUT"] = true
		}
	}
	if v := os.Getenv("AUTO_FLATTEN"); v != "" {
		if _, err := strconv.ParseBool(v); err == nil {
			m["AUTO_FLATTEN"] = true
		}
	}
	if v := os.Getenv("FLATTEN_AGGRESSION"); v != "" {
		if _, err := strconv.ParseFloat(v, 64); err == nil {
			m["FLATTEN_AGGRESSION"] = true
		}
	}
	if v := os.Getenv("DRY_RUN"); v != "" {
		if _, err := strconv.ParseBool(v); err == nil {
			m["DRY_RUN"] = true
		}
	}
	if v := os.Getenv("AUTO_SPREAD"); v != "" {
		if _, err := strconv.ParseBool(v); err == nil {
			m["AUTO_SPREAD"] = true
		}
	}
	if v := os.Getenv("SPREAD_MIN"); v != "" {
		if _, err := strconv.ParseFloat(v, 64); err == nil {
			m["SPREAD_MIN"] = true
		}
	}
	if v := os.Getenv("SPREAD_MAX"); v != "" {
		if _, err := strconv.ParseFloat(v, 64); err == nil {
			m["SPREAD_MAX"] = true
		}
	}
	if v := os.Getenv("SPREAD_NO_FILL_CYCLES"); v != "" {
		if _, err := strconv.ParseFloat(v, 64); err == nil {
			m["SPREAD_NO_FILL_CYCLES"] = true
		}
	}
	if v := os.Getenv("SPREAD_STEP"); v != "" {
		if _, err := strconv.ParseFloat(v, 64); err == nil {
			m["SPREAD_STEP"] = true
		}
	}
	if os.Getenv("BEARER_TOKEN") != "" {
		m["BEARER_TOKEN"] = true
	}
	if os.Getenv("SUBACCOUNT_ADDRESS") != "" {
		m["SUBACCOUNT_ADDRESS"] = true
	}
	if os.Getenv("PRIVATE_KEY") != "" {
		m["PRIVATE_KEY"] = true
	}
	if os.Getenv("NODE_API_KEY") != "" {
		m["NODE_API_KEY"] = true
	}
	if os.Getenv("PACKAGE_ADDRESS") != "" {
		m["PACKAGE_ADDRESS"] = true
	}
	if os.Getenv("APTOS_FULLNODE_URL") != "" {
		m["APTOS_FULLNODE_URL"] = true
	}
	if os.Getenv("MARKET_ADDR") != "" {
		m["MARKET_ADDR"] = true
	}
	if os.Getenv("REST_API_BASE") != "" {
		m["REST_API_BASE"] = true
	}
	if os.Getenv("TG_BOT_TOKEN") != "" {
		m["TG_BOT_TOKEN"] = true
	}
	if v := os.Getenv("TG_ADMIN_ID"); v != "" {
		if _, err := strconv.ParseInt(v, 10, 64); err == nil {
			m["TG_ADMIN_ID"] = true
		}
	}
	if v := os.Getenv("TG_ALERT_INVENTORY"); v != "" {
		if _, err := strconv.ParseBool(v); err == nil {
			m["TG_ALERT_INVENTORY"] = true
		}
	}
	if v := os.Getenv("TG_ALERT_INVENTORY_INTERVAL_MIN"); v != "" {
		if _, err := strconv.ParseFloat(v, 64); err == nil {
			m["TG_ALERT_INVENTORY_INTERVAL_MIN"] = true
		}
	}
	if v := os.Getenv("TG_STRICT_START"); v != "" {
		if _, err := strconv.ParseBool(v); err == nil {
			m["TG_STRICT_START"] = true
		}
	}
	return m
}

func urlExplicit(flagsSet map[string]struct{}, explicitFile, explicitEnv map[string]bool, flagName, envKey string) bool {
	if _, ok := flagsSet[flagName]; ok {
		return true
	}
	if explicitFile != nil && explicitFile[envKey] {
		return true
	}
	if explicitEnv != nil && explicitEnv[envKey] {
		return true
	}
	return false
}

// configPathFromCLI returns the effective config path: CONFIG_FILE env then
// last -config/--config on the command line (later wins).
func configPathFromCLI(envPath string, args []string) string {
	out := strings.TrimSpace(envPath)
	i := 1
	for i < len(args) {
		a := args[i]
		if a == "--" {
			break
		}
		switch {
		case strings.HasPrefix(a, "-config="):
			out = strings.TrimSpace(strings.TrimPrefix(a, "-config="))
			i++
		case strings.HasPrefix(a, "--config="):
			out = strings.TrimSpace(strings.TrimPrefix(a, "--config="))
			i++
		case a == "-config" || a == "--config":
			if i+1 < len(args) {
				next := args[i+1]
				if next != "--" && !strings.HasPrefix(next, "-") {
					out = strings.TrimSpace(next)
					i += 2
					continue
				}
			}
			i++
		default:
			i++
		}
	}
	return strings.TrimSpace(out)
}

func registerAllFlags(fs *flag.FlagSet, cfg *Config) {
	fs.StringVar(&cfg.Network, "network", cfg.Network,
		"Network preset: testnet | mainnet  (sets default URLs and package address)")
	fs.StringVar(&cfg.Locale, "locale", cfg.Locale, "UI language for Telegram copy: zh | en (overrides LOCALE / BOT_LOCALE)")
	fs.StringVar(&cfg.MarketName, "market-name", cfg.MarketName, "Market symbol (e.g. BTC/USD)")
	fs.Float64Var(&cfg.Spread, "spread", cfg.Spread, "Total spread fraction (0.001 = 0.1%)")
	fs.Float64Var(&cfg.OrderSize, "order-size", cfg.OrderSize, "Base units per side per quote")
	fs.Float64Var(&cfg.MaxInventory, "max-inventory", cfg.MaxInventory, "Stop quoting when abs(position) >= this")
	fs.Float64Var(&cfg.SkewPerUnit, "skew-per-unit", cfg.SkewPerUnit, "Extra half-spread per unit of inventory")
	fs.Float64Var(&cfg.MaxMarginUsage, "max-margin-usage", cfg.MaxMarginUsage, "Pause when cross_margin_ratio > this")
	fs.Float64Var(&cfg.RefreshInterval, "refresh-interval", cfg.RefreshInterval, "Seconds between cycles")
	fs.Float64Var(&cfg.RefreshIntervalJitterS, "refresh-interval-jitter", cfg.RefreshIntervalJitterS,
		"Seconds; sleep duration is uniform in [refresh-interval−jitter, refresh-interval+jitter] (lower bound floored at 0.01s); 0 disables")
	fs.Float64Var(&cfg.ShutdownCancelTimeoutS, "shutdown-cancel-timeout", cfg.ShutdownCancelTimeoutS,
		"Seconds; time budget for graceful shutdown bulk cancel (CancelBulkOrders); min 5 after validation")
	fs.BoolVar(&cfg.AutoFlatten, "auto-flatten", cfg.AutoFlatten, "Auto reduce-only order when inventory hits limit")
	fs.Float64Var(&cfg.FlattenAggression, "flatten-aggression", cfg.FlattenAggression, "POST_ONLY flatten: fraction above mid (sell) / below mid (buy); mid is API reference")
	fs.Float64Var(&cfg.FlattenMaxDeviation, "flatten-max-deviation", cfg.FlattenMaxDeviation, "Cap sell / floor buy vs mid for POST_ONLY flatten (0 = no bound)")
	fs.BoolVar(&cfg.DryRun, "dry-run", cfg.DryRun, "Log without sending transactions")
	fs.BoolVar(&cfg.AutoSpread, "auto-spread", cfg.AutoSpread, "Automatically narrow spread after spread-no-fill-cycles cycles with no fill")
	fs.Float64Var(&cfg.SpreadMin, "spread-min", cfg.SpreadMin, "Minimum spread the auto-adjuster will narrow to")
	fs.Float64Var(&cfg.SpreadMax, "spread-max", cfg.SpreadMax, "Maximum spread ceiling")
	fs.IntVar(&cfg.SpreadNoFillCycles, "spread-no-fill-cycles", cfg.SpreadNoFillCycles, "Cycles without fill before adjusting spread")
	fs.Float64Var(&cfg.SpreadStep, "spread-step", cfg.SpreadStep, "Amount to narrow spread per adjustment step")
	fs.StringVar(&cfg.PackageAddress, "package-address", cfg.PackageAddress, "Move package address (overrides network profile)")
	fs.StringVar(&cfg.AptosFullnodeURL, "fullnode-url", cfg.AptosFullnodeURL, "Aptos fullnode URL (overrides network profile)")
	fs.StringVar(&cfg.RestAPIBase, "api-base", cfg.RestAPIBase, "Decibel REST API base URL (overrides network profile)")

	fs.StringVar(&cfg.BearerToken, "bearer-token", cfg.BearerToken, "Decibel REST bearer token (overrides BEARER_TOKEN)")
	fs.StringVar(&cfg.SubaccountAddress, "subaccount", cfg.SubaccountAddress, "Subaccount object address (overrides SUBACCOUNT_ADDRESS)")
	fs.StringVar(&cfg.PrivateKey, "private-key", cfg.PrivateKey, "Ed25519 private key hex or AIP-80 (overrides PRIVATE_KEY); visible in process list")
	fs.StringVar(&cfg.NodeAPIKey, "node-api-key", cfg.NodeAPIKey, "Fullnode API key (overrides NODE_API_KEY; falls back to bearer token)")
	fs.StringVar(&cfg.MarketAddrOverride, "market-addr", cfg.MarketAddrOverride, "Perp market object address (overrides MARKET_ADDR); skips API discovery")

	fs.StringVar(&cfg.TGBotToken, "tg-token", cfg.TGBotToken,
		"Telegram bot token (overrides TG_BOT_TOKEN); visible in process list")
	fs.Int64Var(&cfg.TGAdminID, "tg-admin-id", cfg.TGAdminID,
		"Telegram admin user ID (overrides TG_ADMIN_ID); visible in process list")
	fs.BoolVar(&cfg.TGAlertInventory, "tg-alert-inventory", cfg.TGAlertInventory,
		"Enable Telegram alert when position exceeds max-inventory (overrides TG_ALERT_INVENTORY)")
	fs.IntVar(&cfg.TGAlertInventoryInterval, "tg-alert-interval", cfg.TGAlertInventoryInterval,
		"Minutes between repeated inventory-limit Telegram alerts (overrides TG_ALERT_INVENTORY_INTERVAL_MIN)")
	fs.BoolVar(&cfg.TGStrictStart, "tg-strict-start", cfg.TGStrictStart,
		"When Telegram is enabled, exit if bot init/ready/setCommands fails (overrides TG_STRICT_START)")

	var configPathDummy string
	configDefault := strings.TrimSpace(os.Getenv("CONFIG_FILE"))
	fs.StringVar(&configPathDummy, "config", configDefault,
		"Path to JSON/YAML/TOML config file (overrides CONFIG_FILE); file is merged before other flags take effect")

	fs.Usage = func() {
		fmt.Fprintf(fs.Output(), "Usage of %s [flags]\n\n", fs.Name())
		fmt.Fprintf(fs.Output(), "Initialization:\n")
		fmt.Fprintf(fs.Output(), "  -init-config[=path]   write minimal YAML (network + core trading; secrets blank) (default path: %s)\n\n", InitConfigDefaultPath)
		fs.PrintDefaults()
	}
}
