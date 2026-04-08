package aptos

import "testing"

func TestCreatePerpEngineGlobalAddress(t *testing.T) {
	// Use the testnet package address from the default network profile.
	packageAddr := "0xe7da2794b1d8af76532ed95f38bfdf1136abfd8ea3a240189971988a83101b7f"
	addr, err := CreatePerpEngineGlobalAddress(packageAddr)
	if err != nil {
		t.Fatalf("derive failed: %v", err)
	}
	if len(addr) != 66 || addr[:2] != "0x" {
		t.Fatalf("unexpected address format: %s", addr)
	}
	t.Logf("derived address: %s", addr)
}
