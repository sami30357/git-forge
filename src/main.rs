// The forge's Rust core: bind an ephemeral port, announce it on stdout as
// FORGE_PORT=<port> (main.ts reads that line), then serve until killed.
// One OS thread per connection; the store is lock-free for reads (RCU
// snapshots), so the 50 concurrent clones of the benchmark never queue on a
// single mutex. Silent except for that one line; fatal errors go to stderr.

mod flate;
mod git;
mod gitobj;
mod graphql;
mod http;
mod json;
mod mcp;
mod rest;
mod server;
mod sha1;
mod store;
mod util;

use server::Forge;
use std::io::Write;
use std::sync::Arc;

fn main() {
    let token = std::env::var("MAKEFUN_SHOP_TOKEN").unwrap_or_default();
    let forge = Arc::new(Forge::new(token));
    let (listener, port) = match http::listen() {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("forge: listen: {}", e);
            std::process::exit(1);
        }
    };
    // The line main.ts waits for; flush explicitly (stdout is piped).
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    if writeln!(out, "FORGE_PORT={}", port).is_err() || out.flush().is_err() {
        std::process::exit(1);
    }
    drop(out);
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let forge = forge.clone();
                std::thread::spawn(move || {
                    http::serve_conn(stream, move |req| forge.handle(req));
                });
            }
            Err(_) => continue,
        }
    }
}
