package logging

import (
	"bytes"
	"log/slog"
	"strings"
	"testing"

	"decibel-mm-bot/config"
)

func TestSetupTeeFileBranchNoANSIInFile(t *testing.T) {
	resetDefaultSlog(t)
	var stderr, file bytes.Buffer
	cfg := &config.Config{LogLevel: "info", LogFormat: "text"}
	Setup(&stderr, cfg, &file)
	slog.Info("hello", "k", "v")
	if !strings.Contains(stderr.String(), "hello") {
		t.Fatalf("stderr: %q", stderr.String())
	}
	if !strings.Contains(file.String(), "hello") {
		t.Fatalf("file: %q", file.String())
	}
	if strings.Contains(file.String(), "\033[") {
		t.Fatalf("file writer must not receive ANSI escapes, got %q", file.String())
	}
}

func TestSetupTeeJSONBothBranches(t *testing.T) {
	resetDefaultSlog(t)
	var stderr, file bytes.Buffer
	cfg := &config.Config{LogLevel: "info", LogFormat: "json"}
	Setup(&stderr, cfg, &file)
	slog.Info("hello", "k", "v")
	if stderr.Len() == 0 || file.Len() == 0 {
		t.Fatalf("expected both sinks: stderr=%d file=%d", stderr.Len(), file.Len())
	}
}
