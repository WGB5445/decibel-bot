// Package aptos handles Ed25519 transaction signing and submission to the Aptos/Movement fullnode.
package aptos

import (
	"bytes"
	"context"
	"crypto/ed25519"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"strings"
	"time"

	aptsdk "github.com/aptos-labs/aptos-go-sdk/v2"
	sdkacct "github.com/aptos-labs/aptos-go-sdk/v2/account"

	"golang.org/x/crypto/sha3"
	"golang.org/x/sync/errgroup"
)

// ─────────────────────────────────────────────────────────────────────────────
// Types
// ─────────────────────────────────────────────────────────────────────────────

// TxResult holds the outcome of a committed on-chain transaction.
type TxResult struct {
	Hash     string
	Success  bool
	VMStatus string
}

// CancelSucceeded returns true when the transaction should be counted as a
// successful cancel (either the tx succeeded, or the order was already gone).
func (r *TxResult) CancelSucceeded() bool {
	return r.Success ||
		strings.Contains(r.VMStatus, "ERESOURCE_DOES_NOT_EXIST") ||
		strings.Contains(r.VMStatus, "EORDER_NOT_FOUND")
}

// ─────────────────────────────────────────────────────────────────────────────
// Client
// ─────────────────────────────────────────────────────────────────────────────

// Client signs and submits Aptos entry-function transactions.
type Client struct {
	http          *http.Client
	baseURL       string
	apiKey        string
	privKey       ed25519.PrivateKey
	senderAddress string
	// sdk client (optional). If non-nil, SubmitEntryFunction will prefer using the SDK
	sdk        aptsdk.Client
	sdkAccount *sdkacct.Account
}

// httpDoerAdapter wraps net/http.Client and implements the SDK HTTPDoer
// interface (Do(ctx, req)). It also injects headers (e.g. Authorization).
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

// NewClient creates a Client from a private-key string. The key can be either
// raw hex (optionally prefixed with 0x) or AIP-80 form like "ed25519-priv-...".
// The function prefers creating an SDK account; if SDK client creation fails
// the legacy HTTP fallback is preserved.
func NewClient(fullnodeURL, apiKey, privKeyStr string) (*Client, error) {
	// Validate and extract seed bytes from the provided key string.
	s := strings.TrimSpace(privKeyStr)
	if s == "" {
		return nil, fmt.Errorf("private key is empty")
	}

	algo := ""
	if strings.Contains(s, "-priv-") {
		parts := strings.SplitN(s, "-priv-", 2)
		algo = strings.ToLower(parts[0])
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
	if algo == "secp256k1" {
		return nil, fmt.Errorf("secp256k1 private keys are not supported; provide an ed25519 private key")
	}

	var seed [32]byte
	copy(seed[:], b[:32])

	// Prepare base client (legacy HTTP behaviour kept for fallback).
	c := &Client{
		http:    &http.Client{Timeout: 30 * time.Second},
		baseURL: strings.TrimRight(fullnodeURL, "/"),
		apiKey:  apiKey,
	}

	// Legacy private key used for manual signing fallback.
	privKey := ed25519.NewKeyFromSeed(seed[:])
	c.privKey = privKey
	c.senderAddress = DeriveAddress(privKey)

	// Attempt SDK client creation. Wrap the standard http.Client to match the
	// SDK's HTTPDoer interface (Do(ctx, req)). Also inject the API key header.
	sdkCfg := aptsdk.NetworkConfig{NodeURL: strings.TrimRight(fullnodeURL, "/")}
	adapter := &httpDoerAdapter{client: c.http, headers: map[string]string{}}
	if apiKey != "" {
		adapter.headers["Authorization"] = "Bearer " + apiKey
	}
	sdkClient, err := aptsdk.NewClient(sdkCfg, aptsdk.WithHTTPClient(adapter))
	if err != nil {
		// Warn but do not fail — keep legacy HTTP path as a fallback.
		slog.Warn("aptsdk.NewClient failed, using legacy HTTP client", "err", err)
		return c, nil
	}

	// Build an SDK account from the ed25519 seed using the account helper.
	acct, err := sdkacct.FromEd25519PrivateKey(seed[:])
	if err != nil {
		slog.Warn("failed to create sdk account from seed, using legacy HTTP client", "err", err)
		return c, nil
	}

	c.sdk = sdkClient
	c.sdkAccount = acct
	c.senderAddress = acct.Address().String()
	return c, nil
}

// SenderAddress returns the on-chain address derived from the private key.
func (c *Client) SenderAddress() string {
	return c.senderAddress
}

// ── HTTP helpers ──────────────────────────────────────────────────────────────

func (c *Client) getJSON(ctx context.Context, path string, dst any) error {
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, c.baseURL+path, nil)
	if err != nil {
		return err
	}
	req.Header.Set("Authorization", "Bearer "+c.apiKey)

	resp, err := c.http.Do(req)
	if err != nil {
		return err
	}
	defer resp.Body.Close()

	if resp.StatusCode == http.StatusNotFound {
		return fmt.Errorf("404: %s", path)
	}
	if resp.StatusCode < 200 || resp.StatusCode >= 300 {
		body, _ := io.ReadAll(resp.Body)
		return fmt.Errorf("HTTP %d: %s", resp.StatusCode, string(body))
	}
	return json.NewDecoder(resp.Body).Decode(dst)
}

