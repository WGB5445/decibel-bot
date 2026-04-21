package decibel

import (
	"context"
	"fmt"
	"strings"
	"sync"
	"time"

	"github.com/aptos-labs/aptos-go-sdk"
	"github.com/shopspring/decimal"
)

// WriteClient handles on-chain write operations to Decibel
type WriteClient struct {
	node        *NodeClient
	signer      *aptos.Account
	packageAddr aptos.AccountAddress

	bulkMu        sync.Mutex
	bulkSeq       uint64
	bulkSeqSynced bool
}

// NewWriteClient creates a new write client
func NewWriteClient(node *NodeClient, signer *aptos.Account, packageAddrHex string) (*WriteClient, error) {
	packageAddr := &aptos.AccountAddress{}
	if err := packageAddr.ParseStringRelaxed(packageAddrHex); err != nil {
		return nil, fmt.Errorf("parse package address: %w", err)
	}
	return &WriteClient{
		node:        node,
		signer:      signer,
		packageAddr: *packageAddr,
	}, nil
}

// WalletAddress returns the Aptos signing wallet address.
func (w *WriteClient) WalletAddress() string {
	addr := w.signer.AccountAddress()
	return addr.String()
}

// PlaceOrder places a single order on-chain
func (w *WriteClient) PlaceOrder(ctx context.Context, req PlaceOrderRequest) (*TxResult, error) {
	priceInt := scalePrice(req.Price, PriceDecimals)
	sizeInt := scaleSize(req.Size, SizeDecimals)

	fn := fmt.Sprintf("%s::%s::%s", w.packageAddr.String(), ModuleDexAccountsEntry, FuncPlaceOrderToSubaccount)
	return w.node.SubmitEntryFunction(ctx, w.signer, fn, nil, buildPlaceOrderArgs(
		req.SubaccountAddr,
		req.MarketAddr,
		priceInt, sizeInt,
		req.IsBuy,
		uint8(req.TimeInForce),
		req.IsReduceOnly,
	))
}

// CancelOrder cancels a single order on-chain
func (w *WriteClient) CancelOrder(ctx context.Context, subaccountAddr, marketAddr, orderID string) (*TxResult, error) {
	fn := fmt.Sprintf("%s::%s::%s", w.packageAddr.String(), ModuleDexAccountsEntry, FuncCancelOrder)
	return w.node.SubmitEntryFunction(ctx, w.signer, fn, nil, []any{
		subaccountAddr,
		orderID,
		marketAddr,
	})
}

// CancelAllOrders cancels all orders for a market on-chain
func (w *WriteClient) CancelAllOrders(ctx context.Context, subaccountAddr, marketAddr string) (*TxResult, error) {
	fn := fmt.Sprintf("%s::%s::%s", w.packageAddr.String(), ModuleDexAccountsEntry, FuncCancelAllOrders)
	return w.node.SubmitEntryFunction(ctx, w.signer, fn, nil, []any{
		subaccountAddr,
		marketAddr,
	})
}

// DepositUSDC deposits USDC collateral to a subaccount
func (w *WriteClient) DepositUSDC(ctx context.Context, subaccountAddr string, amount uint64) (*TxResult, error) {
	fn := fmt.Sprintf("%s::%s::%s", w.packageAddr.String(), ModuleDexAccountsEntry, FuncDepositToSubaccount)
	return w.node.SubmitEntryFunction(ctx, w.signer, fn, nil, []any{
		subaccountAddr,
		fmt.Sprintf("%d", amount),
	})
}

// WithdrawUSDC withdraws USDC collateral from a subaccount
func (w *WriteClient) WithdrawUSDC(ctx context.Context, subaccountAddr string, amount uint64) (*TxResult, error) {
	fn := fmt.Sprintf("%s::%s::%s", w.packageAddr.String(), ModuleDexAccountsEntry, FuncWithdrawFromSubaccount)
	return w.node.SubmitEntryFunction(ctx, w.signer, fn, nil, []any{
		subaccountAddr,
		fmt.Sprintf("%d", amount),
	})
}

// ConfigureMarketSettings sets leverage and margin type for a market
func (w *WriteClient) ConfigureMarketSettings(ctx context.Context, subaccountAddr, marketAddr string, leverageBps uint64, isCross bool) (*TxResult, error) {
	fn := fmt.Sprintf("%s::%s::%s", w.packageAddr.String(), ModuleDexAccountsEntry, FuncConfigureUserSettings)
	return w.node.SubmitEntryFunction(ctx, w.signer, fn, nil, []any{
		subaccountAddr,
		marketAddr,
		isCross,
		fmt.Sprintf("%d", leverageBps),
	})
}

// syncBulkSeqFromREST sets w.bulkSeq to max(sequence_number) from GET /bulk_orders
// so the next live PlaceBulkOrders uses max+1. Caller must hold w.bulkMu.
func (w *WriteClient) syncBulkSeqFromREST(ctx context.Context, readClient *ReadClient, subaccountAddr, marketAddr string) error {
	const maxAttempts = 3
	var lastErr error
	for attempt := 1; attempt <= maxAttempts; attempt++ {
		rows, err := readClient.GetBulkOrders(ctx, subaccountAddr, marketAddr)
		if err != nil {
			lastErr = err
			backoff := time.Duration(attempt*attempt) * time.Second
			select {
			case <-ctx.Done():
				return ctx.Err()
			case <-time.After(backoff):
			}
			continue
		}
		var maxSeq uint64
		for _, r := range rows {
			if r.SequenceNumber > maxSeq {
				maxSeq = r.SequenceNumber
			}
		}
		w.bulkSeq = maxSeq
		w.bulkSeqSynced = true
		return nil
	}
	return fmt.Errorf("sync bulk sequence failed after %d attempts: %w", maxAttempts, lastErr)
}

