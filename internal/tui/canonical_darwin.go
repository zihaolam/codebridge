package tui

import (
	"syscall"
	"unsafe"
)

// fGetPath is the macOS fcntl op that asks the kernel for the canonical
// (case-preserved) path of an open file. Stable since 10.0.
const fGetPath = 50

// canonicalCase returns p with its case as recorded in the filesystem catalog.
// macOS APFS/HFS+ is case-insensitive but case-preserving, so os.Getwd and
// filepath.EvalSymlinks both faithfully echo whatever case the caller typed.
// Two paths to the same inode can therefore differ only in case — which breaks
// the scope key comparison. F_GETPATH on an open fd returns the on-disk case.
func canonicalCase(p string) string {
	fd, err := syscall.Open(p, syscall.O_RDONLY, 0)
	if err != nil {
		return p
	}
	defer syscall.Close(fd)
	var buf [1024]byte // PATH_MAX on macOS
	_, _, errno := syscall.Syscall(syscall.SYS_FCNTL, uintptr(fd), fGetPath, uintptr(unsafe.Pointer(&buf[0])))
	if errno != 0 {
		return p
	}
	n := 0
	for n < len(buf) && buf[n] != 0 {
		n++
	}
	return string(buf[:n])
}
