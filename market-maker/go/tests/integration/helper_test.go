//go:build integration

package integration

import (
	"context"
	"os"
	"testing"
	"time"

	"github.com/aptos-labs/aptos-go-sdk"
	"decibel-mm-bot/internal/decibel"
)

var (
	testAPIKey       string
	testPrivKey      string
	testSubaccount   string
	testMarketAddr   string
	testMarketName   string
	testPackageAddr  string
	testFullnodeURL  string
	testRESTBaseURL  string
)

func init() {
	testAPIKey = os.Getenv("TESTNET_API_KEY")
	testPrivKey = os.Getenv("TESTNET_PRIVATE_KEY")
	testSubaccount = os.Getenv("TESTNET_SUBACCOUNT")
	testMarketAddr = os.Getenv("TESTNET_MARKET")
	testMarketName = os.Getenv("TESTNET_MARKET_NAME")
	testPackageAddr = os.Getenv("TESTNET_PACKAGE_ADDR")
	testFullnodeURL = os.Getenv("TESTNET_FULLNODE_URL")
	testRESTBaseURL = os.Getenv("TESTNET_REST_BASE_URL")

	if testFullnodeURL == "" {
		testFullnodeURL = "https://api.testnet.aptoslabs.com/v1"
	}
	if testRESTBaseURL == "" {
		testRESTBaseURL = "https://api.testnet.aptoslabs.com/decibel"
	}
	if testPackageAddr == "" {
		testPackageAddr = decibel.TestnetPackageAddr
	}
	if testMarketName == "" {
		testMarketName = "APT-PERP"
	}
}

func requireEnv(t *testing.T) {
	if testAPIKey == "" {
		t.Skip("TESTNET_API_KEY not set")
	}
	if testPrivKey == "" {
		t.Skip("TESTNET_PRIVATE_KEY not set")
	}
	if testSubaccount == "" {
		t.Skip("TESTNET_SUBACCOUNT not set")
	}
	if testMarketAddr == "" {
		t.Skip("TESTNET_MARKET not set")
	}
}

func newTestNodeClient(t *testing.T) *decibel.NodeClient {
	t.Helper()
	client, err := decibel.NewNodeClient(testFullnodeURL, testAPIKey, decibel.ChainIDForNetwork("testnet"))
	if err != nil {
		t.Fatalf("failed to create node client: %v", err)
	}
	return client
}

func newTestSigner(t *testing.T) *aptos.Account {
	t.Helper()
	acc, err := decibel.ParseAccount(testPrivKey)
	if err != nil {
		t.Fatalf("failed to parse account: %v", err)
	}
	return acc
}

func newTestReadClient(t *testing.T) *decibel.ReadClient {
	t.Helper()
	return decibel.NewReadClient(testRESTBaseURL, testAPIKey)
}

func newTestWriteClient(t *testing.T) *decibel.WriteClient {
	t.Helper()
	node := newTestNodeClient(t)
	signer := newTestSigner(t)
	wc, err := decibel.NewWriteClient(node, signer, testPackageAddr)
	if err != nil {
		t.Fatalf("failed to create write client: %v", err)
	}
	return wc
}

func testContext() (context.Context, context.CancelFunc) {
	return context.WithTimeout(context.Background(), 120*time.Second)
}

func dumpResult(t *testing.T, label string, r *decibel.TxResult) {
	t.Helper()
	if r == nil {
		t.Logf("%s: nil result", label)
		return
	}
	t.Logf("%s: hash=%s success=%v vm_status=%s events=%d", label, r.Hash, r.Success, r.VMStatus, len(r.Events))
	oid := decibel.OrderIDFromEvents(r.Events)
	if oid != "" {
		t.Logf("%s: order_id=%s", label, oid)
	}
	if !r.Success {
		t.Fatalf("%s failed: %s", label, r.VMStatus)
	}
}


