package config

import (
	"fmt"
	"reflect"
	"time"

	"github.com/go-playground/validator/v10"
	"github.com/go-viper/mapstructure/v2"
	"github.com/shopspring/decimal"
	"github.com/spf13/viper"
)

// TrailingStopCfg represents trailing stop configuration
type TrailingStopCfg struct {
	ActivationPrice decimal.Decimal `mapstructure:"activation_price"`
	TrailingDelta   decimal.Decimal `mapstructure:"trailing_delta"`
}

// Config holds all bot configuration
type Config struct {
	Env string `mapstructure:"env" validate:"oneof=testnet mainnet"`

	Decibel struct {
		BearerToken      string `mapstructure:"bearer_token" validate:"required"`
		ApiWalletPrivKey string `mapstructure:"api_wallet_private_key" validate:"required"`
		ApiWalletAddress string `mapstructure:"api_wallet_address"`
		SubaccountAddr   string `mapstructure:"subaccount_address" validate:"required"`
		MarketName       string `mapstructure:"market_name" validate:"required"`
		BuilderCode      string `mapstructure:"builder_code"`
	} `mapstructure:"decibel"`

	Aptos struct {
		FullnodeURL string `mapstructure:"fullnode_url"`
	} `mapstructure:"aptos"`

	Strategy struct {
		Leverage                  int              `mapstructure:"leverage"`
		BidSpread                 decimal.Decimal  `mapstructure:"bid_spread"`
		AskSpread                 decimal.Decimal  `mapstructure:"ask_spread"`
		OrderAmount               decimal.Decimal  `mapstructure:"order_amount"`
		OrderLevels               int              `mapstructure:"order_levels"`
		OrderLevelSpread          decimal.Decimal  `mapstructure:"order_level_spread"`
		OrderLevelAmount          decimal.Decimal  `mapstructure:"order_level_amount"`
		OrderRefreshTime          time.Duration    `mapstructure:"order_refresh_time"`
		OrderRefreshTolerancePct  decimal.Decimal  `mapstructure:"order_refresh_tolerance_pct"`
		FilledOrderDelay          time.Duration    `mapstructure:"filled_order_delay"`
		MinimumSpread             decimal.Decimal  `mapstructure:"minimum_spread"`
		LongProfitTakingSpread    decimal.Decimal  `mapstructure:"long_profit_taking_spread"`
		ShortProfitTakingSpread   decimal.Decimal  `mapstructure:"short_profit_taking_spread"`
		StopLossSpread            decimal.Decimal  `mapstructure:"stop_loss_spread"`
		StopLossSlippageBuffer    decimal.Decimal  `mapstructure:"stop_loss_slippage_buffer"`
		TimeBetweenStopLossOrders time.Duration    `mapstructure:"time_between_stop_loss_orders"`
		PriceCeiling              decimal.Decimal  `mapstructure:"price_ceiling"`
		PriceFloor                decimal.Decimal  `mapstructure:"price_floor"`
		OrderOptimizationEnabled  bool             `mapstructure:"order_optimization_enabled"`
		AskOrderOptimizationDepth decimal.Decimal  `mapstructure:"ask_order_optimization_depth"`
		BidOrderOptimizationDepth decimal.Decimal  `mapstructure:"bid_order_optimization_depth"`

		StopLoss     decimal.Decimal  `mapstructure:"stop_loss"`
		TakeProfit   decimal.Decimal  `mapstructure:"take_profit"`
		TimeLimit    time.Duration    `mapstructure:"time_limit"`
		TrailingStop *TrailingStopCfg `mapstructure:"trailing_stop"`

		UseBulkOrders    bool          `mapstructure:"use_bulk_orders"`
		PostOnly         bool          `mapstructure:"post_only"`
		MaxPendingTx     int           `mapstructure:"max_pending_tx"`
		TxPollInterval   time.Duration `mapstructure:"tx_poll_interval"`
		WsReconnectDelay time.Duration `mapstructure:"ws_reconnect_delay"`
	} `mapstructure:"strategy"`
}

