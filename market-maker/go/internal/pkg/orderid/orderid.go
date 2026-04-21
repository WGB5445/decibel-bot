package orderid

import (
	"fmt"
	"sync/atomic"
	"time"
)

var counter uint64

// Generate creates a unique client order ID with a prefix
func Generate(prefix string) string {
	seq := atomic.AddUint64(&counter, 1)
	ts := time.Now().UnixMilli()
	return fmt.Sprintf("%s-%d-%d", prefix, ts, seq)
}
