package logging

import (
	"bytes"
	"context"
	"log/slog"
	"strings"
	"testing"
	"time"
)

func TestColorHandler_noAnsiWhenDisabled(t *testing.T) {
	var buf bytes.Buffer
	sh := &sharedWriter{w: &buf, useColor: false, minLevel: slog.LevelInfo}
	h := &colorHandler{sh: sh}
	rec := slog.NewRecord(time.Date(2026, 4, 14, 12, 0, 0, 0, time.UTC), slog.LevelWarn, "hello", 0)
	rec.Add("k", "v")
	if err := h.Handle(context.Background(), rec); err != nil {
		t.Fatal(err)
	}
	out := buf.String()
	if strings.Contains(out, "\033[") {
		t.Fatalf("expected no ANSI escapes when useColor=false, got %q", out)
	}
	if !strings.Contains(out, "WARN") || !strings.Contains(out, "hello") {
		t.Fatalf("unexpected output: %q", out)
	}
}

func TestColorHandler_successGreenWhenEnabled(t *testing.T) {
	var buf bytes.Buffer
	sh := &sharedWriter{w: &buf, useColor: true, minLevel: slog.LevelInfo}
	h := &colorHandler{sh: sh}
	rec := slog.NewRecord(time.Date(2026, 4, 14, 12, 0, 0, 0, time.UTC), slog.LevelInfo, "placed", 0)
	rec.Add(attrColor, valSuccess)
	rec.Add("tx_hash", "0xabc")
	if err := h.Handle(context.Background(), rec); err != nil {
		t.Fatal(err)
	}
	out := buf.String()
	if !strings.Contains(out, ansiGreen) {
		t.Fatalf("expected green prefix, got %q", out)
	}
	if strings.Contains(out, attrColor) {
		t.Fatalf("_color attr should be stripped, got %q", out)
	}
}

func TestColorHandler_cycleLeadingNewlines(t *testing.T) {
	var buf bytes.Buffer
	sh := &sharedWriter{w: &buf, useColor: false, minLevel: slog.LevelInfo}
	h := &colorHandler{sh: sh}
	rec := slog.NewRecord(time.Date(2026, 4, 14, 12, 0, 0, 0, time.UTC), slog.LevelInfo, "cycle start", 0)
	rec.Add(attrColor, valCycle)
	if err := h.Handle(context.Background(), rec); err != nil {
		t.Fatal(err)
	}
	out := buf.String()
	if !strings.HasPrefix(out, "\n\n") {
		t.Fatalf("expected leading blank line for cycle, got %q", out)
	}
}
