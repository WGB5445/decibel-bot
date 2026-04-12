package aptos

import "testing"

func TestTxResult_CancelSucceeded_waitPending(t *testing.T) {
	r := &TxResult{Hash: "0xabc", Success: false, VMStatus: VMStatusWaitPending}
	if r.CancelSucceeded() {
		t.Fatal("wait_pending must not count as successful cancel")
	}
}