func (c *Client) postJSON(ctx context.Context, path string, body, dst any) error {
	data, err := json.Marshal(body)
	if err != nil {
		return fmt.Errorf("marshal: %w", err)
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, c.baseURL+path, bytes.NewReader(data))
	if err != nil {
		return err
	}
	req.Header.Set("Authorization", "Bearer "+c.apiKey)
	req.Header.Set("Content-Type", "application/json")

	resp, err := c.http.Do(req)
	if err != nil {
		return err
	}
	defer resp.Body.Close()

	if resp.StatusCode < 200 || resp.StatusCode >= 300 {
		body, _ := io.ReadAll(resp.Body)
		return fmt.Errorf("HTTP %d POST %s: %s", resp.StatusCode, path, string(body))
	}
	return json.NewDecoder(resp.Body).Decode(dst)
}

// ── Chain queries ─────────────────────────────────────────────────────────────

func (c *Client) fetchSequenceNumber(ctx context.Context) (uint64, error) {
	var account struct {
		SequenceNumber string `json:"sequence_number"`
	}
	if err := c.getJSON(ctx, "/accounts/"+c.senderAddress, &account); err != nil {
		return 0, fmt.Errorf("fetch sequence_number: %w", err)
	}
	var seq uint64
	if _, err := fmt.Sscanf(account.SequenceNumber, "%d", &seq); err != nil {
		return 0, fmt.Errorf("parse sequence_number %q: %w", account.SequenceNumber, err)
	}
	return seq, nil
}

func (c *Client) fetchGasUnitPrice(ctx context.Context) (uint64, error) {
	var gas struct {
		GasEstimate uint64 `json:"gas_estimate"`
	}
	if err := c.getJSON(ctx, "/estimate_gas_price", &gas); err != nil {
		return 0, fmt.Errorf("fetch gas_estimate: %w", err)
	}
	return gas.GasEstimate, nil
}

// ── Transaction lifecycle ─────────────────────────────────────────────────────

