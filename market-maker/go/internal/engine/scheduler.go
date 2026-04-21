package engine

import (
	"context"
	"time"

	"decibel-mm-bot/internal/models"
)

// Scheduler generates tick events at a configured interval
type Scheduler struct {
	interval time.Duration
	bus      *EventBus
}

// NewScheduler creates a scheduler
func NewScheduler(interval time.Duration, bus *EventBus) *Scheduler {
	return &Scheduler{
		interval: interval,
		bus:      bus,
	}
}

// Start runs the scheduler loop
func (s *Scheduler) Start(ctx context.Context) {
	ticker := time.NewTicker(s.interval)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case t := <-ticker.C:
			s.bus.Publish(models.Event{
				Type: models.EventTick,
				Data: t,
				TS:   t,
			})
		}
	}
}
