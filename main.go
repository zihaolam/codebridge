// Command cb is a TUI to manage many Claude Code sessions from one place.
//
// Subcommands:
//
//	cb               launch the TUI client (default)   [not yet implemented]
//	cb daemon        run the long-lived PTY/state server [not yet implemented]
//	cb hook <event>  no-op observer invoked by Claude Code hooks [phase 2]
//	cb install-hooks merge the hook block into ~/.claude/settings.json [phase 2]
//	cb demo <cmd...> spawn a command under a session and dump its screen (debug)
package main

import (
	"fmt"
	"os"

	"codebridge/internal/cli"
)

func main() {
	if err := cli.Run(os.Args[1:]); err != nil {
		fmt.Fprintln(os.Stderr, "cb:", err)
		os.Exit(1)
	}
}
