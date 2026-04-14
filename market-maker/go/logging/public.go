package logging

import (
	"context"
	"log/slog"

	"decibel-mm-bot/logctx"
)

// SuccessCtx logs at INFO with green styling when colors are enabled (chain / TG success paths).
// When ctx carries a MM cycle via [logctx.WithCycle], "cycle" is prepended to attributes.
func SuccessCtx(ctx context.Context, msg string, args ...any) {
	if ctx == nil {
		ctx = context.Background()
	}
	prefix := append([]any{attrColor, valSuccess}, logctx.AppendAttrs(ctx)...)
	slog.Log(ctx, slog.LevelInfo, msg, append(prefix, args...)...)
}

// Success logs like [SuccessCtx] with no request context (no cycle from context).
func Success(msg string, args ...any) {
	SuccessCtx(context.Background(), msg, args...)
}

// Cycle logs at INFO with blank lines before/after cycle records (see colorHandler),
// regardless of whether ANSI colors are active.
func Cycle(msg string, args ...any) {
	slog.Log(context.Background(), slog.LevelInfo, msg, append([]any{attrColor, valCycle}, args...)...)
}
