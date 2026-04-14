package logging

import (
	"io"
	"log/slog"
	"os"
	"strings"

	"golang.org/x/term"

	"decibel-mm-bot/config"
)

// Setup installs the default slog logger. Pass cfg after [config.Load] so CLI/env
// for log level and format apply; pass nil to read LOG_LEVEL / LOG_FORMAT from
// the environment only (e.g. before config load).
//
// When tee is non-nil, each log line is written to w (stderr, with TTY colors when
// enabled) and to tee (plain text or JSON matching cfg.LogFormat, never ANSI).
func Setup(w io.Writer, cfg *config.Config, tee io.Writer) {
	level := slog.LevelInfo
	format := "text"
	if cfg != nil {
		level = ParseLogLevel(cfg.LogLevel)
		format = strings.ToLower(strings.TrimSpace(cfg.LogFormat))
	} else {
		level = ParseLogLevel(os.Getenv("LOG_LEVEL"))
		format = strings.ToLower(strings.TrimSpace(os.Getenv("LOG_FORMAT")))
	}
	if format == "" {
		format = "text"
	}
	if format != "json" {
		format = "text"
	}

	opts := &slog.HandlerOptions{Level: level}

	if tee != nil {
		if format == "json" {
			h := &teeHandler{
				a: slog.NewJSONHandler(w, opts),
				b: slog.NewJSONHandler(tee, opts),
			}
			slog.SetDefault(slog.New(h))
			return
		}

		useColor := false
		if os.Getenv("NO_COLOR") == "" {
			if f, ok := w.(*os.File); ok {
				useColor = term.IsTerminal(int(f.Fd()))
			}
		}
		shOut := &sharedWriter{w: w, useColor: useColor, minLevel: level}
		shFile := &sharedWriter{w: tee, useColor: false, minLevel: level}
		h := &teeHandler{
			a: &colorHandler{sh: shOut},
			b: &colorHandler{sh: shFile},
		}
		slog.SetDefault(slog.New(h))
		return
	}

	if format == "json" {
		h := slog.NewJSONHandler(w, opts)
		slog.SetDefault(slog.New(h))
		return
	}

	useColor := false
	if os.Getenv("NO_COLOR") == "" {
		if f, ok := w.(*os.File); ok {
			useColor = term.IsTerminal(int(f.Fd()))
		}
	}
	sh := &sharedWriter{
		w:        w,
		useColor: useColor,
		minLevel: level,
	}
	slog.SetDefault(slog.New(&colorHandler{sh: sh}))
}

// ParseLogLevel maps config strings to slog levels; unknown values default to info.
func ParseLogLevel(s string) slog.Level {
	switch strings.ToLower(strings.TrimSpace(s)) {
	case "debug":
		return slog.LevelDebug
	case "warn", "warning":
		return slog.LevelWarn
	case "error":
		return slog.LevelError
	default:
		return slog.LevelInfo
	}
}
