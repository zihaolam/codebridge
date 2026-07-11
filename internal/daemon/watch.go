package daemon

import (
	"bufio"
	"encoding/json"
	"net"
	"time"

	"codebridge/internal/ipc"
)

// watchInterval is the safety-net cadence for state changes that don't fire a
// notifyChange — chiefly a child exiting (noticed by pruneExited), which has
// no hook event of its own.
const watchInterval = time.Second

// subscribeChanges registers a coalescing wakeup channel for watch streams.
func (d *Daemon) subscribeChanges() chan struct{} {
	d.watchMu.Lock()
	defer d.watchMu.Unlock()
	if d.watchers == nil {
		d.watchers = make(map[chan struct{}]struct{})
	}
	ch := make(chan struct{}, 1)
	d.watchers[ch] = struct{}{}
	return ch
}

func (d *Daemon) unsubscribeChanges(ch chan struct{}) {
	d.watchMu.Lock()
	defer d.watchMu.Unlock()
	delete(d.watchers, ch)
}

// notifyChange wakes every watch stream. Non-blocking: the 1-buffered wakeup
// channels coalesce bursts (e.g. rapid PreToolUse/PostToolUse hook storms)
// into a single re-snapshot per watcher.
func (d *Daemon) notifyChange() {
	d.watchMu.Lock()
	defer d.watchMu.Unlock()
	for ch := range d.watchers {
		select {
		case ch <- struct{}{}:
		default:
		}
	}
}

// watch takes over conn as a push stream: one Response{OK, Sessions} line
// immediately, then another whenever the registry changes (spawn / kill /
// rename / hook status) or the safety ticker notices a quieter change (child
// exit). Pushes are deduped against the last snapshot's JSON so wakeups that
// don't alter the visible list write nothing. Returns when the client closes.
func (d *Daemon) watch(conn net.Conn, sc *bufio.Scanner) {
	ch := d.subscribeChanges()
	defer d.unsubscribeChanges(ch)

	// The client sends nothing after the watch request, so the scanner
	// unblocking means it disconnected — the ticker path would only notice on
	// the next write, which could be much later on an idle daemon.
	done := make(chan struct{})
	go func() {
		for sc.Scan() {
		}
		close(done)
	}()

	t := time.NewTicker(watchInterval)
	defer t.Stop()
	last := ""
	push := func() bool {
		d.pruneExited()
		snap := d.snapshot()
		b, err := json.Marshal(snap)
		if err != nil {
			return true
		}
		if s := string(b); s != last {
			last = s
			return ipc.WriteJSON(conn, ipc.Response{OK: true, Sessions: snap}) == nil
		}
		return true
	}
	if !push() {
		return
	}
	for {
		select {
		case <-done:
			return
		case <-ch:
		case <-t.C:
		}
		if !push() {
			return
		}
	}
}
