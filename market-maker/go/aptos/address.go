package aptos

import (
	"encoding/hex"
	"fmt"
	"strings"

	"golang.org/x/crypto/sha3"
)

// CreatePerpEngineGlobalAddress derives the GlobalPerpEngine object address
// for the given Move package address using the named-object derivation.
func CreatePerpEngineGlobalAddress(packageAddr string) (string, error) {
	return CreateNamedObjectAddress(packageAddr, []byte("GlobalPerpEngine"))
}

// CreateNamedObjectAddress derives a named object address from a creator
// account (package) address and an arbitrary byte seed using the same
// scheme used by other Aptos SDKs (sha3_256(creator || seed || scheme)).
// The returned string is 0x-prefixed.
func CreateNamedObjectAddress(packageAddr string, seed []byte) (string, error) {
	s := strings.TrimPrefix(packageAddr, "0x")
	b, err := hex.DecodeString(s)
	if err != nil {
		return "", fmt.Errorf("invalid package address: %w", err)
	}
	if len(b) > 32 {
		return "", fmt.Errorf("package address too long: %d", len(b))
	}

	// Left-pad to 32 bytes to produce canonical account bytes.
	addr := make([]byte, 32)
	copy(addr[32-len(b):], b)

	h := sha3.New256()
	h.Write(addr)
	h.Write(seed)
	h.Write([]byte{0xfe}) // NamedObject scheme byte (254)
	sum := h.Sum(nil)
	return "0x" + hex.EncodeToString(sum), nil
}
