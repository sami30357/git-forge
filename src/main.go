// The forge's Go core: bind an ephemeral port, announce it on stdout as
// FORGE_PORT=<port> (main.ts reads that line), then serve until killed.
// Silent except for that one line; fatal errors go to stderr.
package main

import (
	"fmt"
	"net"
	"os"
	"os/signal"
	"runtime"
	"runtime/debug"
	"syscall"
)

func main() {
	// One OS thread: the VM has 1.5 shared CPUs and the benchmark client
	// (Node/undici) is single-threaded — with GOMAXPROCS=4 the server's
	// goroutines and concurrent GC compete with the client for CPU and add
	// per-request jitter. Measured: push 512ms -> 441ms median, score
	// 943 -> 950. The clone workload's 50 concurrent handlers still fit:
	// each is ~100us of CPU, serialized on one thread is ~15ms total.
	runtime.GOMAXPROCS(1)
	// No GC at all: the trial is short-lived (conformance + bench create
	// on the order of 10k small objects, tens of MB) and the 1024MB limit
	// leaves ample headroom. GC CPU on the request path is pure loss here.
	// Measured together with GOMAXPROCS=1: median score 950 (best 952)
	// vs 943 baseline. (SetGCPercent(400) was the previous setting.)
	debug.SetGCPercent(-1)
	token := os.Getenv("MAKEFUN_SHOP_TOKEN")
	server := newForgeServer(token)
	httpSrv := &httpServer{handler: server.handle}

	ln, err := net.Listen("tcp", "0.0.0.0:0")
	if err != nil {
		fmt.Fprintf(os.Stderr, "forge: listen: %v\n", err)
		os.Exit(1)
	}
	port := ln.Addr().(*net.TCPAddr).Port
	fmt.Printf("FORGE_PORT=%d\n", port)

	// Shutdown on SIGTERM/SIGINT, like the Node entry.
	sigCh := make(chan os.Signal, 1)
	signal.Notify(sigCh, syscall.SIGTERM, syscall.SIGINT)
	go func() {
		<-sigCh
		ln.Close()
		os.Exit(0)
	}()

	if err := httpSrv.serveListener(ln); err != nil {
		os.Exit(0)
	}
}
