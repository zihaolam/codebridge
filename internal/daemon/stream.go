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
				screen, off, maxOff := s.RenderScroll(int(scrollOff.Load()))
				cx, cy := s.Cursor()
				if screen != last || off != lastOff || maxOff != lastMax || cx != lastCx || cy != lastCy {
					last, lastOff, lastMax = screen, off, maxOff
					lastCx, lastCy = cx, cy
					_ = write(ipc.StreamDown{
						Type:      "frame",
						Screen:    screen,
						CursorX:   cx,
						CursorY:   cy,
						Alt:       s.IsAltScreen(),
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
		case "detach":
			closeStop()
			return
		}
	}
	closeStop()
}
