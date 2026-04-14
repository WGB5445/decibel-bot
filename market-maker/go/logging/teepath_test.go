package logging

import (
	"path/filepath"
	"strings"
	"testing"
)

func TestTeeAutoBasename(t *testing.T) {
	got := TeeAutoBasename("0x0000000000000000000000000000000000000000000000000000000000a1b2c3d4", "BTC/USD")
	if !strings.HasSuffix(got, ".log") {
		t.Fatalf("expected .log suffix, got %q", got)
	}
	if !strings.Contains(got, "a1b2c3d4") {
		t.Fatalf("expected last 8 addr chars, got %q", got)
	}
	if !strings.Contains(strings.ToUpper(got), "BTC") {
		t.Fatalf("expected market fragment, got %q", got)
	}
}

func TestTeeAutoBasename_shortAddr(t *testing.T) {
	got := TeeAutoBasename("0xabc", "ETH-USD")
	if got == "" || !strings.HasSuffix(got, ".log") {
		t.Fatalf("unexpected %q", got)
	}
}

func TestTeeAutoPath_joinsDir(t *testing.T) {
	p, err := TeeAutoPath("./logs", "0x0000000000000000000000000000000000000000000000000000000000a1b2c3d4", "M/P")
	if err != nil {
		t.Fatal(err)
	}
	dir := filepath.Dir(p)
	if !strings.HasSuffix(dir, "logs") {
		t.Fatalf("expected logs dir, got dir=%q full=%q", dir, p)
	}
}

func TestSanitizeFilenameComponent(t *testing.T) {
	if g := sanitizeFilenameComponent("BTC/USD"); g != "BTC-USD" {
		t.Fatalf("got %q", g)
	}
	if g := sanitizeFilenameComponent(""); g != "" {
		t.Fatalf("empty in empty out, got %q", g)
	}
}
