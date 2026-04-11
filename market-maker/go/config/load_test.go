package config

import (
	"strings"
	"testing"
)

// 64 hex chars (32 bytes) — satisfies ParsePrivateKey minimum length.
var testPrivateKeyHex = "0x" + strings.Repeat("11", 32)

func TestLoad_priority_cliOverFileOverEnv(t *testing.T) {
	t.Setenv("NETWORK", "testnet")
	t.Setenv("BEARER_TOKEN", "env-token")
	t.Setenv("SUBACCOUNT_ADDRESS", "0xsub")
	t.Setenv("PRIVATE_KEY", testPrivateKeyHex)
	t.Setenv("SPREAD", "0.01")

	yaml := "spread: 0.02\n"
	cfg, err := LoadWith(LoadOptions{
		Args:       []string{"prog", "-spread", "0.03"},
		ConfigData: []byte(yaml),
		ConfigExt:  ".yaml",
	})
	if err != nil {
		t.Fatal(err)
	}
	if cfg.Spread != 0.03 {
		t.Fatalf("spread: want 0.03 (CLI), got %v", cfg.Spread)
	}
}

func TestLoad_priority_fileOverEnv(t *testing.T) {
	t.Setenv("NETWORK", "testnet")
	t.Setenv("BEARER_TOKEN", "env-token")
	t.Setenv("SUBACCOUNT_ADDRESS", "0xsub")
	t.Setenv("PRIVATE_KEY", testPrivateKeyHex)
	t.Setenv("SPREAD", "0.01")

	yaml := "spread: 0.02\n"
	cfg, err := LoadWith(LoadOptions{
		Args:       []string{"prog"},
		ConfigData: []byte(yaml),
		ConfigExt:  ".yaml",
	})
	if err != nil {
		t.Fatal(err)
	}
	if cfg.Spread != 0.02 {
		t.Fatalf("spread: want 0.02 (file), got %v", cfg.Spread)
	}
}

func TestLoad_decodeFormats_equivalent(t *testing.T) {
	t.Setenv("NETWORK", "testnet")
	t.Setenv("BEARER_TOKEN", "t")
	t.Setenv("SUBACCOUNT_ADDRESS", "0x1")
	t.Setenv("PRIVATE_KEY", testPrivateKeyHex)

	jsonData := `{"spread": 0.07, "dry_run": true}`
	yamlData := "spread: 0.07\ndry_run: true\n"
	tomlData := "spread = 0.07\ndry_run = true\n"

	for _, tc := range []struct {
		name string
		ext  string
		data []byte
	}{
		{"json", ".json", []byte(jsonData)},
		{"yaml", ".yaml", []byte(yamlData)},
		{"toml", ".toml", []byte(tomlData)},
	} {
		t.Run(tc.name, func(t *testing.T) {
			cfg, err := LoadWith(LoadOptions{
				Args:       []string{"prog"},
				ConfigData: tc.data,
				ConfigExt:  tc.ext,
			})
			if err != nil {
				t.Fatal(err)
			}
			if cfg.Spread != 0.07 || !cfg.DryRun {
				t.Fatalf("got spread=%v dry_run=%v", cfg.Spread, cfg.DryRun)
			}
		})
	}
}

func TestLoad_networkSwitch_refreshesURLs(t *testing.T) {
	t.Setenv("NETWORK", "testnet")
	t.Setenv("BEARER_TOKEN", "t")
	t.Setenv("SUBACCOUNT_ADDRESS", "0x1")
	t.Setenv("PRIVATE_KEY", testPrivateKeyHex)

	want := networkProfiles["mainnet"].RestAPIBase
	yaml := "network: mainnet\n"
	cfg, err := LoadWith(LoadOptions{
		Args:       []string{"prog"},
		ConfigData: []byte(yaml),
		ConfigExt:  ".yaml",
	})
	if err != nil {
		t.Fatal(err)
	}
	if cfg.Network != "mainnet" {
		t.Fatalf("network: %q", cfg.Network)
	}
	if cfg.RestAPIBase != want {
		t.Fatalf("RestAPIBase: want %q, got %q", want, cfg.RestAPIBase)
	}
}

