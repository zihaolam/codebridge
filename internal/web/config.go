// Package web is the mobile bridge: it talks to the daemon over its unix socket
// as a normal client (exactly like the TUI) and exposes a WebSocket + static-
// file HTTP server for the PWA. The CLI launches the default bridge alongside
// the daemon, while the daemon package remains web-agnostic.
//
// The server binds 127.0.0.1 only. Remote access is expected to go through
// `tailscale serve`, which fronts it tailnet-only with a real HTTPS cert.
package web

import (
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"

	"codebridge/internal/ipc"
)

// Config is the bridge's persisted state at ~/.cb/web.json.
type Config struct {
	// Token is the bearer token a client must present as its first WebSocket
	// message. The tailnet authenticates devices; the token authenticates the
	// client — it's what stops a random webpage on a tailnet device from
	// opening a socket to the bridge (WebSockets don't obey same-origin).
	Token string `json:"token"`
	// AllowedOrigins are extra Origin header patterns accepted on the WS
	// upgrade, e.g. a custom domain mirroring the PWA. Same-host origins
	// (the tailscale-serve hostname) are always accepted.
	AllowedOrigins []string `json:"allowed_origins,omitempty"`
}

func configPath() string {
	return filepath.Join(ipc.Dir(), "web.json")
}

// LoadOrCreate reads ~/.cb/web.json, generating it with a fresh token on
// first run.
func LoadOrCreate() (Config, error) {
	b, err := os.ReadFile(configPath())
	if err == nil {
		var cfg Config
		if err := json.Unmarshal(b, &cfg); err != nil {
			return Config{}, fmt.Errorf("parsing %s: %w", configPath(), err)
		}
		if cfg.Token != "" {
			return cfg, nil
		}
		// Fall through: config exists but has no token yet — mint one.
		return rotateInto(cfg)
	}
	if !os.IsNotExist(err) {
		return Config{}, err
	}
	return rotateInto(Config{})
}

// Rotate replaces the token with a fresh one and persists it. Existing
// clients must re-enter the new token.
func Rotate() (Config, error) {
	cfg, err := LoadOrCreate()
	if err != nil {
		return Config{}, err
	}
	return rotateInto(cfg)
}

func rotateInto(cfg Config) (Config, error) {
	cfg.Token = newToken()
	if err := save(cfg); err != nil {
		return Config{}, err
	}
	return cfg, nil
}

func save(cfg Config) error {
	if err := os.MkdirAll(ipc.Dir(), 0o700); err != nil {
		return err
	}
	b, err := json.MarshalIndent(cfg, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(configPath(), append(b, '\n'), 0o600)
}

func newToken() string {
	b := make([]byte, 32)
	if _, err := rand.Read(b); err != nil {
		panic(err) // crypto/rand failure is unrecoverable
	}
	return hex.EncodeToString(b)
}
