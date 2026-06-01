//go:build !darwin

package tui

// canonicalCase is a no-op on systems with case-sensitive filesystems (Linux's
// default). Windows would benefit from a GetFinalPathNameByHandleW-based impl;
// not implemented until someone runs cb there.
func canonicalCase(p string) string { return p }
