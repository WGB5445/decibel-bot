package config

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestParseInitConfigFlag(t *testing.T) {
	t.Parallel()
	cases := []struct {
		args []string
		want string
		ok   bool
	}{
		{[]string{"prog"}, "", false},
		{[]string{"prog", "-init-config"}, InitConfigDefaultPath, true},
		{[]string{"prog", "--init-config"}, InitConfigDefaultPath, true},
		{[]string{"prog", "-init-config="}, InitConfigDefaultPath, true},
		{[]string{"prog", "-init-config=/tmp/x.yaml"}, "/tmp/x.yaml", true},
		{[]string{"prog", "-init-config", "/tmp/y.yaml"}, "/tmp/y.yaml", true},
		{[]string{"prog", "--init-config=/tmp/z.yaml"}, "/tmp/z.yaml", true},
	}
	for _, tc := range cases {
		got, ok := ParseInitConfigFlag(tc.args)
		if ok != tc.ok || got != tc.want {
			t.Fatalf("args=%v got=%q ok=%v want=%q ok=%v", tc.args, got, ok, tc.want, tc.ok)
		}
	}
}

func TestInitConfigYAMLBytes_decodesWithKnownFields(t *testing.T) {
	t.Parallel()
	data, err := InitConfigYAMLBytes()
	if err != nil {
		t.Fatal(err)
	}
	fc, err := decodeFileConfig(data, ".yaml")
	if err != nil {
		t.Fatalf("decode generated init yaml: %v\n%s", err, string(data))
	}
	if fc.Network == nil || *fc.Network != "testnet" {
		t.Fatalf("network: %+v", fc.Network)
	}
	if fc.Spread == nil || *fc.Spread != 0.001 {
		t.Fatalf("spread: %+v", fc.Spread)
	}
	if fc.BearerToken == nil || *fc.BearerToken != "" {
		t.Fatalf("bearer_token should be explicit empty: %+v", fc.BearerToken)
	}
	if fc.PrivateKey == nil || *fc.PrivateKey != "" {
		t.Fatalf("private_key should be explicit empty: %+v", fc.PrivateKey)
	}
}

func TestWriteInitConfigYAML_roundtripLoadWith(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "bot.yaml")

	if err := WriteInitConfigYAML(path); err != nil {
		t.Fatal(err)
	}
	if err := WriteInitConfigYAML(path); err == nil {
		t.Fatal("expected overwrite error")
	}

	t.Setenv("NETWORK", "testnet")
	t.Setenv("BEARER_TOKEN", "t")
	t.Setenv("SUBACCOUNT_ADDRESS", "0x1")
	t.Setenv("PRIVATE_KEY", testPrivateKeyHex)

	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	fc, err := decodeFileConfig(data, ".yaml")
	if err != nil {
		t.Fatal(err)
	}
	if fc.Network == nil || *fc.Network != "testnet" {
		t.Fatalf("network %+v", fc.Network)
	}
	if fc.BearerToken == nil || *fc.BearerToken != "" {
		t.Fatalf("bearer %+v", fc.BearerToken)
	}

	// File explicitly clears credentials → overrides env → validate must fail.
	_, err = LoadWith(LoadOptions{
		Args:       []string{"prog"},
		ConfigData: data,
		ConfigExt:  ".yaml",
	})
	if err == nil {
		t.Fatal("expected validate error: init template leaves secrets empty")
	}
	if !strings.Contains(err.Error(), "BEARER_TOKEN") && !strings.Contains(err.Error(), "missing required") {
		t.Fatalf("unexpected error: %v", err)
	}
}
