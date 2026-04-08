package main

import (
	"context"
	"fmt"
	"log"
	"net/http"
	"os"
	"time"

	"decibel-mm-bot/config"

	aptsdk "github.com/aptos-labs/aptos-go-sdk/v2"
	sdkacct "github.com/aptos-labs/aptos-go-sdk/v2/account"
)

// httpDoer implements the SDK's HTTPDoer using net/http.Client and headers.
type httpDoer struct {
	client  *http.Client
	headers map[string]string
}

func (h *httpDoer) Do(ctx context.Context, req *http.Request) (*http.Response, error) {
	req = req.WithContext(ctx)
	for k, v := range h.headers {
		req.Header.Set(k, v)
	}
	return h.client.Do(req)
}

func main() {
	ctx := context.Background()
	cfg, err := config.Load()
	if err != nil {
		log.Fatalf("load config: %v", err)
	}

	// Respect the operator-controlled DRY_RUN requirement: if DRY_RUN env
	// variable was not explicitly set, we will only simulate and will NOT
	// submit to the network. This avoids accidental submissions.
	if _, ok := os.LookupEnv("DRY_RUN"); !ok {
		log.Println("DRY_RUN not explicitly set; will only simulate. Set DRY_RUN=false to enable real submission.")
	}

	seed, err := cfg.ParsePrivateKey()
	if err != nil {
		log.Fatalf("parse private key: %v", err)
	}

	// Build SDK client
	sdkCfg := aptsdk.NetworkConfig{NodeURL: cfg.AptosFullnodeURL}
	headers := map[string]string{}
	if k := cfg.NodeKey(); k != "" {
		headers["Authorization"] = "Bearer " + k
	}
	doer := &httpDoer{client: &http.Client{Timeout: 30 * time.Second}, headers: headers}
	sdkClient, err := aptsdk.NewClient(sdkCfg, aptsdk.WithHTTPClient(doer))
	if err != nil {
		log.Fatalf("new sdk client: %v", err)
	}

	acct, err := sdkacct.FromEd25519PrivateKey(seed[:])
	if err != nil {
		log.Fatalf("create account from seed: %v", err)
	}

	log.Printf("Using account: %s", acct.Address().String())

	// Build a harmless transfer payload: transfer 0 APT to our subaccount address
	recipientAddr, err := aptsdk.ParseAddress(cfg.SubaccountAddress)
	if err != nil {
		log.Fatalf("parse subaccount address: %v", err)
	}

	payload := &aptsdk.EntryFunctionPayload{
		Module:   aptsdk.ModuleID{Address: aptsdk.AccountOne, Name: "coin"},
		Function: "transfer",
		TypeArgs: []aptsdk.TypeTag{aptsdk.AptosCoinTypeTag},
		Args:     []any{recipientAddr, uint64(0)},
	}

	// Build raw transaction
	rawTxn, err := sdkClient.BuildTransaction(ctx, acct.Address(), payload, aptsdk.WithMaxGas(20000))
	if err != nil {
		log.Fatalf("build transaction: %v", err)
	}

	// Simulate
	simRes, err := sdkClient.SimulateTransaction(ctx, rawTxn, acct, aptsdk.WithMaxGas(20000))
	if err != nil {
		log.Fatalf("simulation failed: %v", err)
	}
	fmt.Printf("Simulation success=%v vm_status=%s gas_used=%d\n", simRes.Success, simRes.VMStatus, simRes.GasUsed)

	// If DRY_RUN env var not explicitly set, stop here.
	if _, ok := os.LookupEnv("DRY_RUN"); !ok {
		log.Println("Stopped after simulation because DRY_RUN not explicitly set.")
		return
	}

	// Only submit if DRY_RUN is explicitly set to "false"
	if v := os.Getenv("DRY_RUN"); v == "false" {
		subRes, err := sdkClient.SignAndSubmitTransaction(ctx, acct, payload, aptsdk.WithMaxGas(20000))
		if err != nil {
			log.Fatalf("submit failed: %v", err)
		}
		log.Printf("submitted tx hash=%s", subRes.Hash)
		tx, err := sdkClient.WaitForTransaction(ctx, subRes.Hash)
		if err != nil {
			log.Fatalf("wait for tx: %v", err)
		}
		log.Printf("tx committed success=%v vm_status=%s", tx.Success, tx.VMStatus)
		return
	}

	log.Println("DRY_RUN is set but not " + "false; skipping actual submission.")
}