// PlaceBulkOrders places multiple orders in a single on-chain transaction.
// Decibel's Move function signature:
//   place_bulk_orders_to_subaccount(
//     subaccount: address,
//     market: address,
//     sequence_number: u64,
//     bid_prices: vector<u64>,
//     bid_sizes: vector<u64>,
//     ask_prices: vector<u64>,
//     ask_sizes: vector<u64>,
//     builder_address: Option<address>,
//     builder_fees: Option<u64>
//   )
func (w *WriteClient) PlaceBulkOrders(ctx context.Context, readClient *ReadClient, subaccountAddr, marketAddr string, bids, asks []BulkOrderRequest) (*TxResult, error) {
	w.bulkMu.Lock()
	defer w.bulkMu.Unlock()

	if !w.bulkSeqSynced {
		if err := w.syncBulkSeqFromREST(ctx, readClient, subaccountAddr, marketAddr); err != nil {
			return nil, fmt.Errorf("sync bulk sequence from REST: %w", err)
		}
	}

	bidPrices := make([]string, len(bids))
	bidSizes := make([]string, len(bids))
	for i, b := range bids {
		bidPrices[i] = fmt.Sprintf("%d", scalePrice(b.Price, PriceDecimals))
		bidSizes[i] = fmt.Sprintf("%d", scaleSize(b.Size, SizeDecimals))
	}
	askPrices := make([]string, len(asks))
	askSizes := make([]string, len(asks))
	for i, a := range asks {
		askPrices[i] = fmt.Sprintf("%d", scalePrice(a.Price, PriceDecimals))
		askSizes[i] = fmt.Sprintf("%d", scaleSize(a.Size, SizeDecimals))
	}

	w.bulkSeq++

	fn := fmt.Sprintf("%s::%s::%s", w.packageAddr.String(), ModuleDexAccountsEntry, FuncPlaceBulkOrders)
	result, err := w.node.SubmitEntryFunction(ctx, w.signer, fn, nil, []any{
		subaccountAddr,
		marketAddr,
		w.bulkSeq,
		bidPrices,
		bidSizes,
		askPrices,
		askSizes,
		nil, // builder_address Option<address>
		nil, // builder_fees Option<u64>
	})
	if err != nil {
		return result, err
	}
	return result, nil
}

// ResetBulkSeq forces a re-sync of the bulk sequence number on next PlaceBulkOrders call.
func (w *WriteClient) ResetBulkSeq() {
	w.bulkMu.Lock()
	defer w.bulkMu.Unlock()
	w.bulkSeqSynced = false
	w.bulkSeq = 0
}

func buildPlaceOrderArgs(
	subaccountAddr, marketAddr string,
	priceInt, sizeInt uint64,
	isBuy bool,
	timeInForce uint8,
	isReduceOnly bool,
) []any {
	return []any{
		subaccountAddr,
		marketAddr,
		fmt.Sprintf("%d", priceInt),
		fmt.Sprintf("%d", sizeInt),
		isBuy,
		timeInForce,
		isReduceOnly,
		nil, // client_order_id Option<String>
		nil, // stop_price Option<u64>
		nil, // tp_trigger Option<u64>
		nil, // tp_limit Option<u64>
		nil, // sl_trigger Option<u64>
		nil, // sl_limit Option<u64>
		nil, // builder_addr Option<address>
		nil, // builder_fees Option<u64>
	}
}

func scalePrice(price decimal.Decimal, decimals int) uint64 {
	scale := decimal.NewFromInt(1).Shift(int32(decimals))
	scaled := price.Mul(scale).Truncate(0)
	if scaled.LessThan(decimal.Zero) {
		return 0
	}
	return uint64(scaled.BigInt().Uint64())
}

func scaleSize(size decimal.Decimal, decimals int) uint64 {
	scale := decimal.NewFromInt(1).Shift(int32(decimals))
	scaled := size.Mul(scale).Truncate(0)
	if scaled.LessThan(decimal.Zero) {
		return 0
	}
	return uint64(scaled.BigInt().Uint64())
}

// OutcomeFromTxResult extracts a PlaceOrderOutcome from a TxResult.
func OutcomeFromTxResult(r *TxResult) PlaceOrderOutcome {
	if r == nil {
		return PlaceOrderOutcome{}
	}
	return PlaceOrderOutcome{
		TxHash:  r.Hash,
		OrderID: OrderIDFromEvents(r.Events),
	}
}

// AddrEqual compares two Aptos addresses case-insensitively,
// ignoring leading zeros and the "0x" prefix.
func AddrEqual(a, b string) bool {
	return NormalizeAddr(a) == NormalizeAddr(b)
}

// NormalizeAddr strips the "0x" prefix, lowercases, and removes leading zeros.
func NormalizeAddr(addr string) string {
	s := strings.TrimPrefix(strings.ToLower(addr), "0x")
	s = strings.TrimLeft(s, "0")
	if s == "" {
		return "0"
	}
	return s
}

// AddrSuffix returns the last 6 characters of NormalizeAddr for compact logs.
func AddrSuffix(addr string) string {
	n := NormalizeAddr(addr)
	if len(n) <= 6 {
		return n
	}
	return n[len(n)-6:]
}
