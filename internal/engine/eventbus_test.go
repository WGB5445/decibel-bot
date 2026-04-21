package engine

import (
	"testing"

	"github.com/bujih/decibel-mm-go/internal/models"
)

func TestEventBusPublishSubscribe(t *testing.T) {
	bus := NewEventBus()
	ch := bus.Subscribe()

	bus.Publish(models.Event{Type: models.EventTick})

	ev := <-ch
	if ev.Type != models.EventTick {
		t.Fatalf("expected EventTick, got %d", ev.Type)
	}

	bus.Close()
}

func TestEventBusMultipleSubscribers(t *testing.T) {
	bus := NewEventBus()
	ch1 := bus.Subscribe()
	ch2 := bus.Subscribe()

	bus.Publish(models.Event{Type: models.EventTick})

	ev1 := <-ch1
	ev2 := <-ch2
	if ev1.Type != models.EventTick || ev2.Type != models.EventTick {
		t.Fatal("both subscribers should receive the event")
	}

	bus.Close()
}
