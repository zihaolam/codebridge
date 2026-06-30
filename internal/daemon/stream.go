package daemon

import (
	"bufio"
	"encoding/base64"
	"encoding/json"
	"net"
	"sync"
	"sync/atomic"
	"time"

	"codebridge/internal/ipc"
)

// frameInterval bounds how often we re-render and push a frame to a client.
const frameInterval = 33 * time.Millisecond // ~30fps

// attach turns conn into a bidirectional stream for one session: frames flow
// down (throttled, deduped), input/resize/detach flow up. It returns when the
// client detaches, disconnects, or the session ends.
func (d *Daemon) attach(conn net.Conn, sc *bufio.Scanner, req ipc.Request) {
	s := d.lookup(req.ID)
	if s == nil {
		_ = ipc.WriteJSON(conn, ipc.StreamDown{Type: "gone"})
		return
	}
	if req.Rows > 0 && req.Cols > 0 {
		_ = s.Resize(req.Rows, req.Cols)
	}

	// Serialize writes: both the frame ticker and the "gone" notice write to conn.
	var wmu sync.Mutex
	write := func(v any) error {
		wmu.Lock()
		defer wmu.Unlock()
		return ipc.WriteJSON(conn, v)
	}

	stop := make(chan struct{})
	var once sync.Once
	closeStop := func() { once.Do(func() { close(stop) }) }

	// scrollOff is the client's requested scroll position (lines up from the
	// live bottom, 0 == follow). Written by the up-loop, read by the frame loop.
	var scrollOff atomic.Int64

	go func() {
		t := time.NewTicker(frameInterval)
		defer t.Stop()
		last := ""
		lastOff, lastMax := -1, -1
		lastCx, lastCy := -1, -1
		for {
			select {
			case <-stop:
				return
			case <-t.C:
				// Defer rendering while the child is inside a DEC 2026
				// Synchronized Output Mode block — codex brackets each
				// redraw with one, and the intermediate state (erase
				// display + reposition + half-written content) is exactly
				// what the spec asks the terminal to hide. Without this
				// skip the 30fps ticker captures the mid-batch state and
				// the pane visibly jumps on every codex render. Latency
				// after ESU is bounded by frameInterval, plus the
				// session-side watchdog if the block never closes.
				if s.IsSyncBlock() {
					continue
				}
				// Offset 0 (live, the common case) goes through LiveFrame, which
				// renders at most once per change and shares that render across
				// every client attached to this session. Only a client actively
				// browsing scrollback (offset > 0) needs a bespoke render of its
				// own window.
				var (
					screen string
					off    int
					maxOff int
					cx, cy int
					alt    bool
				)
				if so := int(scrollOff.Load()); so == 0 {
					screen, cx, cy, maxOff, alt = s.LiveFrame()
				} else {
					screen, off, maxOff = s.RenderScroll(so)
					cx, cy = s.Cursor()
					alt = s.IsAltScreen()
				}
				if screen != last || off != lastOff || maxOff != lastMax || cx != lastCx || cy != lastCy {
					last, lastOff, lastMax = screen, off, maxOff
					lastCx, lastCy = cx, cy
					_ = write(ipc.StreamDown{
						Type:      "frame",
						Screen:    screen,
						CursorX:   cx,
						CursorY:   cy,
						Alt:       alt,
						Offset:    off,
						MaxOffset: maxOff,
					})
				}
				if s.Exited() {
					_ = write(ipc.StreamDown{Type: "gone"})
					return
				}
			}
		}
	}()

	for sc.Scan() {
		var up ipc.StreamUp
		if err := json.Unmarshal(sc.Bytes(), &up); err != nil {
			continue
		}
		switch up.Type {
		case "input":
			if data, err := base64.StdEncoding.DecodeString(up.Data); err == nil {
				_, _ = s.WriteInput(data)
			}
		case "paste":
			if data, err := base64.StdEncoding.DecodeString(up.Data); err == nil {
				s.Paste(string(data))
			}
		case "resize":
			if up.Rows > 0 && up.Cols > 0 {
				_ = s.Resize(up.Rows, up.Cols)
			}
		case "scroll":
			scrollOff.Store(int64(up.Offset))
		case "interrupt":
			if st, ok := statusForClientInterrupt(s.Status()); ok {
				s.SetStatus(st, "")
			}
		case "detach":
			closeStop()
			return
		}
	}
	closeStop()
}
