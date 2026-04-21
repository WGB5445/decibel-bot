package engine

import (
	"sync"

	"decibel-mm-bot/internal/models"
)

// EventBus is a simple in-memory pub-sub for strategy events
type EventBus struct {
	subscribers []chan models.Event
	mu          sync.RWMutex
}

// NewEventBus creates a new event bus
func NewEventBus() *EventBus {
	return &EventBus{
		subscribers: make([]chan models.Event, 0),
	}
}

// Subscribe returns a new event channel
func (eb *EventBus) Subscribe() <-chan models.Event {
	ch := make(chan models.Event, 256)
	eb.mu.Lock()
	defer eb.mu.Unlock()
	eb.subscribers = append(eb.subscribers, ch)
	return ch
}

// Publish sends an event to all subscribers (non-blocking, drops if full)
func (eb *EventBus) Publish(ev models.Event) {
	eb.mu.RLock()
	defer eb.mu.RUnlock()
	for _, ch := range eb.subscribers {
		select {
		case ch <- ev:
		default:
			// drop if subscriber is slow
		}
	}
}

// Close shuts down all subscriber channels
func (eb *EventBus) Close() {
	eb.mu.Lock()
	defer eb.mu.Unlock()
	for _, ch := range eb.subscribers {
		close(ch)
	}
	eb.subscribers = nil
}
