package aptos

import (
	"context"
	"errors"
	"testing"

	aptossdk "github.com/aptos-labs/aptos-go-sdk"
)

// testSigner is a fixed, valid Ed25519 test account — no network needed to construct it.
func testSigner(t *testing.T) *aptossdk.Account {
	t.Helper()
	acct, err := ParseAccount("0x1111111111111111111111111111111111111111111111111111111111111111")
	if err != nil {
		t.Fatalf("ParseAccount: %v", err)
	}
	return acct
}

// fakeBackend is a scriptable txSubmitBackend for testing TxSubmitter's sequencing logic
// without a live Aptos node.
type fakeBackend struct {
	initialSeq    uint64
	initialSeqErr error

	// submitResults is consumed in order, one per submitEntryFunction call. If shorter
	// than the number of calls, the last entry repeats.
	submitResults []submitResult
	calls         []uint64 // sequence numbers passed to submitEntryFunction, in call order
}

type submitResult struct {
	result *TxResult
	err    error
}

func (f *fakeBackend) initialSequenceNumber(_ aptossdk.AccountAddress) (uint64, error) {
	return f.initialSeq, f.initialSeqErr
}

func (f *fakeBackend) submitEntryFunction(_ context.Context, _ aptossdk.TransactionSigner, _ string, _ []string, _ []any, seq *uint64) (*TxResult, error) {
	f.calls = append(f.calls, *seq)
	idx := len(f.calls) - 1
	if idx >= len(f.submitResults) {
		idx = len(f.submitResults) - 1
	}
	if idx < 0 {
		return nil, errors.New("fakeBackend: no submitResults configured")
	}
	r := f.submitResults[idx]
	return r.result, r.err
}

func TestTxSubmitterFetchesInitialSequenceLazily(t *testing.T) {
	backend := &fakeBackend{
		initialSeq:    42,
		submitResults: []submitResult{{result: &TxResult{Hash: "0x1", Success: true}}},
	}
	ts := &TxSubmitter{backend: backend}
	if ts.seqSet {
		t.Fatal("expected seqSet=false before first call")
	}

	signer := testSigner(t)
	_, err := ts.SubmitEntryFunction(context.Background(), signer, "0x1::m::f", nil, nil)
	if err != nil {
		t.Fatalf("SubmitEntryFunction: %v", err)
	}
	if !ts.seqSet {
		t.Error("expected seqSet=true after first call")
	}
	if len(backend.calls) != 1 || backend.calls[0] != 42 {
		t.Errorf("expected first submit to use seq=42, got calls=%v", backend.calls)
	}
}

func TestTxSubmitterAdvancesSequenceOnSuccess(t *testing.T) {
	backend := &fakeBackend{
		initialSeq: 10,
		submitResults: []submitResult{
			{result: &TxResult{Hash: "0x1", Success: true}},
			{result: &TxResult{Hash: "0x2", Success: true}},
		},
	}
	ts := &TxSubmitter{backend: backend}
	signer := testSigner(t)

	for i := 0; i < 2; i++ {
		if _, err := ts.SubmitEntryFunction(context.Background(), signer, "0x1::m::f", nil, nil); err != nil {
			t.Fatalf("call %d: %v", i, err)
		}
	}
	if len(backend.calls) != 2 || backend.calls[0] != 10 || backend.calls[1] != 11 {
		t.Errorf("expected sequence 10 then 11, got %v", backend.calls)
	}
}

// The core safety property: when submission actually reached the mempool (a TxResult
// was returned, even alongside an error — e.g. WaitForTransaction timed out), the
// sequence number MUST advance so the next call doesn't attempt to resubmit the same
// slot (which the chain would reject as a duplicate/replay).
func TestTxSubmitterAdvancesSequenceOnWaitPendingError(t *testing.T) {
	backend := &fakeBackend{
		initialSeq: 5,
		submitResults: []submitResult{
			{result: &TxResult{Hash: "0xpending", VMStatus: VMStatusWaitPending}, err: errors.New("wait for transaction: timeout")},
			{result: &TxResult{Hash: "0xnext", Success: true}},
		},
	}
	ts := &TxSubmitter{backend: backend}
	signer := testSigner(t)

	if _, err := ts.SubmitEntryFunction(context.Background(), signer, "0x1::m::f", nil, nil); err == nil {
		t.Fatal("expected the first call to return the wait-pending error")
	}
	if _, err := ts.SubmitEntryFunction(context.Background(), signer, "0x1::m::f", nil, nil); err != nil {
		t.Fatalf("second call: %v", err)
	}
	if len(backend.calls) != 2 || backend.calls[0] != 5 || backend.calls[1] != 6 {
		t.Errorf("expected sequence 5 then 6 (advance despite wait-pending error), got %v", backend.calls)
	}
}

// The complementary safety property: when submission never reached the mempool (no
// TxResult at all — e.g. a build/sign error before SubmitTransaction), the sequence
// number must NOT advance, so the same slot is safely retried.
func TestTxSubmitterDoesNotAdvanceSequenceOnPreSubmitFailure(t *testing.T) {
	backend := &fakeBackend{
		initialSeq: 7,
		submitResults: []submitResult{
			{result: nil, err: errors.New("entry function from ABI: boom")},
			{result: &TxResult{Hash: "0xok", Success: true}},
		},
	}
	ts := &TxSubmitter{backend: backend}
	signer := testSigner(t)

	if _, err := ts.SubmitEntryFunction(context.Background(), signer, "0x1::m::f", nil, nil); err == nil {
		t.Fatal("expected the first call to fail")
	}
	if _, err := ts.SubmitEntryFunction(context.Background(), signer, "0x1::m::f", nil, nil); err != nil {
		t.Fatalf("second call: %v", err)
	}
	if len(backend.calls) != 2 || backend.calls[0] != 7 || backend.calls[1] != 7 {
		t.Errorf("expected sequence 7 reused on both calls (no advance on pre-submit failure), got %v", backend.calls)
	}
}

func TestTxSubmitterPropagatesInitialSequenceFetchError(t *testing.T) {
	backend := &fakeBackend{initialSeqErr: errors.New("network down")}
	ts := &TxSubmitter{backend: backend}
	signer := testSigner(t)

	if _, err := ts.SubmitEntryFunction(context.Background(), signer, "0x1::m::f", nil, nil); err == nil {
		t.Fatal("expected an error when the initial sequence-number fetch fails")
	}
	if ts.seqSet {
		t.Error("expected seqSet to remain false after a failed initial fetch")
	}
}
