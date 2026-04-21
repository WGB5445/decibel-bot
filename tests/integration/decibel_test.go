//go:build integration

package integration

import (
	"testing"

	"github.com/bujih/decibel-mm-go/internal/decibel"
	"github.com/shopspring/decimal"
)

func TestIntegration_ConnectAndBalance(t *testing.T) {
	requireEnv(t)
	ctx, cancel := testContext()
	defer cancel()

	node := newTestNodeClient(t)
	signer := newTestSigner(t)

	addr := signer.AccountAddress()
	bal, err := node.APTBalance(ctx, addr.String())
	if err != nil {
		t.Fatalf("failed to get APT balance: %v", err)
	}
	t.Logf("APT balance: %f", bal)
	if bal <= 0 {
		t.Fatalf("insufficient APT balance for gas: %f", bal)
	}
}

func TestIntegration_PlaceAndCancelOrder(t *testing.T) {
	requireEnv(t)
	ctx, cancel := testContext()
	defer cancel()

	writeClient := newTestWriteClient(t)

	// Fetch mid price from REST to place a passive quote
	readClient := newTestReadClient(t)
	prices, err := readClient.GetPrices(ctx)
	if err != nil {
		t.Fatalf("failed to get prices: %v", err)
	}
	var mid decimal.Decimal
	for _, p := range prices {
		if decibel.AddrEqual(p.Market, testMarketAddr) {
			mid = decimal.NewFromFloat(p.MidPx)
			break
		}
	}
	if mid.IsZero() {
		t.Fatalf("could not find mid price for market %s", testMarketAddr)
	}

	// Place a tiny POST_ONLY bid slightly below mid
	bidPrice := mid.Mul(decimal.NewFromFloat(0.99))
	size := decimal.NewFromFloat(0.01)

	req := decibel.PlaceOrderRequest{
		SubaccountAddr: testSubaccount,
		MarketAddr:     testMarketAddr,
		Price:          bidPrice,
		Size:           size,
		IsBuy:          true,
		TimeInForce:    decibel.TIFPostOnly,
		IsReduceOnly:   false,
	}

	placeRes, err := writeClient.PlaceOrder(ctx, req)
	if err != nil {
		t.Fatalf("place order failed: %v", err)
	}
	dumpResult(t, "place_order", placeRes)

	orderID := decibel.OrderIDFromEvents(placeRes.Events)
	if orderID == "" {
		t.Fatalf("place order did not return an order_id in events")
	}
	t.Logf("placed order_id=%s", orderID)

	// Cancel the order
	cancelRes, err := writeClient.CancelOrder(ctx, testSubaccount, testMarketAddr, orderID)
	if err != nil {
		t.Fatalf("cancel order failed: %v", err)
	}
	dumpResult(t, "cancel_order", cancelRes)
	if !cancelRes.CancelSucceeded() {
		t.Fatalf("cancel order was not successful: vm_status=%s", cancelRes.VMStatus)
	}
}

func TestIntegration_PlaceBulkOrdersAndCancelAll(t *testing.T) {
	requireEnv(t)
	ctx, cancel := testContext()
	defer cancel()

	writeClient := newTestWriteClient(t)
	readClient := newTestReadClient(t)

	// Fetch mid price from REST
	prices, err := readClient.GetPrices(ctx)
	if err != nil {
		t.Fatalf("failed to get prices: %v", err)
	}
	var mid decimal.Decimal
	for _, p := range prices {
		if decibel.AddrEqual(p.Market, testMarketAddr) {
			mid = decimal.NewFromFloat(p.MidPx)
			break
		}
	}
	if mid.IsZero() {
		t.Fatalf("could not find mid price for market %s", testMarketAddr)
	}

	size := decimal.NewFromFloat(0.01)
	bids := []decibel.BulkOrderRequest{
		{
			MarketAddr:   testMarketAddr,
			Price:        mid.Mul(decimal.NewFromFloat(0.99)),
			Size:         size,
			IsBuy:        true,
			TimeInForce:  decibel.TIFPostOnly,
			IsReduceOnly: false,
		},
	}
	asks := []decibel.BulkOrderRequest{
		{
			MarketAddr:   testMarketAddr,
			Price:        mid.Mul(decimal.NewFromFloat(1.01)),
			Size:         size,
			IsBuy:        false,
			TimeInForce:  decibel.TIFPostOnly,
			IsReduceOnly: false,
		},
	}

	bulkRes, err := writeClient.PlaceBulkOrders(ctx, readClient, testSubaccount, testMarketAddr, bids, asks)
	if err != nil {
		t.Fatalf("place bulk orders failed: %v", err)
	}
	dumpResult(t, "place_bulk_orders", bulkRes)

	// Cancel all orders for the market
	cancelRes, err := writeClient.CancelAllOrders(ctx, testSubaccount, testMarketAddr)
	if err != nil {
		t.Fatalf("cancel all orders failed: %v", err)
	}
	dumpResult(t, "cancel_all_orders", cancelRes)
}
