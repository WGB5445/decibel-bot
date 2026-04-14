package i18n

import "strings"

// Locale is a supported UI language tag for bot-facing copy (e.g. Telegram).
type Locale string

const (
	LocaleZH Locale = "zh"
	LocaleEN Locale = "en"
)

// ParseLocale maps env/CLI/config values to a supported locale. Unknown values default to Chinese.
func ParseLocale(s string) Locale {
	switch strings.ToLower(strings.TrimSpace(s)) {
	case "en", "english":
		return LocaleEN
	default:
		return LocaleZH
	}
}

// String returns the stable tag stored in config ("zh" or "en").
func (l Locale) String() string {
	if l == LocaleEN {
		return "en"
	}
	return "zh"
}
