// Package logctx stores request-scoped values (e.g. market-making cycle id) in
// context.Context for structured logging in exchange code without widening
// the Exchange interface.
package logctx

import "context"

type cycleKey struct{}

// WithCycle returns ctx that carries cycle for this MM round. Pass the same
// ctx into exchange calls so Decibel logs can include "cycle".
func WithCycle(ctx context.Context, cycle uint64) context.Context {
	return context.WithValue(ctx, cycleKey{}, cycle)
}

// Cycle returns the cycle id if ctx was produced via WithCycle.
func Cycle(ctx context.Context) (uint64, bool) {
	v := ctx.Value(cycleKey{})
	if v == nil {
		return 0, false
	}
	c, ok := v.(uint64)
	return c, ok
}

// AppendAttrs prepends "cycle", id when present; otherwise returns attrs unchanged.
func AppendAttrs(ctx context.Context, attrs ...any) []any {
	if c, ok := Cycle(ctx); ok {
		out := make([]any, 0, 2+len(attrs))
		out = append(out, "cycle", c)
		out = append(out, attrs...)
		return out
	}
	return attrs
}
