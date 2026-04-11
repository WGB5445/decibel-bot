package config

import (
	"bytes"
	"encoding/json"
	"fmt"
	"path/filepath"
	"strings"

	"github.com/pelletier/go-toml/v2"
	"gopkg.in/yaml.v3"
)

// FileConfig is the on-disk shape (JSON / YAML / TOML). Pointer fields mean
// "omit": nil does not override; non-nil overrides the current Config value.
type FileConfig struct {
	Network               *string  `json:"network,omitempty"                yaml:"network,omitempty"                toml:"network,omitempty"`
	MarketName            *string  `json:"market_name,omitempty"            yaml:"market_name,omitempty"            toml:"market_name,omitempty"`
	Spread                *float64 `json:"spread,omitempty"                 yaml:"spread,omitempty"                 toml:"spread,omitempty"`
	OrderSize             *float64 `json:"order_size,omitempty"             yaml:"order_size,omitempty"             toml:"order_size,omitempty"`
	MaxInventory          *float64 `json:"max_inventory,omitempty"          yaml:"max_inventory,omitempty"          toml:"max_inventory,omitempty"`
	SkewPerUnit           *float64 `json:"skew_per_unit,omitempty"          yaml:"skew_per_unit,omitempty"          toml:"skew_per_unit,omitempty"`
	MaxMarginUsage        *float64 `json:"max_margin_usage,omitempty"       yaml:"max_margin_usage,omitempty"       toml:"max_margin_usage,omitempty"`
	RefreshInterval       *float64 `json:"refresh_interval,omitempty"       yaml:"refresh_interval,omitempty"       toml:"refresh_interval,omitempty"`
	RefreshIntervalJitter *float64 `json:"refresh_interval_jitter,omitempty" yaml:"refresh_interval_jitter,omitempty" toml:"refresh_interval_jitter,omitempty"`
	CooldownS             *float64 `json:"cooldown_s,omitempty"             yaml:"cooldown_s,omitempty"             toml:"cooldown_s,omitempty"`
	CancelResyncS         *float64 `json:"cancel_resync_s,omitempty"        yaml:"cancel_resync_s,omitempty"        toml:"cancel_resync_s,omitempty"`
	AutoFlatten           *bool    `json:"auto_flatten,omitempty"           yaml:"auto_flatten,omitempty"           toml:"auto_flatten,omitempty"`
	FlattenAggression     *float64 `json:"flatten_aggression,omitempty"     yaml:"flatten_aggression,omitempty"     toml:"flatten_aggression,omitempty"`
	DryRun                *bool    `json:"dry_run,omitempty"                yaml:"dry_run,omitempty"                toml:"dry_run,omitempty"`
	AutoSpread            *bool    `json:"auto_spread,omitempty"            yaml:"auto_spread,omitempty"            toml:"auto_spread,omitempty"`
	SpreadMin             *float64 `json:"spread_min,omitempty"             yaml:"spread_min,omitempty"             toml:"spread_min,omitempty"`
	SpreadMax             *float64 `json:"spread_max,omitempty"             yaml:"spread_max,omitempty"             toml:"spread_max,omitempty"`
	SpreadNoFillCycles    *int     `json:"spread_no_fill_cycles,omitempty"  yaml:"spread_no_fill_cycles,omitempty"  toml:"spread_no_fill_cycles,omitempty"`
	SpreadStep            *float64 `json:"spread_step,omitempty"            yaml:"spread_step,omitempty"            toml:"spread_step,omitempty"`
	BearerToken           *string  `json:"bearer_token,omitempty"           yaml:"bearer_token,omitempty"           toml:"bearer_token,omitempty"`
	SubaccountAddress     *string  `json:"subaccount_address,omitempty"     yaml:"subaccount_address,omitempty"     toml:"subaccount_address,omitempty"`
	PrivateKey            *string  `json:"private_key,omitempty"            yaml:"private_key,omitempty"            toml:"private_key,omitempty"`
	NodeAPIKey            *string  `json:"node_api_key,omitempty"           yaml:"node_api_key,omitempty"           toml:"node_api_key,omitempty"`
	PackageAddress        *string  `json:"package_address,omitempty"        yaml:"package_address,omitempty"        toml:"package_address,omitempty"`
	AptosFullnodeURL      *string  `json:"aptos_fullnode_url,omitempty"     yaml:"aptos_fullnode_url,omitempty"     toml:"aptos_fullnode_url,omitempty"`
	MarketAddr            *string  `json:"market_addr,omitempty"            yaml:"market_addr,omitempty"            toml:"market_addr,omitempty"`
	RestAPIBase           *string  `json:"rest_api_base,omitempty"          yaml:"rest_api_base,omitempty"          toml:"rest_api_base,omitempty"`
	TGBotToken            *string  `json:"tg_bot_token,omitempty"           yaml:"tg_bot_token,omitempty"           toml:"tg_bot_token,omitempty"`
	TGAdminID             *int64   `json:"tg_admin_id,omitempty"            yaml:"tg_admin_id,omitempty"            toml:"tg_admin_id,omitempty"`
	TGAlertInventory      *bool    `json:"tg_alert_inventory,omitempty"     yaml:"tg_alert_inventory,omitempty"     toml:"tg_alert_inventory,omitempty"`
	TGAlertInvIntervalMin *int     `json:"tg_alert_inventory_interval_min,omitempty" yaml:"tg_alert_inventory_interval_min,omitempty" toml:"tg_alert_inventory_interval_min,omitempty"`
	TGStrictStart         *bool    `json:"tg_strict_start,omitempty"        yaml:"tg_strict_start,omitempty"        toml:"tg_strict_start,omitempty"`
}

