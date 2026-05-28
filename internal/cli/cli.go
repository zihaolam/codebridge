// Package cli routes cb subcommands.
package cli

import (
	"fmt"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"time"

	"command-center/internal/daemon"
	"command-center/internal/hook"
	"command-center/internal/ipc"
	"command-center/internal/session"
	"command-center/internal/tui"
)

// Run dispatches the given args (os.Args[1:]) to a subcommand.
func Run(args []string) error {
	if len(args) == 0 {
		return runDashboard()
	}
	switch args[0] {
	case "demo":
		return runDemo(args[1:])
	case "daemon":
		return daemon.Run()
	case "stop":
		resp, err := ipc.Send(ipc.Request{Type: "shutdown"})
		if err != nil {
			return fmt.Errorf("daemon not running")
		}
		if !resp.OK {
			return fmt.Errorf("%s", resp.Error)
		}
		fmt.Println("daemon stopped")
		return nil
	case "hook":
		return hook.Run(args[1:])
	case "ctl":
		return runCtl(args[1:])
	case "attach":
		return runAttach(args[1:])
	case "tile":
		_, err := tui.Tile()
		return err
	case "install-hooks":
		return hook.Install(args[1:])
	case "-h", "--help", "help":
		fmt.Println("usage: cb [daemon|ctl|hook|demo] ...")
		return nil
	default:
		return fmt.Errorf("unknown subcommand %q", args[0])
	}
}

// runCtl is a debug client for driving the daemon: list / spawn / kill.
func runCtl(args []string) error {
	if len(args) == 0 {
		return fmt.Errorf("usage: cb ctl <list|spawn|kill> ...")
	}
	switch args[0] {
	case "list":
		resp, err := ipc.Send(ipc.Request{Type: "list"})
		if err != nil {
			return err
		}
		if len(resp.Sessions) == 0 {
			fmt.Println("(no sessions)")
		}
		for _, s := range resp.Sessions {
			fmt.Printf("%s  %-14s exited=%v  %v  %s\n", s.ID[:8], s.Status, s.Exited, s.Argv, s.LastMessage)
		}
		return nil
	case "spawn":
		argv := args[1:]
		cwd, _ := os.Getwd()
		resp, err := ipc.Send(ipc.Request{Type: "spawn", Argv: argv, Cwd: cwd})
		if err != nil {
			return err
		}
		if !resp.OK {
			return fmt.Errorf("%s", resp.Error)
		}
		fmt.Println(resp.ID)
		return nil
	case "kill":
		if len(args) < 2 {
			return fmt.Errorf("usage: cb ctl kill <id>")
		}
		resp, err := ipc.Send(ipc.Request{Type: "kill", ID: args[1]})
		if err != nil {
			return err
		}
		if !resp.OK {
			return fmt.Errorf("%s", resp.Error)
		}
		return nil
	default:
		return fmt.Errorf("unknown ctl command %q", args[0])
	}
}

// runDashboard runs the central dashboard, handing off to the attach view when
// the user selects a session and returning to the dashboard on detach. It auto-
// starts the daemon if it isn't already running.
func runDashboard() error {
	if err := ensureDaemon(); err != nil {
		return err
	}
	selectID := ""
	for {
		id, action, err := tui.Dashboard(selectID)
		if err != nil {
			return err
		}
		switch action {
		case tui.DashQuit:
			return nil
		case tui.DashTile:
			focused, err := tui.Tile()
			if err != nil {
				return err
			}
			selectID = focused
		case tui.DashAttach:
			last, err := attachLoop(id)
			if err != nil {
				return err
			}
			selectID = last
		}
	}
}

// ensureDaemon starts `cb daemon` in the background if the socket isn't live.
// If a daemon is already running, it verifies the protocol version matches so a
// stale daemon left over from a rebuild fails loudly instead of silently
// dropping attach/input messages.
func ensureDaemon() error {
	if c, err := net.Dial("unix", ipc.SocketPath()); err == nil {
		c.Close()
		resp, err := ipc.Send(ipc.Request{Type: "ping"})
		if err == nil && resp.Version == ipc.ProtocolVersion {
			return nil
		}
		return fmt.Errorf("a stale cb daemon is running (protocol v%d, want v%d, pid %d).\n"+
			"restart it:  kill %d   (or: pkill -f 'cb daemon')   then rerun cb",
			resp.Version, ipc.ProtocolVersion, resp.PID, resp.PID)
	}
	exe, err := os.Executable()
	if err != nil {
		return err
	}
	cmd := exec.Command(exe, "daemon")
	// Route daemon logs to a file so they survive and can be inspected.
	_ = os.MkdirAll(ipc.Dir(), 0o700)
	if logf, err := os.OpenFile(filepath.Join(ipc.Dir(), "daemon.log"),
		os.O_CREATE|os.O_WRONLY|os.O_APPEND, 0o644); err == nil {
		cmd.Stdout, cmd.Stderr = logf, logf
	}
	if err := cmd.Start(); err != nil {
		return fmt.Errorf("starting daemon: %w", err)
	}
	_ = cmd.Process.Release()
	// Wait briefly for the socket to come up.
	for i := 0; i < 50; i++ {
		if c, err := net.Dial("unix", ipc.SocketPath()); err == nil {
			c.Close()
			return nil
		}
		time.Sleep(20 * time.Millisecond)
	}
	return fmt.Errorf("daemon did not become ready")
}

// runAttach attaches the interactive client to a session. With no id, it
// attaches to the only session (erroring if there are zero or several).
func runAttach(args []string) error {
	id := ""
	if len(args) > 0 {
		id = args[0]
	}
	if id == "" {
		resp, err := ipc.Send(ipc.Request{Type: "list"})
		if err != nil {
			return err
		}
		switch len(resp.Sessions) {
		case 0:
			return fmt.Errorf("no sessions; spawn one with: cb ctl spawn <cmd...>")
		case 1:
			id = resp.Sessions[0].ID
		default:
			return fmt.Errorf("multiple sessions; pass an id: cb attach <id>")
		}
	}
	_, err := attachLoop(id)
	return err
}

// attachLoop attaches to a session, following Ctrl-a g jumps to other sessions
// until the user detaches back to the dashboard. It returns the id of the last
// session the user was viewing, so the dashboard can highlight it.
func attachLoop(id string) (string, error) {
	last := id
	for id != "" {
		last = id
		next, err := tui.Attach(id)
		if err != nil {
			return last, err
		}
		id = next
	}
	return last, nil
}

// runDemo spawns a command under a session, drains it for a few seconds (or
// until it exits), then prints the rendered screen. Used to verify PTY +
// emulator plumbing and that large output bursts never block the child.
func runDemo(argv []string) error {
	if len(argv) == 0 {
		argv = []string{"bash", "-c", "seq 1 200000; echo DONE-BURST"}
	}
	cwd, _ := os.Getwd()
	s, err := session.New("demo", argv, cwd, 24, 80)
	if err != nil {
		return err
	}

	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		if s.Exited() {
			break
		}
		time.Sleep(100 * time.Millisecond)
	}

	fmt.Printf("--- exited=%v status=%s ---\n", s.Exited(), s.Status())
	fmt.Println(s.Render())
	return nil
}