// SubmitEntryFunction builds, signs, and submits an entry function transaction.
//
// Flow:
//  1. Parallel-fetch sequence_number and gas_unit_price.
//  2. Build unsigned JSON tx body.
//  3. POST /transactions/encode_submission → BCS bytes (hex).
//  4. Ed25519-sign the bytes.
//  5. Attach signature and POST /transactions.
//  6. Poll until committed.
func (c *Client) SubmitEntryFunction(
	ctx context.Context,
	function string,
	typeArgs []string,
	args []any,
) (*TxResult, error) {
	// nil type_arguments serialises to JSON null; Aptos requires an empty array.
	if typeArgs == nil {
		typeArgs = []string{}
	}

	// If we have a configured SDK client, prefer the SDK path (build/sign/submit/wait).
	if c.sdk != nil && c.sdkAccount != nil {
		if typeArgs == nil {
			typeArgs = []string{}
		}

		// Parse function spec: 0xaddr::module::function
		parts := strings.Split(function, "::")
		if len(parts) != 3 {
			return nil, fmt.Errorf("invalid function spec: %s", function)
		}
		moduleAddr, err := aptsdk.ParseAddress(parts[0])
		if err != nil {
			return nil, fmt.Errorf("parse module address %s: %w", parts[0], err)
		}

		// Convert type argument strings to SDK TypeTag values
		typeTags := make([]aptsdk.TypeTag, 0, len(typeArgs))
		for _, t := range typeArgs {
			tt, err := aptsdk.ParseTypeTag(t)
			if err != nil {
				return nil, fmt.Errorf("parse type tag %q: %w", t, err)
			}
			typeTags = append(typeTags, *tt)
		}

		// Convert simple string address arguments like "0x..." into AccountAddress
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

		// Sign and submit via SDK; set max gas to 20000 units.
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

	// 1. Fetch chain state in parallel.
	var seqNum, gasPrice uint64
	g, gctx := errgroup.WithContext(ctx)
	g.Go(func() error {
		var err error
		seqNum, err = c.fetchSequenceNumber(gctx)
		return err
	})
	g.Go(func() error {
		var err error
		gasPrice, err = c.fetchGasUnitPrice(gctx)
		return err
	})
	if err := g.Wait(); err != nil {
		return nil, fmt.Errorf("pre-tx chain query: %w", err)
	}

	expiry := time.Now().Add(60 * time.Second).Unix()

	// 2. Build unsigned tx body.
	unsignedTx := map[string]any{
		"sender":                    c.senderAddress,
		"sequence_number":           fmt.Sprintf("%d", seqNum),
		"max_gas_amount":            "20000",
		"gas_unit_price":            fmt.Sprintf("%d", gasPrice),
		"expiration_timestamp_secs": fmt.Sprintf("%d", expiry),
		"payload": map[string]any{
			"type":           "entry_function_payload",
			"function":       function,
			"type_arguments": typeArgs,
			"arguments":      args,
		},
	}

	slog.Debug("encoding transaction", "function", function)

	// 3. Encode to BCS bytes for signing.
	var encodedHex string
	if err := c.postJSON(ctx, "/transactions/encode_submission", unsignedTx, &encodedHex); err != nil {
		return nil, fmt.Errorf("encode_submission: %w", err)
	}
	bytesToSign, err := hex.DecodeString(strings.TrimPrefix(encodedHex, "0x"))
	if err != nil {
		return nil, fmt.Errorf("decode hex from encode_submission: %w", err)
	}

	// 4. Sign.
	sig := ed25519.Sign(c.privKey, bytesToSign)
	pubKey := c.privKey.Public().(ed25519.PublicKey)

	// 5. Attach signature.
	unsignedTx["signature"] = map[string]string{
		"type":       "ed25519_signature",
		"public_key": "0x" + hex.EncodeToString(pubKey),
		"signature":  "0x" + hex.EncodeToString(sig),
	}

	slog.Debug("submitting transaction", "function", function)

	// 6. Submit.
	var submitResp map[string]any
	if err := c.postJSON(ctx, "/transactions", unsignedTx, &submitResp); err != nil {
		return nil, fmt.Errorf("submit transaction: %w", err)
	}
	hash, _ := submitResp["hash"].(string)
	if hash == "" {
		return nil, fmt.Errorf("no hash in transaction response")
	}
	slog.Debug("transaction submitted", "hash", hash)

	return c.waitForTransaction(ctx, hash)
}

// waitForTransaction polls until the tx is committed or times out (~12 s).
func (c *Client) waitForTransaction(ctx context.Context, hash string) (*TxResult, error) {
	// Give the network a head-start.
	select {
	case <-ctx.Done():
		return nil, ctx.Err()
	case <-time.After(500 * time.Millisecond):
	}

	for attempt := 1; attempt <= 12; attempt++ {
		req, _ := http.NewRequestWithContext(ctx, http.MethodGet,
			c.baseURL+"/transactions/by_hash/"+hash, nil)
		req.Header.Set("Authorization", "Bearer "+c.apiKey)

		resp, err := c.http.Do(req)
		if err != nil {
			slog.Debug("poll error, retrying", "attempt", attempt, "err", err)
			sleep(ctx, time.Second)
			continue
		}

		if resp.StatusCode == http.StatusNotFound {
			slog.Debug("tx not yet indexed", "attempt", attempt)
			resp.Body.Close()
			sleep(ctx, time.Second)
			continue
		}

		body, _ := io.ReadAll(resp.Body)
		resp.Body.Close()

		if resp.StatusCode < 200 || resp.StatusCode >= 300 {
			return nil, fmt.Errorf("HTTP %d polling tx %s", resp.StatusCode, hash)
		}

		var txData map[string]any
		if err := json.Unmarshal(body, &txData); err != nil {
			return nil, fmt.Errorf("parse tx response: %w", err)
		}

		if txType, _ := txData["type"].(string); txType == "pending_transaction" {
			slog.Debug("tx pending", "attempt", attempt)
			sleep(ctx, time.Second)
			continue
		}

		success, _ := txData["success"].(bool)
		vmStatus, _ := txData["vm_status"].(string)
		if vmStatus == "" {
			vmStatus = "unknown"
		}

		if !success {
			slog.Warn("transaction failed on-chain", "hash", hash, "vm_status", vmStatus)
		} else {
			slog.Debug("transaction succeeded", "hash", hash)
		}
		return &TxResult{Hash: hash, Success: success, VMStatus: vmStatus}, nil
	}

	return nil, fmt.Errorf("transaction %s not committed after 12 attempts", hash)
}

// ─────────────────────────────────────────────────────────────────────────────
// Address derivation
// ─────────────────────────────────────────────────────────────────────────────

// DeriveAddress computes the Aptos account address from an Ed25519 private key.
//
//	address = sha3_256(public_key_bytes || 0x00)   // 0x00 = Ed25519 scheme tag
func DeriveAddress(privKey ed25519.PrivateKey) string {
	pubKey := privKey.Public().(ed25519.PublicKey)
	h := sha3.New256()
	h.Write(pubKey)
	h.Write([]byte{0x00})
	return "0x" + hex.EncodeToString(h.Sum(nil))
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

// NoneOption returns the Move-ABI encoding for a None optional value.
func NoneOption() map[string][]any {
	return map[string][]any{"vec": {}}
}

// sleep respects context cancellation.
func sleep(ctx context.Context, d time.Duration) {
	select {
	case <-ctx.Done():
	case <-time.After(d):
	}
}
