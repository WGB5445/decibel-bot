package logging

import (
	"errors"
	"log/slog"
	"os"
	"strings"
	"testing"
	"time"

	"decibel-mm-bot/config"
)

func resetDefaultSlog(t *testing.T) {
	t.Cleanup(func() {
		slog.SetDefault(slog.New(slog.NewTextHandler(os.Stderr, &slog.HandlerOptions{Level: slog.LevelInfo})))
	})
}

func TestTeeFileWriterSyncLinesReachFileBeforeProcessExit(t *testing.T) {
	resetDefaultSlog(t)
	f, err := os.CreateTemp(t.TempDir(), "teelog-*.log")
	if err != nil {
		t.Fatal(err)
	}
	path := f.Name()
	_ = f.Close()

	out, err := os.OpenFile(path, os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o644)
	if err != nil {
		t.Fatal(err)
	}
	sink := TeeFileWriter(out, 0, false)
	cfg := &config.Config{LogLevel: "info", LogFormat: "text"}
	Setup(os.Stderr, cfg, sink)

	slog.Info("alpha", "k", "v")
	slog.Info("beta")

	if err := sink.Close(); err != nil {
		t.Fatalf("Close: %v", err)
	}

	body, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	s := string(body)
	if !strings.Contains(s, "alpha") || !strings.Contains(s, "beta") {
		t.Fatalf("expected both log lines in file, got:\n%s", s)
	}
}

func TestTeeFileWriterAsyncCloseDrainsAllLines(t *testing.T) {
	resetDefaultSlog(t)
	f, err := os.CreateTemp(t.TempDir(), "teelog-async-*.log")
	if err != nil {
		t.Fatal(err)
	}
	path := f.Name()
	_ = f.Close()

	out, err := os.OpenFile(path, os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o644)
	if err != nil {
		t.Fatal(err)
	}
	sink := TeeFileWriter(out, 50, false)
	cfg := &config.Config{LogLevel: "info", LogFormat: "text"}
	Setup(os.Stderr, cfg, sink)

	for i := 0; i < 20; i++ {
		slog.Info("async_line", "i", i)
	}
	if err := sink.Close(); err != nil {
		t.Fatalf("Close: %v", err)
	}

	body, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	count := strings.Count(string(body), "async_line")
	if count != 20 {
		t.Fatalf("want 20 async_line records, got %d in:\n%s", count, string(body))
	}
}

func TestTeeFileWriterWriteAfterClose(t *testing.T) {
	resetDefaultSlog(t)
	f, err := os.CreateTemp(t.TempDir(), "teelog-close-*.log")
	if err != nil {
		t.Fatal(err)
	}
	_ = f.Close()
	out, err := os.OpenFile(f.Name(), os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o644)
	if err != nil {
		t.Fatal(err)
	}
	sink := TeeFileWriter(out, 0, false)
	cfg := &config.Config{LogLevel: "info", LogFormat: "text"}
	Setup(os.Stderr, cfg, sink)
	_ = sink.Close()

	_, err = sink.Write([]byte("late\n"))
	if !errors.Is(err, ErrTeeClosed) {
		t.Fatalf("Write after Close: want ErrTeeClosed, got %v", err)
	}
}

func TestTeeFileWriterAsyncTickerDoesNotPanicOnSmallInterval(t *testing.T) {
	resetDefaultSlog(t)
	f, err := os.CreateTemp(t.TempDir(), "teelog-tick-*.log")
	if err != nil {
		t.Fatal(err)
	}
	_ = f.Close()
	out, err := os.OpenFile(f.Name(), os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o644)
	if err != nil {
		t.Fatal(err)
	}
	sink := TeeFileWriter(out, 1, false)
	cfg := &config.Config{LogLevel: "info", LogFormat: "text"}
	Setup(os.Stderr, cfg, sink)
	slog.Info("tick_ok")
	time.Sleep(5 * time.Millisecond)
	_ = sink.Close()
}

func TestTeeFileWriterJSONSync(t *testing.T) {
	resetDefaultSlog(t)
	f, err := os.CreateTemp(t.TempDir(), "teelog-json-*.log")
	if err != nil {
		t.Fatal(err)
	}
	path := f.Name()
	_ = f.Close()
	out, err := os.OpenFile(path, os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o644)
	if err != nil {
		t.Fatal(err)
	}
	sink := TeeFileWriter(out, 0, false)
	cfg := &config.Config{LogLevel: "info", LogFormat: "json"}
	Setup(os.Stderr, cfg, sink)
	slog.Info("json_row", "x", 1)
	_ = sink.Close()
	body, _ := os.ReadFile(path)
	if !strings.Contains(string(body), "json_row") {
		t.Fatalf("missing json log: %s", string(body))
	}
}
