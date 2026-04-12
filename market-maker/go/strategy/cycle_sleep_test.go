package strategy

import (
	"testing"
	"time"
)

func TestCycleSleepDuration_noJitter(t *testing.T) {
	t.Parallel()
	d := cycleSleepDuration(20, 0)
	if want := 20 * time.Second; d != want {
		t.Fatalf("got %v want %v", d, want)
	}
}

func TestCycleSleepDuration_jitterInRange(t *testing.T) {
	t.Parallel()
	const base, jit = 20.0, 2.0
	for range 200 {
		sec := cycleSleepDuration(base, jit).Seconds()
		if sec < base-jit-1e-9 || sec > base+jit+1e-9 {
			t.Fatalf("out of range: %v", sec)
		}
	}
}

func TestCycleSleepDuration_largeJitterClampsLow(t *testing.T) {
	t.Parallel()
	const base, jit = 1.0, 10.0
	for range 100 {
		sec := cycleSleepDuration(base, jit).Seconds()
		if sec < minCycleSleepSeconds-1e-9 {
			t.Fatalf("below floor: %v", sec)
		}
		if sec > base+jit+1e-9 {
			t.Fatalf("above high: %v", sec)
		}
	}
}

func TestCycleSleepDuration_highNotBelowLow(t *testing.T) {
	t.Parallel()
	// Degenerate: base < jitter so raw low is negative; clamped low = 0.01, high = base + jitter
	d := cycleSleepDuration(0.5, 5)
	sec := d.Seconds()
	if sec < minCycleSleepSeconds-1e-12 {
		t.Fatalf("expected low clamp: %v", sec)
	}
	if sec > 5.5+1e-9 {
		t.Fatalf("unexpected high: %v", sec)
	}
}
