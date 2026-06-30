// Command cb is a TUI to manage many Claude Code sessions from one place.
//
// Subcommands:
//
//	cb               launch the TUI client (default)   [not yet implemented]
//	cb daemon        run the long-lived PTY/state server [not yet implemented]
//	cb hook <event>  no-op observer invoked by Claude Code hooks [phase 2]
//	cb install-hooks merge the hook block into ~/.claude/settings.json [phase 2]
//	cb demo <cmd...> spawn a command under a session and dump its screen (debug)
//	cb version       print the build version
package main

import (
	"fmt"
	"os"

	"codebridge/internal/cli"
)

// Build metadata, injected at release time by goreleaser via
// -ldflags "-X main.version=... -X main.commit=... -X main.date=..."
// (see .goreleaser.yml). A plain `go build` leaves these at their defaults;
// cli.SetBuild forwards them and the version command falls back to the VCS
// stamp Go embeds in the binary.
var (
	version = "dev"
	commit  = ""
	date    = ""
)

func main() {
	cli.SetBuild(version, commit, date)
	if err := cli.Run(os.Args[1:]); err != nil {
		fmt.Fprintln(os.Stderr, "cb:", err)
		os.Exit(1)
	}
}
