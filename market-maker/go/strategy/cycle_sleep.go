package strategy

import (
	"math/rand/v2"
	"time"
)

// minCycleSleepSeconds avoids a zero or negative time.After busy-spin when jitter is large vs base.
const minCycleSleepSeconds = 0.01

// cycleSleepDuration returns how long to wait before the next cycle.
// When jitterSec <= 0, it is baseSec (same as legacy fixed refresh).
// Otherwise sleep is uniform on [max(0.01, baseSec−jitterSec), baseSec+jitterSec].
func cycleSleepDuration(baseSec, jitterSec float64) time.Duration {
	if jitterSec <= 0 {
		return time.Duration(baseSec * float64(time.Second))
	}
	low := baseSec - jitterSec
	if low < minCycleSleepSeconds {
		low = minCycleSleepSeconds
	}
	high := baseSec + jitterSec
	if high < low {
		high = low
	}
	sec := low + rand.Float64()*(high-low)
	return time.Duration(sec * float64(time.Second))
}
