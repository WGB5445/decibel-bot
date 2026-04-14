package logging

import (
	"fmt"
	"path/filepath"
	"strings"

	"decibel-mm-bot/api"
)

// TeeAutoBasename returns a safe log filename: last 8 hex chars of normalized subaccount + "_" + market + ".log".
func TeeAutoBasename(subaccountAddr, marketName string) string {
	n := api.NormalizeAddr(subaccountAddr)
	if n == "" || n == "0" {
		n = "account"
	}
	if len(n) > 8 {
		n = n[len(n)-8:]
	}
	m := sanitizeFilenameComponent(marketName)
	if m == "" {
		m = "market"
	}
	return fmt.Sprintf("%s_%s.log", n, m)
}

// TeeAutoPath joins dir with [TeeAutoBasename](subaccountAddr, marketName). dir may be ".".
func TeeAutoPath(dir, subaccountAddr, marketName string) (string, error) {
	d := strings.TrimSpace(dir)
	if d == "" {
		d = "."
	}
	base := TeeAutoBasename(subaccountAddr, marketName)
	return filepath.Join(d, base), nil
}

func sanitizeFilenameComponent(s string) string {
	var b strings.Builder
	for _, r := range strings.TrimSpace(s) {
		switch r {
		case '/', '\\', ':', '*', '?', '"', '<', '>', '|', ' ':
			b.WriteByte('-')
		default:
			if r >= 0x20 && r < 0x7f {
				b.WriteRune(r)
			} else {
				b.WriteByte('-')
			}
		}
	}
	out := strings.Trim(b.String(), "-.")
	if len(out) > 120 {
		out = out[:120]
	}
	// Collapse repeated dashes lightly
	for strings.Contains(out, "--") {
		out = strings.ReplaceAll(out, "--", "-")
	}
	if out == "" {
		return ""
	}
	return out
}
