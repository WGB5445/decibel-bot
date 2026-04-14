package logging

import (
	"context"
	"fmt"
	"io"
	"slices"
	"strings"
	"sync"
	"time"

	"log/slog"
)

// Internal attribute keys — stripped before printing; never user-facing.
const (
	attrColor  = "_color"
	valSuccess = "success"
	valCycle   = "cycle"
)

const (
	ansiReset  = "\033[0m"
	ansiRed    = "\033[31m"
	ansiGreen  = "\033[32m"
	ansiYellow = "\033[33m"
)

// sharedWriter holds the output stream and mutex for all clones from WithAttrs/WithGroup.
type sharedWriter struct {
	mu       sync.Mutex
	w        io.Writer
	useColor bool
	minLevel slog.Level
}

// colorHandler writes one line per record (plus optional leading/trailing newlines for cycle).
type colorHandler struct {
	sh     *sharedWriter
	attrs  []slog.Attr
	groups string // dot-separated prefix for keys (slog.WithGroup)
}

func (h *colorHandler) Enabled(_ context.Context, level slog.Level) bool {
	return level >= h.sh.minLevel
}

func (h *colorHandler) WithAttrs(attrs []slog.Attr) slog.Handler {
	return &colorHandler{
		sh:     h.sh,
		attrs:  slices.Concat(h.attrs, attrs),
		groups: h.groups,
	}
}

func (h *colorHandler) WithGroup(name string) slog.Handler {
	g := name
	if h.groups != "" {
		g = h.groups + "." + name
	}
	return &colorHandler{
		sh:     h.sh,
		attrs:  h.attrs,
		groups: g,
	}
}

func (h *colorHandler) Handle(_ context.Context, r slog.Record) error {
	var fromRecord []slog.Attr
	r.Attrs(func(a slog.Attr) bool {
		fromRecord = append(fromRecord, a)
		return true
	})
	all := slices.Concat(h.attrs, fromRecord)

	kind := ""
	filtered := make([]slog.Attr, 0, len(all))
	for _, a := range all {
		if a.Key == attrColor {
			kind = a.Value.String()
			continue
		}
		filtered = append(filtered, a)
	}

	var b strings.Builder
	if kind == valCycle {
		b.WriteString("\n\n")
	}

	if h.sh.useColor {
		b.WriteString(linePrefix(r.Level, kind))
	}

	ts := r.Time
	if ts.IsZero() {
		ts = time.Now()
	}
	b.WriteString(ts.Format("2006/01/02 15:04:05"))
	b.WriteByte(' ')
	b.WriteString(r.Level.String())
	b.WriteByte(' ')
	b.WriteString(r.Message)

	for _, a := range filtered {
		b.WriteByte(' ')
		b.WriteString(formatAttr(h.groups, a))
	}

	if h.sh.useColor {
		b.WriteString(ansiReset)
	}
	if kind == valCycle {
		b.WriteByte('\n')
	}
	b.WriteByte('\n')

	h.sh.mu.Lock()
	_, err := io.WriteString(h.sh.w, b.String())
	h.sh.mu.Unlock()
	return err
}

func linePrefix(level slog.Level, kind string) string {
	switch {
	case level >= slog.LevelError:
		return ansiRed
	case level >= slog.LevelWarn:
		return ansiYellow
	case kind == valSuccess:
		return ansiGreen
	default:
		return ansiReset
	}
}

func formatAttr(group string, a slog.Attr) string {
	key := a.Key
	if group != "" {
		key = group + "." + key
	}
	var buf strings.Builder
	buf.WriteString(key)
	buf.WriteByte('=')
	switch a.Value.Kind() {
	case slog.KindString:
		s := a.Value.String()
		if strings.ContainsAny(s, " \t\n\"=") {
			fmt.Fprintf(&buf, "%q", s)
		} else {
			buf.WriteString(s)
		}
	case slog.KindInt64:
		fmt.Fprintf(&buf, "%d", a.Value.Int64())
	case slog.KindUint64:
		fmt.Fprintf(&buf, "%d", a.Value.Uint64())
	case slog.KindFloat64:
		fmt.Fprintf(&buf, "%g", a.Value.Float64())
	case slog.KindBool:
		fmt.Fprintf(&buf, "%t", a.Value.Bool())
	case slog.KindDuration:
		buf.WriteString(a.Value.Duration().String())
	case slog.KindTime:
		buf.WriteString(a.Value.Time().Format(time.RFC3339Nano))
	case slog.KindAny:
		if err, ok := a.Value.Any().(error); ok {
			fmt.Fprintf(&buf, "%q", err.Error())
		} else {
			fmt.Fprintf(&buf, "%v", a.Value.Any())
		}
	default:
		buf.WriteString(a.Value.String())
	}
	return buf.String()
}
