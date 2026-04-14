package logging

import (
	"context"
	"log/slog"
)

// teeHandler forwards each record to two handlers (stderr + file). Uses [slog.Record.Clone]
// so each child can consume attrs independently.
type teeHandler struct {
	a, b slog.Handler
}

func (t *teeHandler) Enabled(ctx context.Context, level slog.Level) bool {
	return t.a.Enabled(ctx, level)
}

func (t *teeHandler) Handle(ctx context.Context, r slog.Record) error {
	r1 := r.Clone()
	r2 := r.Clone()
	if err := t.a.Handle(ctx, r1); err != nil {
		return err
	}
	return t.b.Handle(ctx, r2)
}

func (t *teeHandler) WithAttrs(attrs []slog.Attr) slog.Handler {
	return &teeHandler{a: t.a.WithAttrs(attrs), b: t.b.WithAttrs(attrs)}
}

func (t *teeHandler) WithGroup(name string) slog.Handler {
	return &teeHandler{a: t.a.WithGroup(name), b: t.b.WithGroup(name)}
}