func decodeFileConfig(data []byte, ext string) (*FileConfig, error) {
	ext = strings.ToLower(strings.TrimSpace(ext))
	var fc FileConfig
	var err error
	switch ext {
	case ".json":
		dec := json.NewDecoder(bytes.NewReader(data))
		dec.DisallowUnknownFields()
		err = dec.Decode(&fc)
	case ".yaml", ".yml":
		dec := yaml.NewDecoder(bytes.NewReader(data))
		dec.KnownFields(true)
		err = dec.Decode(&fc)
	case ".toml":
		err = toml.Unmarshal(data, &fc)
	default:
		return nil, fmt.Errorf("unsupported config format %q (use .json, .yaml, .yml, or .toml)", ext)
	}
	if err != nil {
		return nil, err
	}
	return &fc, nil
}

// applyFileConfig merges non-nil FileConfig fields into dst and records which
// environment-style keys were set by the file (for URL / network side-effects).
func applyFileConfig(dst *Config, src *FileConfig, explicitFile map[string]bool) {
	set := func(key string) {
		explicitFile[key] = true
	}
	if src == nil {
		return
	}
	if src.Network != nil {
		dst.Network = strings.TrimSpace(*src.Network)
		set("NETWORK")
	}
	if src.MarketName != nil {
		dst.MarketName = strings.TrimSpace(*src.MarketName)
		set("MARKET_NAME")
	}
	if src.Spread != nil {
		dst.Spread = *src.Spread
		set("SPREAD")
	}
	if src.OrderSize != nil {
		dst.OrderSize = *src.OrderSize
		set("ORDER_SIZE")
	}
	if src.MaxInventory != nil {
		dst.MaxInventory = *src.MaxInventory
		set("MAX_INVENTORY")
	}
	if src.SkewPerUnit != nil {
		dst.SkewPerUnit = *src.SkewPerUnit
		set("SKEW_PER_UNIT")
	}
	if src.MaxMarginUsage != nil {
		dst.MaxMarginUsage = *src.MaxMarginUsage
		set("MAX_MARGIN_USAGE")
	}
	if src.RefreshInterval != nil {
		dst.RefreshInterval = *src.RefreshInterval
		set("REFRESH_INTERVAL")
	}
	if src.RefreshIntervalJitter != nil {
		dst.RefreshIntervalJitterS = *src.RefreshIntervalJitter
		set("REFRESH_INTERVAL_JITTER_S")
	}
	if src.CooldownS != nil {
		dst.CooldownS = *src.CooldownS
		set("COOLDOWN_S")
	}
	if src.CancelResyncS != nil {
		dst.CancelResyncS = *src.CancelResyncS
		set("CANCEL_RESYNC_S")
	}
	if src.AutoFlatten != nil {
		dst.AutoFlatten = *src.AutoFlatten
		set("AUTO_FLATTEN")
	}
	if src.FlattenAggression != nil {
		dst.FlattenAggression = *src.FlattenAggression
		set("FLATTEN_AGGRESSION")
	}
	if src.DryRun != nil {
		dst.DryRun = *src.DryRun
		set("DRY_RUN")
	}
	if src.AutoSpread != nil {
		dst.AutoSpread = *src.AutoSpread
		set("AUTO_SPREAD")
	}
	if src.SpreadMin != nil {
		dst.SpreadMin = *src.SpreadMin
		set("SPREAD_MIN")
	}
	if src.SpreadMax != nil {
		dst.SpreadMax = *src.SpreadMax
		set("SPREAD_MAX")
	}
	if src.SpreadNoFillCycles != nil {
		dst.SpreadNoFillCycles = *src.SpreadNoFillCycles
		set("SPREAD_NO_FILL_CYCLES")
	}
	if src.SpreadStep != nil {
		dst.SpreadStep = *src.SpreadStep
		set("SPREAD_STEP")
	}
	if src.BearerToken != nil {
		dst.BearerToken = *src.BearerToken
		set("BEARER_TOKEN")
	}
	if src.SubaccountAddress != nil {
		dst.SubaccountAddress = strings.TrimSpace(*src.SubaccountAddress)
		set("SUBACCOUNT_ADDRESS")
	}
	if src.PrivateKey != nil {
		dst.PrivateKey = *src.PrivateKey
		set("PRIVATE_KEY")
	}
	if src.NodeAPIKey != nil {
		dst.NodeAPIKey = *src.NodeAPIKey
		set("NODE_API_KEY")
	}
	if src.PackageAddress != nil {
		dst.PackageAddress = strings.TrimSpace(*src.PackageAddress)
		set("PACKAGE_ADDRESS")
	}
	if src.AptosFullnodeURL != nil {
		dst.AptosFullnodeURL = strings.TrimSpace(*src.AptosFullnodeURL)
		set("APTOS_FULLNODE_URL")
	}
	if src.MarketAddr != nil {
		dst.MarketAddrOverride = strings.TrimSpace(*src.MarketAddr)
		set("MARKET_ADDR")
	}
	if src.RestAPIBase != nil {
		dst.RestAPIBase = strings.TrimSpace(*src.RestAPIBase)
		set("REST_API_BASE")
	}
	if src.TGBotToken != nil {
		dst.TGBotToken = *src.TGBotToken
		set("TG_BOT_TOKEN")
	}
	if src.TGAdminID != nil {
		dst.TGAdminID = *src.TGAdminID
		set("TG_ADMIN_ID")
	}
	if src.TGAlertInventory != nil {
		dst.TGAlertInventory = *src.TGAlertInventory
		set("TG_ALERT_INVENTORY")
	}
	if src.TGAlertInvIntervalMin != nil {
		dst.TGAlertInventoryInterval = *src.TGAlertInvIntervalMin
		set("TG_ALERT_INVENTORY_INTERVAL_MIN")
	}
	if src.TGStrictStart != nil {
		dst.TGStrictStart = *src.TGStrictStart
		set("TG_STRICT_START")
	}
}

func configExtFromPath(path string) (string, error) {
	ext := strings.ToLower(filepath.Ext(path))
	if ext == "" {
		return "", fmt.Errorf("config file %q has no extension (.json, .yaml, .yml, .toml)", path)
	}
	return ext, nil
}
