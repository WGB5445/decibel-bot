package aptos

import (
	"strings"
	"testing"
)

func TestCreatePerpEngineGlobalAddress_testnetPackage(t *testing.T) {
	pkg := "0xe7da2794b1d8af76532ed95f38bfdf1136abfd8ea3a240189971988a83101b7f"
	addr, err := CreatePerpEngineGlobalAddress(pkg)
	if err != nil {
		t.Fatal(err)
	}
	if !strings.HasPrefix(addr, "0x") || len(addr) < 66 {
		t.Fatalf("unexpected address: %q", addr)
	}
}

// Golden: named-object(creator=testnet package, seed="GlobalPerpEngine") — stable across SDK refactor.
func TestCreatePerpEngineGlobalAddress_golden(t *testing.T) {
	pkg := "0xe7da2794b1d8af76532ed95f38bfdf1136abfd8ea3a240189971988a83101b7f"
	const want = "0xc72692f7305357331cf13e0b66996b38731f1ec0e4e1437ab65a007f06d037a0"
	got, err := CreatePerpEngineGlobalAddress(pkg)
	if err != nil {
		t.Fatal(err)
	}
	if got != want {
		t.Fatalf("CreatePerpEngineGlobalAddress(%q) = %q, want %q", pkg, got, want)
	}
}
