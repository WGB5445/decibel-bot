package aptos

import (
	"fmt"
	"strings"

	aptossdk "github.com/aptos-labs/aptos-go-sdk"
)

// CreatePerpEngineGlobalAddress derives the GlobalPerpEngine object address for the
// given Move package address, matching Python:
//
//	create_object_address(AccountAddress.from_str(package), b"GlobalPerpEngine")
func CreatePerpEngineGlobalAddress(packageAddr string) (string, error) {
	return CreateNamedObjectAddress(packageAddr, []byte("GlobalPerpEngine"))
}

// CreateNamedObjectAddress derives a named object address from a creator address and
// seed using Aptos named-object derivation (same as aptos_sdk create_object_address):
// SHA3-256(creator_32 || seed || NamedObjectScheme).
func CreateNamedObjectAddress(packageAddr string, seed []byte) (string, error) {
	var creator aptossdk.AccountAddress
	if err := creator.ParseStringRelaxed(strings.TrimSpace(packageAddr)); err != nil {
		return "", fmt.Errorf("invalid package address: %w", err)
	}
	derived := creator.NamedObjectAddress(seed)
	return derived.String(), nil
}
