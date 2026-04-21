package decibel

import (
	"context"
	"encoding/hex"
	"fmt"
	"strings"
	"time"

	"github.com/aptos-labs/aptos-go-sdk"
	aptapi "github.com/aptos-labs/aptos-go-sdk/api"
	"github.com/aptos-labs/aptos-go-sdk/crypto"
)

const maxGasAmount = 200_000

// TxResult holds the outcome of an entry-function submission.
type TxResult struct {
	Hash     string
	Success  bool
	VMStatus string
	Events   []*aptapi.Event
}

// VMStatusWaitPending is set on TxResult when the tx hash is known but confirmation polling failed.
const VMStatusWaitPending = "wait_pending"

// CancelSucceeded returns true when the transaction should be counted as a
// successful cancel (either the tx succeeded, or the order was already gone).
func (r *TxResult) CancelSucceeded() bool {
	return r.Success ||
		strings.Contains(r.VMStatus, "ERESOURCE_DOES_NOT_EXIST") ||
		strings.Contains(r.VMStatus, "EORDER_NOT_FOUND")
}

// NodeClient wraps aptos.Client for a single Aptos fullnode.
type NodeClient struct {
	sdk *aptos.Client
}

// ChainIDForNetwork returns the Aptos chain id for known profiles (testnet=2, mainnet=1).
func ChainIDForNetwork(network string) uint8 {
	switch strings.ToLower(strings.TrimSpace(network)) {
	case "mainnet":
		return 1
	case "testnet":
		return 2
	default:
		return 0
	}
}

// NewNodeClient connects to fullnodeURL. If apiKey is non-empty, it is sent as
// Authorization: Bearer.
func NewNodeClient(fullnodeURL, apiKey string, chainID uint8) (*NodeClient, error) {
	node := strings.TrimRight(strings.TrimSpace(fullnodeURL), "/")
	cfg := aptos.NetworkConfig{
		NodeUrl: node,
		ChainId: chainID,
	}
	sdkClient, err := aptos.NewClient(cfg)
	if err != nil {
		return nil, fmt.Errorf("aptos NewClient: %w", err)
	}
	if apiKey != "" {
		sdkClient.SetHeader("Authorization", "Bearer "+apiKey)
	}
	return &NodeClient{sdk: sdkClient}, nil
}

// ParseAccount builds an sdk account from a private key string (hex with optional 0x,
// or AIP-80 ed25519-priv-...).
func ParseAccount(privKeyStr string) (*aptos.Account, error) {
	s := strings.TrimSpace(privKeyStr)
	if s == "" {
		return nil, fmt.Errorf("private key is empty")
	}

	var hexBody string
	if strings.Contains(s, "-priv-") {
		parts := strings.SplitN(s, "-priv-", 2)
		if len(parts) != 2 {
			return nil, fmt.Errorf("invalid AIP-80 private key string")
		}
		algo := strings.ToLower(strings.TrimSpace(parts[0]))
		if strings.HasPrefix(algo, "secp256k1") {
			return nil, fmt.Errorf("secp256k1 private keys are not supported; use ed25519")
		}
		hexBody = strings.TrimPrefix(strings.TrimSpace(parts[1]), "0x")
	} else {
		hexBody = strings.TrimPrefix(s, "0x")
	}

	b, err := hex.DecodeString(hexBody)
	if err != nil {
		return nil, fmt.Errorf("private key hex: %w", err)
	}
	if len(b) < 32 {
		return nil, fmt.Errorf("private key must be at least 32 bytes, got %d", len(b))
	}
	if len(b) > 32 {
		b = b[:32]
	}

	var key crypto.Ed25519PrivateKey
	if err := key.FromBytes(b); err != nil {
		return nil, err
	}
	return aptos.NewAccountFromSigner(&key)
}

