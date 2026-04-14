package logging

import (
	"context"
	"log/slog"
)

// Success logs at INFO with green styling when colors are enabled (chain / TG success paths).
func Success(msg string, args ...any) {
	slog.Log(context.Background(), slog.LevelInfo, msg, append([]any{attrColor, valSuccess}, args...)...)
}

// Cycle logs at INFO with default foreground and blank lines before/after when colors are enabled.
func Cycle(msg string, args ...any) {
	slog.Log(context.Background(), slog.LevelInfo, msg, append([]any{attrColor, valCycle}, args...)...)
}
