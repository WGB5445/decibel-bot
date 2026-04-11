package config

import (
	"fmt"
	"os"
	"strings"

	"gopkg.in/yaml.v3"
)

// InitConfigDefaultPath is the output path when -init-config is given without a value.
const InitConfigDefaultPath = "decibel-mm-bot.yaml"

// ParseInitConfigFlag scans argv for -init-config / --init-config. If present, it returns
// the output path and true. A bare -init-config uses InitConfigDefaultPath.
func ParseInitConfigFlag(args []string) (path string, ok bool) {
	for i := 1; i < len(args); i++ {
		a := args[i]
		if a == "--" {
			break
		}
		switch {
		case strings.HasPrefix(a, "-init-config="):
			p := strings.TrimSpace(strings.TrimPrefix(a, "-init-config="))
			if p == "" {
				p = InitConfigDefaultPath
			}
			return p, true
		case strings.HasPrefix(a, "--init-config="):
			p := strings.TrimSpace(strings.TrimPrefix(a, "--init-config="))
			if p == "" {
				p = InitConfigDefaultPath
			}
			return p, true
		case a == "-init-config" || a == "--init-config":
			if i+1 < len(args) {
				next := args[i+1]
				if next != "--" && !strings.HasPrefix(next, "-") {
					return strings.TrimSpace(next), true
				}
			}
			return InitConfigDefaultPath, true
		}
	}
	return "", false
}

// initTemplateYAMLDoc is a minimal, pointer-free shape for yaml.Marshal so empty-string
// credentials are always emitted (FileConfig uses omitempty on pointers which can drop "" keys).
// Field names must stay aligned with FileConfig yaml tags for decodeFileConfig(KnownFields).
type initTemplateYAMLDoc struct {
	Network               string  `yaml:"network"`
	MarketName            string  `yaml:"market_name"`
	Spread                float64 `yaml:"spread"`
	OrderSize             float64 `yaml:"order_size"`
	MaxInventory          float64 `yaml:"max_inventory"`
	RefreshInterval       float64 `yaml:"refresh_interval"`
	RefreshIntervalJitter float64 `yaml:"refresh_interval_jitter"`
	BearerToken           string  `yaml:"bearer_token"`
	SubaccountAddress     string  `yaml:"subaccount_address"`
	PrivateKey            string  `yaml:"private_key"`
}

func newInitTemplateYAMLDoc() initTemplateYAMLDoc {
	return initTemplateYAMLDoc{
		Network:               "testnet",
		MarketName:            "BTC/USD",
		Spread:                0.001,
		OrderSize:             0.001,
		MaxInventory:          0.005,
		RefreshInterval:       20,
		RefreshIntervalJitter: 0,
		BearerToken:           "",
		SubaccountAddress:     "",
		PrivateKey:            "",
	}
}

// InitConfigYAMLBytes returns a minimal YAML document (comments + marshalled initTemplateYAMLDoc)
// that decodeFileConfig accepts with KnownFields(true).
func InitConfigYAMLBytes() ([]byte, error) {
	doc := newInitTemplateYAMLDoc()
	body, err := yaml.Marshal(&doc)
	if err != nil {
		return nil, fmt.Errorf("marshal init config: %w", err)
	}
	header := strings.Join([]string{
		"# Decibel market-maker — minimal YAML template (-init-config).",
		"# Fill bearer_token, subaccount_address, private_key (here or via env); left empty the bot will refuse to start.",
		"# Other parameters use env / defaults from README unless you add keys (same names as FileConfig).",
		"# Priority: CLI > this file > environment variables > network preset defaults.",
		"",
	}, "\n")
	return append([]byte(header), body...), nil
}

// WriteInitConfigYAML writes InitConfigYAMLBytes to path. It refuses to overwrite an existing file.
func WriteInitConfigYAML(path string) error {
	if strings.TrimSpace(path) == "" {
		return fmt.Errorf("init-config: output path is empty")
	}
	if _, err := os.Stat(path); err == nil {
		return fmt.Errorf("init-config: %s already exists (delete it first or choose another path)", path)
	} else if !os.IsNotExist(err) {
		return fmt.Errorf("init-config: stat %s: %w", path, err)
	}
	data, err := InitConfigYAMLBytes()
	if err != nil {
		return err
	}
	if err := os.WriteFile(path, data, 0o600); err != nil {
		return fmt.Errorf("init-config: write %s: %w", path, err)
	}
	return nil
}