func TestLoad_networkSwitch_respectsExplicitEnvRestAPI(t *testing.T) {
	t.Setenv("NETWORK", "testnet")
	t.Setenv("BEARER_TOKEN", "t")
	t.Setenv("SUBACCOUNT_ADDRESS", "0x1")
	t.Setenv("PRIVATE_KEY", testPrivateKeyHex)
	custom := "https://example.invalid/rest"
	t.Setenv("REST_API_BASE", custom)

	yaml := "network: mainnet\n"
	cfg, err := LoadWith(LoadOptions{
		Args:       []string{"prog"},
		ConfigData: []byte(yaml),
		ConfigExt:  ".yaml",
	})
	if err != nil {
		t.Fatal(err)
	}
	if cfg.RestAPIBase != custom {
		t.Fatalf("RestAPIBase: want %q, got %q", custom, cfg.RestAPIBase)
	}
}

func TestLoad_networkSwitch_respectsExplicitFileRestAPI(t *testing.T) {
	t.Setenv("NETWORK", "testnet")
	t.Setenv("BEARER_TOKEN", "t")
	t.Setenv("SUBACCOUNT_ADDRESS", "0x1")
	t.Setenv("PRIVATE_KEY", testPrivateKeyHex)

	custom := "https://file.override/rest"
	yaml := "network: mainnet\nrest_api_base: " + custom + "\n"
	cfg, err := LoadWith(LoadOptions{
		Args:       []string{"prog"},
		ConfigData: []byte(yaml),
		ConfigExt:  ".yaml",
	})
	if err != nil {
		t.Fatal(err)
	}
	if cfg.RestAPIBase != custom {
		t.Fatalf("RestAPIBase: want %q, got %q", custom, cfg.RestAPIBase)
	}
}

func TestLoad_yaml_unknownField(t *testing.T) {
	t.Setenv("NETWORK", "testnet")
	t.Setenv("BEARER_TOKEN", "t")
	t.Setenv("SUBACCOUNT_ADDRESS", "0x1")
	t.Setenv("PRIVATE_KEY", testPrivateKeyHex)

	yaml := "spread: 0.01\nnot_a_real_key: 1\n"
	_, err := LoadWith(LoadOptions{
		Args:       []string{"prog"},
		ConfigData: []byte(yaml),
		ConfigExt:  ".yaml",
	})
	if err == nil {
		t.Fatal("expected error for unknown YAML field")
	}
}

func TestLoad_json_unknownField(t *testing.T) {
	t.Setenv("NETWORK", "testnet")
	t.Setenv("BEARER_TOKEN", "t")
	t.Setenv("SUBACCOUNT_ADDRESS", "0x1")
	t.Setenv("PRIVATE_KEY", testPrivateKeyHex)

	data := `{"spread":0.01,"not_a_real_key":true}`
	_, err := LoadWith(LoadOptions{
		Args:       []string{"prog"},
		ConfigData: []byte(data),
		ConfigExt:  ".json",
	})
	if err == nil {
		t.Fatal("expected error for unknown JSON field")
	}
}

func TestLoad_unsupportedConfigExt(t *testing.T) {
	t.Setenv("NETWORK", "testnet")
	t.Setenv("BEARER_TOKEN", "t")
	t.Setenv("SUBACCOUNT_ADDRESS", "0x1")
	t.Setenv("PRIVATE_KEY", testPrivateKeyHex)

	_, err := LoadWith(LoadOptions{
		Args:       []string{"prog"},
		ConfigData: []byte("x"),
		ConfigExt:  ".xml",
	})
	if err == nil {
		t.Fatal("expected error for unsupported extension")
	}
}

func TestValidate_negativeRefreshIntervalJitter(t *testing.T) {
	t.Parallel()
	c := &Config{
		BearerToken:            "t",
		SubaccountAddress:      "0x1",
		PrivateKey:             testPrivateKeyHex,
		PackageAddress:         "0xpackage",
		RefreshIntervalJitterS: -0.5,
	}
	if err := c.validate(); err == nil {
		t.Fatal("expected error for negative refresh jitter")
	}
}

func TestLoad_marketAddr_cliOverridesEnv(t *testing.T) {
	t.Setenv("NETWORK", "testnet")
	t.Setenv("BEARER_TOKEN", "t")
	t.Setenv("SUBACCOUNT_ADDRESS", "0x1")
	t.Setenv("PRIVATE_KEY", testPrivateKeyHex)
	t.Setenv("MARKET_ADDR", "0xenv")

	cfg, err := LoadWith(LoadOptions{
		Args: []string{"prog", "-market-addr", "0xcli"},
	})
	if err != nil {
		t.Fatal(err)
	}
	if cfg.MarketAddrOverride != "0xcli" {
		t.Fatalf("market addr: %q", cfg.MarketAddrOverride)
	}
}
