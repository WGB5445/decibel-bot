package retry

import (
	"context"
	"time"
)

// Do executes fn up to maxAttempts with exponential backoff
func Do(ctx context.Context, maxAttempts int, initialDelay time.Duration, fn func() error) error {
	var err error
	delay := initialDelay
	for i := 0; i < maxAttempts; i++ {
		err = fn()
		if err == nil {
			return nil
		}
		if i == maxAttempts-1 {
			break
		}
		select {
		case <-ctx.Done():
			return ctx.Err()
		case <-time.After(delay):
			delay *= 2
		}
	}
	return err
}
