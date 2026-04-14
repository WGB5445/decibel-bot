package i18n

import "testing"

func TestParseLocale(t *testing.T) {
	if ParseLocale("") != LocaleZH {
		t.Fatal("empty -> zh")
	}
	if ParseLocale("EN") != LocaleEN {
		t.Fatal("EN -> en")
	}
	if ParseLocale("english") != LocaleEN {
		t.Fatal("english -> en")
	}
	if ParseLocale("zh").String() != "zh" {
		t.Fatal("zh tag")
	}
	if ParseLocale("en").String() != "en" {
		t.Fatal("en tag")
	}
}
