package web

import (
	"context"
	"embed"
	"fmt"
	"io/fs"
	"net/http"
	"path"
	"strings"
	"time"
)

// distFS holds the built PWA (vite output). dist/ is gitignored apart from
// .gitkeep so `go build` works before the frontend has ever been built; in
// that state the server falls back to a "UI not built" page.
//
//go:embed all:dist
var distFS embed.FS

// DefaultPort is where the daemon-owned bridge listens on 127.0.0.1.
const DefaultPort = 8899

// Server is the bridge: static PWA + /ws, one poller shared by all clients.
type Server struct {
	cfg         Config
	poller      *poller
	authTimeout time.Duration
}

func NewServer(cfg Config) *Server {
	return &Server{cfg: cfg, poller: newPoller(), authTimeout: 5 * time.Second}
}

// Start launches the shared list poller; it stops when ctx is cancelled.
func (s *Server) Start(ctx context.Context) {
	go s.poller.run(ctx)
}

// Handler returns the bridge's HTTP handler (also used by tests directly).
func (s *Server) Handler() http.Handler {
	mux := http.NewServeMux()
	mux.HandleFunc("/ws", s.handleWS)
	mux.Handle("/", staticHandler())
	return mux
}

// Run serves the bridge on 127.0.0.1:port until the process exits. The default
// instance is launched alongside `cb daemon`; callers may use another port for
// an explicitly requested extra bridge.
func Run(port int) error {
	cfg, err := LoadOrCreate()
	if err != nil {
		return err
	}
	s := NewServer(cfg)
	s.Start(context.Background())

	addr := fmt.Sprintf("127.0.0.1:%d", port)
	fmt.Printf("cb web listening on http://%s\n", addr)
	fmt.Printf("  expose on your tailnet:  tailscale serve --bg %d\n", port)
	fmt.Printf("  pairing token:           cb web token   (or: cb web qr --url <https url>)\n")
	return http.ListenAndServe(addr, s.Handler())
}

// staticHandler serves the embedded PWA with an SPA fallback: any path
// without a file extension falls back to index.html so client-side routes
// deep-link correctly.
func staticHandler() http.Handler {
	sub, err := fs.Sub(distFS, "dist")
	if err != nil {
		panic(err) // embed layout is fixed at compile time
	}
	files := http.FileServer(http.FS(sub))
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		p := strings.TrimPrefix(path.Clean(r.URL.Path), "/")
		if p == "" {
			p = "index.html"
		}
		// The embedded FS carries no modtimes, so without explicit headers
		// browsers heuristically cache everything — including index.html,
		// which then pins users to a stale bundle. Vite's hashed assets are
		// immutable; everything else must revalidate.
		if strings.HasPrefix(p, "assets/") {
			w.Header().Set("Cache-Control", "public, max-age=31536000, immutable")
		} else {
			w.Header().Set("Cache-Control", "no-cache")
		}
		if _, err := fs.Stat(sub, p); err != nil {
			if path.Ext(p) == "" {
				if _, err := fs.Stat(sub, "index.html"); err == nil {
					r.URL.Path = "/"
					files.ServeHTTP(w, r)
					return
				}
			}
			if p == "index.html" || path.Ext(p) == "" {
				w.Header().Set("Content-Type", "text/html; charset=utf-8")
				fmt.Fprint(w, "<!doctype html><h1>cb web</h1><p>UI not built. Run <code>npm run build</code> in <code>web/</code>, then rebuild cb.</p>")
				return
			}
			http.NotFound(w, r)
			return
		}
		files.ServeHTTP(w, r)
	})
}