// SubmitEntryFunction builds, signs, submits, and waits for an entry function.
func (n *NodeClient) SubmitEntryFunction(
	ctx context.Context,
	signer aptos.TransactionSigner,
	function string,
	typeArgs []string,
	args []any,
) (*TxResult, error) {
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	if typeArgs == nil {
		typeArgs = []string{}
	}

	parts := strings.Split(function, "::")
	if len(parts) != 3 {
		return nil, fmt.Errorf("invalid function spec %q (want addr::module::func)", function)
	}
	var moduleAddr aptos.AccountAddress
	if err := moduleAddr.ParseStringRelaxed(parts[0]); err != nil {
		return nil, fmt.Errorf("parse module address: %w", err)
	}

	typeArgsAny := make([]any, len(typeArgs))
	for i, t := range typeArgs {
		typeArgsAny[i] = t
	}

	entry, err := n.sdk.EntryFunctionWithArgs(moduleAddr, parts[1], parts[2], typeArgsAny, normalizeEntryArgs(args))
	if err != nil {
		return nil, fmt.Errorf("entry function from ABI: %w", err)
	}

	payload := aptos.TransactionPayload{Payload: entry}
	rawTxn, err := n.sdk.BuildTransaction(
		signer.AccountAddress(),
		payload,
		aptos.MaxGasAmount(maxGasAmount),
		aptos.GasUnitPrice(100),
	)
	if err != nil {
		return nil, fmt.Errorf("build transaction: %w", err)
	}

	signedTxn, err := rawTxn.SignedTransaction(signer)
	if err != nil {
		return nil, fmt.Errorf("sign transaction: %w", err)
	}

	submitRes, err := n.sdk.SubmitTransaction(signedTxn)
	if err != nil {
		return nil, fmt.Errorf("submit transaction: %w", err)
	}

	waitTimeout := 60 * time.Second
	if d, ok := ctx.Deadline(); ok {
		if rem := time.Until(d); rem > 0 && rem < waitTimeout {
			waitTimeout = rem
		}
	}
	waitOpts := []any{aptos.PollTimeout(waitTimeout)}

	hash := string(submitRes.Hash)
	tx, err := n.sdk.WaitForTransaction(hash, waitOpts...)
	if err != nil {
		return &TxResult{
			Hash:     hash,
			Success:  false,
			VMStatus: VMStatusWaitPending,
			Events:   nil,
		}, fmt.Errorf("wait for transaction: %w", err)
	}

	vm := tx.VmStatus
	if vm == "" {
		vm = "unknown"
	}
	return &TxResult{
		Hash:     hash,
		Success:  tx.Success,
		VMStatus: vm,
		Events:   tx.Events,
	}, nil
}

func normalizeEntryArgs(args []any) []any {
	out := make([]any, len(args))
	copy(out, args)
	for i, a := range out {
		if s, ok := a.(string); ok && strings.HasPrefix(s, "0x") {
			var addr aptos.AccountAddress
			if err := addr.ParseStringRelaxed(s); err == nil {
				out[i] = addr
			}
		}
	}
	return out
}

// WaitForTransaction waits for a transaction to be confirmed.
func (n *NodeClient) WaitForTransaction(ctx context.Context, hash string) (*aptapi.UserTransaction, error) {
	waitTimeout := 60 * time.Second
	if d, ok := ctx.Deadline(); ok {
		if rem := time.Until(d); rem > 0 && rem < waitTimeout {
			waitTimeout = rem
		}
	}
	waitOpts := []any{aptos.PollTimeout(waitTimeout)}
	return n.sdk.WaitForTransaction(hash, waitOpts...)
}

// APTBalance returns the APT balance (in APT, not octas) for the given address string.
func (n *NodeClient) APTBalance(ctx context.Context, addrStr string) (float64, error) {
	var addr aptos.AccountAddress
	if err := addr.ParseStringRelaxed(addrStr); err != nil {
		return 0, fmt.Errorf("parse address: %w", err)
	}
	octas, err := n.sdk.AccountAPTBalance(addr)
	if err != nil {
		return 0, fmt.Errorf("apt balance: %w", err)
	}
	return float64(octas) / 1e8, nil
}
