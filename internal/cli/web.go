package cli

import (
	"flag"
	"fmt"

	qrcode "github.com/skip2/go-qrcode"

	"codebridge/internal/web"
)

// runWeb drives the default daemon-owned bridge: reports its address, manages
// pairing tokens, and prints pairing QR codes. Supplying a non-default port
// still starts an extra bridge explicitly for the rare case that it is needed.
func runWeb(args []string) error {
	if len(args) > 0 {
		switch args[0] {
		case "token":
			if len(args) > 1 && args[1] == "rotate" {
				cfg, err := web.Rotate()
				if err != nil {
					return err
				}
				fmt.Println(cfg.Token)
				return nil
			}
			cfg, err := web.LoadOrCreate()
			if err != nil {
				return err
			}
			fmt.Println(cfg.Token)
			return nil
		case "qr":
			return runWebQR(args[1:])
		case "-h", "--help", "help":
			fmt.Println("usage: cb web [--port N] | cb web token [rotate] | cb web qr [--url URL]")
			return nil
		}
	}

	fs := flag.NewFlagSet("web", flag.ContinueOnError)
	port := fs.Int("port", web.DefaultPort, "port to listen on (127.0.0.1 only)")
	if err := fs.Parse(args); err != nil {
		return err
	}
	if err := ensureDaemon(); err != nil {
		return err
	}
	if *port == web.DefaultPort {
		fmt.Printf("cb web is running at http://127.0.0.1:%d\n", web.DefaultPort)
		return nil
	}
	return web.Run(*port)
}

// runWebQR prints a terminal QR code encoding the PWA URL with the token in
// the fragment (#token=...), which the client stores on first open. The
// fragment never leaves the browser, so the token stays out of server logs.
func runWebQR(args []string) error {
	fs := flag.NewFlagSet("web qr", flag.ContinueOnError)
	url := fs.String("url", fmt.Sprintf("http://127.0.0.1:%d", web.DefaultPort),
		"URL the phone should open (your tailscale serve HTTPS endpoint)")
	if err := fs.Parse(args); err != nil {
		return err
	}
	cfg, err := web.LoadOrCreate()
	if err != nil {
		return err
	}
	link := *url + "#token=" + cfg.Token
	qr, err := qrcode.New(link, qrcode.Medium)
	if err != nil {
		return err
	}
	fmt.Print(qr.ToSmallString(false))
	fmt.Println(link)
	return nil
}
