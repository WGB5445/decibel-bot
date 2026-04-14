package logging

import (
	"bufio"
	"errors"
	"io"
	"os"
	"sync"
	"sync/atomic"
	"time"
)

// ErrTeeClosed is returned from TeeFileWriter.Write after Close.
var ErrTeeClosed = errors.New("log tee writer is closed")

// TeeFileWriter returns an io.WriteCloser for slog tee file output: each record
// is flushed to the kernel by default; optional async mode and optional fsync.
// Close flushes and closes f — the caller must not use or close f afterward.
func TeeFileWriter(f *os.File, asyncIntervalMS int, fsync bool) io.WriteCloser {
	if asyncIntervalMS > 0 {
		return newAsyncTeeSink(f, asyncIntervalMS, fsync)
	}
	return newSyncFlushTeeSink(f, fsync)
}

// ── Synchronous: bufio + Flush after each Write (and optional Sync) ───────────

type syncFlushTeeSink struct {
	f      *os.File
	bw     *bufio.Writer
	fsync  bool
	closed atomic.Bool
}

func newSyncFlushTeeSink(f *os.File, fsync bool) *syncFlushTeeSink {
	return &syncFlushTeeSink{
		f:     f,
		bw:    bufio.NewWriter(f),
		fsync: fsync,
	}
}

func (s *syncFlushTeeSink) Write(p []byte) (int, error) {
	if s.closed.Load() {
		return 0, ErrTeeClosed
	}
	n, err := s.bw.Write(p)
	if err != nil {
		return n, err
	}
	if err := s.bw.Flush(); err != nil {
		return n, err
	}
	if s.fsync {
		if err := s.f.Sync(); err != nil {
			return n, err
		}
	}
	return n, nil
}

func (s *syncFlushTeeSink) Close() error {
	if !s.closed.CompareAndSwap(false, true) {
		return nil
	}
	_ = s.bw.Flush()
	if s.fsync {
		_ = s.f.Sync()
	}
	return s.f.Close()
}

// ── Asynchronous: queue + single worker (bounded channel; full => sync write) ─

type asyncTeeSink struct {
	f        *os.File
	fsync    bool
	interval time.Duration
	ch       chan []byte
	stop     chan struct{}
	done     sync.WaitGroup
	mu       sync.Mutex // serializes fallback writes with worker file access
	closed   atomic.Bool
}

func newAsyncTeeSink(f *os.File, intervalMS int, fsync bool) *asyncTeeSink {
	if intervalMS < 1 {
		intervalMS = 1
	}
	a := &asyncTeeSink{
		f:        f,
		fsync:    fsync,
		interval: time.Duration(intervalMS) * time.Millisecond,
		ch:       make(chan []byte, 256),
		stop:     make(chan struct{}),
	}
	a.done.Add(1)
	go a.worker()
	return a
}

func (a *asyncTeeSink) worker() {
	defer a.done.Done()
	ticker := time.NewTicker(a.interval)
	defer ticker.Stop()
	bw := bufio.NewWriter(a.f)

	flush := func() {
		_ = bw.Flush()
		if a.fsync {
			_ = a.f.Sync()
		}
	}

	writeBuf := func(p []byte) {
		a.mu.Lock()
		_, _ = bw.Write(p)
		flush()
		a.mu.Unlock()
	}

	for {
		select {
		case p := <-a.ch:
			writeBuf(p)
		case <-ticker.C:
			a.mu.Lock()
			_ = bw.Flush()
			if a.fsync {
				_ = a.f.Sync()
			}
			a.mu.Unlock()
		case <-a.stop:
			for {
				select {
				case p := <-a.ch:
					writeBuf(p)
				default:
					a.mu.Lock()
					_ = bw.Flush()
					if a.fsync {
						_ = a.f.Sync()
					}
					a.mu.Unlock()
					return
				}
			}
		}
	}
}

func (a *asyncTeeSink) Write(p []byte) (int, error) {
	if a.closed.Load() {
		return 0, ErrTeeClosed
	}
	buf := append([]byte(nil), p...)
	select {
	case a.ch <- buf:
		return len(p), nil
	default:
		a.mu.Lock()
		defer a.mu.Unlock()
		n, err := a.f.Write(p)
		if err != nil {
			return n, err
		}
		if a.fsync {
			if err := a.f.Sync(); err != nil {
				return n, err
			}
		}
		if n < len(p) {
			return n, io.ErrShortWrite
		}
		return n, nil
	}
}

func (a *asyncTeeSink) Close() error {
	if !a.closed.CompareAndSwap(false, true) {
		return nil
	}
	close(a.stop)
	a.done.Wait()
	return a.f.Close()
}