// Defaults applies default values
func Defaults() map[string]interface{} {
	return map[string]interface{}{
		"env": "testnet",

		"strategy.leverage":                      10,
		"strategy.bid_spread":                    "0.01",
		"strategy.ask_spread":                    "0.01",
		"strategy.order_amount":                  "0.1",
		"strategy.order_levels":                  1,
		"strategy.order_level_spread":            "0.01",
		"strategy.order_level_amount":            "0",
		"strategy.order_refresh_time":            "30s",
		"strategy.order_refresh_tolerance_pct":   "0",
		"strategy.filled_order_delay":            "60s",
		"strategy.minimum_spread":                "-100",
		"strategy.long_profit_taking_spread":     "0",
		"strategy.short_profit_taking_spread":    "0",
		"strategy.stop_loss_spread":              "0",
		"strategy.stop_loss_slippage_buffer":     "0.005",
		"strategy.time_between_stop_loss_orders": "60s",
		"strategy.price_ceiling":                 "-1",
		"strategy.price_floor":                   "-1",
		"strategy.order_optimization_enabled":    false,
		"strategy.ask_order_optimization_depth":  "0",
		"strategy.bid_order_optimization_depth":  "0",

		"strategy.stop_loss":     "0.03",
		"strategy.take_profit":   "0.02",
		"strategy.time_limit":    "45m",

		"strategy.use_bulk_orders":     true,
		"strategy.post_only":           true,
		"strategy.max_pending_tx":      10,
		"strategy.tx_poll_interval":    "2s",
		"strategy.ws_reconnect_delay":  "5s",
	}
}

// Load reads config from file and environment variables
func Load(path string) (*Config, error) {
	v := viper.New()
	for k, val := range Defaults() {
		v.SetDefault(k, val)
	}

	if path != "" {
		v.SetConfigFile(path)
		if err := v.ReadInConfig(); err != nil {
			return nil, fmt.Errorf("read config: %w", err)
		}
	}

	v.AutomaticEnv()
	v.SetEnvPrefix("DECIBEL")

	var cfg Config
	if err := v.Unmarshal(&cfg, func(c *mapstructure.DecoderConfig) {
		c.DecodeHook = mapstructure.ComposeDecodeHookFunc(
			c.DecodeHook,
			func(from, to reflect.Type, data interface{}) (interface{}, error) {
				if to == reflect.TypeOf(decimal.Decimal{}) {
					switch v := data.(type) {
					case string:
						return decimal.NewFromString(v)
					case float64:
						return decimal.NewFromFloat(v), nil
					case int:
						return decimal.NewFromInt(int64(v)), nil
					}
				}
				return data, nil
			},
		)
	}); err != nil {
		return nil, fmt.Errorf("unmarshal config: %w", err)
	}

	validate := validator.New()
	if err := validate.Struct(&cfg); err != nil {
		return nil, fmt.Errorf("validate config: %w", err)
	}

	return &cfg, nil
}

// BaseURLs returns REST, WS and Aptos fullnode base URLs for the configured environment.
// Fullnode defaults to standard Aptos public nodes; override via aptos.fullnode_url.
func (c *Config) BaseURLs() (rest, ws, fullnode string) {
	switch c.Env {
	case "testnet":
		return "https://api.testnet.aptoslabs.com/decibel",
			"wss://api.testnet.aptoslabs.com/decibel/ws",
			"https://fullnode.testnet.aptoslabs.com/v1"
	case "mainnet":
		return "https://api.mainnet.aptoslabs.com/decibel",
			"wss://api.mainnet.aptoslabs.com/decibel/ws",
			"https://fullnode.mainnet.aptoslabs.com/v1"
	default: // testnet
		return "https://api.testnet.aptoslabs.com/decibel",
			"wss://api.testnet.aptoslabs.com/decibel/ws",
			"https://fullnode.testnet.aptoslabs.com/v1"
	}
}
