// Package aptos handles transaction submission using the aptos-go-sdk v2 only.
package aptos

import (
	"context"
	"encoding/hex"
	"fmt"
	"net/http"
	"strings"
	"time"

	aptsdk "github.com/aptos-labs/aptos-go-sdk/v2"
	sdkacct "github.com/aptos-labs/aptos-go-sdk/v2/account"
)

// TxResult holds the outcome of a committed on-chain transaction.
type TxResult struct {
	Hash     string
	Success  bool
	VMStatus string
}

// CancelSucceeded returns true when the transaction should be counted as a
// successful cancel (either the tx succeeded, or the order was already gone).
func (r *TxResult) CancelSucceeded() bool {
	return r.Success || strings.Contains(r.VMStatus, "ERESOURCE_DOES_NOT_EXIST") || strings.Contains(r.VMStatus, "EORDER_NOT_FOUND")
}

// Client signs and submits Aptos entry-function transactions using the SDK.
type Client struct {
	sdk           aptsdk.Client
	sdkAccount    *sdkacct.Account
	senderAddress string
}

// httpDoerAdapter wraps net/http.Client and injects headers for the SDK.
type httpDoerAdapter struct {
	client  *http.Client
	headers map[string]string
}

func (a *httpDoerAdapter) Do(ctx context.Context, req *http.Request) (*http.Response, error) {
	req = req.WithContext(ctx)
	for k, v := range a.headers {
		req.Header.Set(k, v)
	}
	return a.client.Do(req)
}

// NewClient creates an SDK-only client from a private-key string (AIP-80 or raw hex).
// Returns error if SDK client or account creation fails; there is no legacy HTTP fallback.
func NewClient(fullnodeURL, apiKey, privKeyStr string) (*Client, error) {
	s := strings.TrimSpace(privKeyStr)
	if s == "" {
		return nil, fmt.Errorf("private key is empty")
	}

	if strings.Contains(s, "-priv-") {
		parts := strings.SplitN(s, "-priv-", 2)
		if strings.ToLower(parts[0]) == "secp256k1" {
			return nil, fmt.Errorf("secp256k1 private keys are not supported; provide an ed25519 private key")
		}
		s = parts[1]
	}
	s = strings.TrimPrefix(s, "0x")
	s = strings.TrimSpace(s)

	b, err := hex.DecodeString(s)
	if err != nil {
		return nil, fmt.Errorf("private key is not valid hex: %w", err)
	}
	if len(b) < 32 {
		return nil, fmt.Errorf("private key must be at least 32 bytes, got %d", len(b))
	}
	seed := b[:32]

	adapter := &httpDoerAdapter{client: &http.Client{Timeout: 30 * time.Second}, headers: map[string]string{}}
	if apiKey != "" {
		adapter.headers["Authorization"] = "Bearer " + apiKey
	}
	sdkCfg := aptsdk.NetworkConfig{NodeURL: strings.TrimRight(fullnodeURL, "/")}
	sdkClient, err := aptsdk.NewClient(sdkCfg, aptsdk.WithHTTPClient(adapter))
	if err != nil {
		return nil, fmt.Errorf("aptsdk.NewClient: %w", err)
	}

	acct, err := sdkacct.FromEd25519PrivateKey(seed)
	if err != nil {
		return nil, fmt.Errorf("create sdk account: %w", err)
	}

	c := &Client{sdk: sdkClient, sdkAccount: acct, senderAddress: acct.Address().String()}
	return c, nil
}

// SenderAddress returns the on-chain address derived from the private key.
func (c *Client) SenderAddress() string { return c.senderAddress }

// SubmitEntryFunction builds, signs and submits an entry function via the SDK.
func (c *Client) SubmitEntryFunction(ctx context.Context, function string, typeArgs []string, args []any) (*TxResult, error) {
	if typeArgs == nil {
		typeArgs = []string{}
	}
	if c.sdk == nil || c.sdkAccount == nil {
		return nil, fmt.Errorf("sdk client not initialized")
	}

	parts := strings.Split(function, "::")
	if len(parts) != 3 {
		return nil, fmt.Errorf("invalid function spec: %s", function)
	}
	moduleAddr, err := aptsdk.ParseAddress(parts[0])
	if err != nil {
		return nil, fmt.Errorf("parse module address %s: %w", parts[0], err)
	}

	typeTags := make([]aptsdk.TypeTag, 0, len(typeArgs))
	for _, t := range typeArgs {
		tt, err := aptsdk.ParseTypeTag(t)
		if err != nil {
			return nil, fmt.Errorf("parse type tag %q: %w", t, err)
		}
		typeTags = append(typeTags, *tt)
	}

	convertedArgs := make([]any, len(args))
	for i, a := range args {
		switch v := a.(type) {
		case string:
			if strings.HasPrefix(v, "0x") {
				if addr, err := aptsdk.ParseAddress(v); err == nil {
					convertedArgs[i] = addr
					continue
				}
			}
			convertedArgs[i] = v
		default:
			convertedArgs[i] = v
		}
	}

	payload := &aptsdk.EntryFunctionPayload{
		Module:   aptsdk.ModuleID{Address: moduleAddr, Name: parts[1]},
		Function: parts[2],
		TypeArgs: typeTags,
		Args:     convertedArgs,
	}

	submitRes, err := c.sdk.SignAndSubmitTransaction(ctx, c.sdkAccount, payload, aptsdk.WithMaxGas(20000))
	if err != nil {
		return nil, fmt.Errorf("sdk sign and submit: %w", err)
	}

	tx, err := c.sdk.WaitForTransaction(ctx, submitRes.Hash)
	if err != nil {
		return nil, fmt.Errorf("wait for transaction: %w", err)
	}

	return &TxResult{Hash: submitRes.Hash, Success: tx.Success, VMStatus: tx.VMStatus}, nil
}

// NoneOption returns the Move-ABI encoding for a None optional value.
func NoneOption() map[string][]any { return map[string][]any{"vec": {}} }
