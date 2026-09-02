package aptos

import (
	"context"
	"fmt"
	"sync"

	aptossdk "github.com/aptos-labs/aptos-go-sdk"
)

// EntryFunctionSubmitter is implemented by both *NodeClient (per-call live sequence
// fetch — correct for a single-market bot where one goroutine submits per signer) and
// *TxSubmitter (locally-tracked, mutex-serialized sequence number — required when
// multiple goroutines submit concurrently for the same signer, e.g. multi-market mode).
type EntryFunctionSubmitter interface {
	SubmitEntryFunction(ctx context.Context, signer aptossdk.TransactionSigner, function string, typeArgs []string, args []any) (*TxResult, error)
}

// txSubmitBackend is the seam TxSubmitter depends on instead of *NodeClient directly,
// so its sequencing/failure-handling logic can be unit-tested with a fake instead of a
// live Aptos node.
type txSubmitBackend interface {
	initialSequenceNumber(addr aptossdk.AccountAddress) (uint64, error)
	submitEntryFunction(ctx context.Context, signer aptossdk.TransactionSigner, function string, typeArgs []string, args []any, seq *uint64) (*TxResult, error)
}

// initialSequenceNumber fetches addr's current on-chain sequence number.
func (n *NodeClient) initialSequenceNumber(addr aptossdk.AccountAddress) (uint64, error) {
	info, err := n.sdk.Account(addr)
	if err != nil {
		return 0, err
	}
	return info.SequenceNumber()
}

// TxSubmitter serializes all entry-function submissions for one signer through a
// single mutex, tracking the account's sequence number locally instead of relying on
// aptos-go-sdk's default behavior of fetching the "current" sequence number live from
// chain on every call. That default races when multiple goroutines submit concurrently
// for the same account — e.g. one MarketMaker instance per market sharing one
// subaccount/signer in a multi-market bot: both could read the same pending sequence
// number before either commits, and one submission would be rejected.
//
// Submissions are fully serialized end-to-end (build+sign+submit+wait), not just
// sequence-number assignment. This trades a little throughput (a market's cycle may
// wait briefly behind another market's in-flight submission) for correctness, which is
// the right tradeoff at a market maker's ~10-30s cycle cadence — confirmations
// typically take low single-digit seconds.
type TxSubmitter struct {
	backend txSubmitBackend

	mu     sync.Mutex
	seq    uint64
	seqSet bool
}

// NewTxSubmitter creates a submitter wrapping node. The account's current sequence
// number is fetched lazily on first use, not at construction, so construction never fails.
func NewTxSubmitter(node *NodeClient) *TxSubmitter {
	return &TxSubmitter{backend: node}
}

// SubmitEntryFunction serializes this call against all other callers sharing this
// TxSubmitter (i.e. the same signer/account). See the type doc for the sequencing and
// failure-handling rules.
func (t *TxSubmitter) SubmitEntryFunction(
	ctx context.Context,
	signer aptossdk.TransactionSigner,
	function string,
	typeArgs []string,
	args []any,
) (*TxResult, error) {
	t.mu.Lock()
	defer t.mu.Unlock()

	if !t.seqSet {
		seqNo, err := t.backend.initialSequenceNumber(signer.AccountAddress())
		if err != nil {
			return nil, fmt.Errorf("tx submitter: fetch initial sequence number: %w", err)
		}
		t.seq = seqNo
		t.seqSet = true
	}

	result, err := t.backend.submitEntryFunction(ctx, signer, function, typeArgs, args, &t.seq)

	// Advance the local sequence number whenever the transaction was actually
	// submitted to the mempool (result != nil, i.e. we have a hash) — regardless of
	// whether confirmation succeeded, timed out (VMStatusWaitPending), or the tx later
	// fails on-chain; in all those cases the sequence number slot has been consumed
	// from the mempool's perspective. Only skip advancing when submission itself never
	// happened (result == nil, e.g. ABI/build/sign error before SubmitTransaction), so
	// the same sequence number is safely retried on the next call.
	if result != nil {
		t.seq++
	}
	return result, err
}
