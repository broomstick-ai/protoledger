//! `protoledger serve`: a store over HTTP and a WebSocket (see `docs/SERVE.md`).
//!
//! - `GET /v1/subscribe?from=<agent>:<seq>,...|all` (WebSocket) streams the
//!   store's [`Published`](crate::store::Published) events as JSON text
//!   frames, per agent in append order, each starting strictly after the seq
//!   given (SPEC §9.4: `(agent, last seq seen)` is the whole cursor).
//! - `GET /v1/agents`, `/v1/agents/{id}`, `/v1/agents/{id}/events`,
//!   `/v1/agents/{id}/children`, `/v1/bindings`; `POST /v1/import`,
//!   `/v1/project`, `/v1/attach`, `/v1/unbind`.
//!
//! **One owner.** The [`Store`](crate::Store) is not `Sync` and its writers
//! hold the ledgers' locks, so it lives on one OS thread, the *engine*, which
//! also runs the sync engine ([`crate::sync::Follower`]) for every active
//! binding. HTTP handlers send the engine a closure and await its answer.
//! The engine holds the writer lock of every ledger it appends to (locks are
//! per ledger), so project, attach and unbind are performed by the lock
//! holder: the case in which `protoledger unbind` from another process is
//! refused as locked.
//!
//! **Loopback only, by default.** There is no authentication: anyone who can
//! reach the port can read every ledger and import, project, attach and
//! unbind. [`Server::start`] refuses an address that is not loopback unless
//! [`ServeOptions::allow_remote`] is set.
//!
//! **Events out.** The engine holds one live subscription on the store (the
//! *hub*, [`Store::subscribe`](crate::Store::subscribe)) and fans each
//! published event out to the WebSocket clients. A client's events before
//! the live point are *backfilled* from the ledger files themselves (they are
//! append-only, so a reader needs no lock): see the private `engine` module for why the seam
//! between the two has no hole and no duplicate.
//!
//! **Why axum on tokio.** axum gives routing, JSON and the WebSocket upgrade
//! (through tokio-tungstenite) in one dependency on the runtime it needs
//! anyway; hyper + tokio-tungstenite by hand would be the same crates with
//! the routing and upgrade plumbing written here. Both live behind the
//! `serve` feature: without it the library pulls in no tokio.

mod engine;
mod http;

use crate::sync::FollowOptions;
use anyhow::{Context, Result, anyhow};
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// The resume rule, in one place: an event at `seq` is sent to a client whose
/// cursor is `from` iff it comes strictly after it. Every path that starts
/// from a client's seq (the WebSocket's backfill and live floor, and
/// `/v1/agents/{id}/events`) decides with this function and nothing else.
pub(crate) fn after(from: u64, seq: u64) -> bool {
    seq > from
}

/// How `serve` follows the store's bindings, and where it may listen.
#[derive(Debug, Clone, Copy, Default)]
pub struct ServeOptions {
    pub follow: FollowOptions,
    /// Listen on an address that is not loopback. Off by default: `serve`
    /// has no authentication.
    pub allow_remote: bool,
}

/// Resolve `--listen`: an `ip:port` (`[::1]:7417` for IPv6) or a
/// `host:port` such as `localhost:7417`, choosing among the addresses a host
/// resolves to with [`pick_listen`]; whether the address may be used at all
/// is [`check_listen`]'s call.
pub fn resolve_listen(listen: &str, allow_remote: bool) -> Result<SocketAddr> {
    let addrs: Vec<SocketAddr> = listen.to_socket_addrs().with_context(|| format!("--listen {listen:?}: not an address (ip:port or host:port)"))?.collect();
    pick_listen(&addrs, allow_remote).ok_or_else(|| anyhow!("--listen {listen:?} resolves to no address"))
}

/// Which of the addresses `--listen` resolved to (in the resolver's order) to
/// listen on. Without `allow_remote`, the first loopback one if there is one
/// (so `localhost` is served wherever it resolves); with it, the first one,
/// loopback or not: `--allow-remote` listens where `--listen` says, with no
/// preference of protoledger's (docs/SERVE.md).
pub fn pick_listen(addrs: &[SocketAddr], allow_remote: bool) -> Option<SocketAddr> {
    if allow_remote {
        return addrs.first().copied();
    }
    addrs.iter().find(|a| a.ip().is_loopback()).or(addrs.first()).copied()
}

/// Refuse to listen on an address that is not loopback unless remote
/// listening was asked for: `serve` has no authentication.
pub fn check_listen(addr: SocketAddr, allow_remote: bool) -> Result<()> {
    if allow_remote || addr.ip().is_loopback() {
        return Ok(());
    }
    Err(anyhow!(
        "refusing to listen on {addr}: it is not a loopback address, and serve has no authentication \
         (anyone who can reach it can read every ledger and import, project, attach and unbind); \
         listen on 127.0.0.1 or [::1], or pass --allow-remote to listen there anyway"
    ))
}

/// A running server. Dropping it stops it.
pub struct Server {
    addr: SocketAddr,
    rt: Option<tokio::runtime::Runtime>,
    task: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
    engine_done: Option<tokio::sync::oneshot::Receiver<Result<()>>>,
    engine: Option<std::thread::JoinHandle<()>>,
    stop: Arc<AtomicBool>,
}

impl Server {
    /// Open the store at `root` (on the engine thread), bring every active
    /// binding up to date, start following them, and listen on `listen`
    /// (port 0 picks one; see [`Server::addr`]).
    ///
    /// Refuses a `listen` that is not loopback unless `opts.allow_remote`
    /// ([`check_listen`]), before it opens anything.
    pub fn start(root: impl AsRef<Path>, listen: SocketAddr, opts: ServeOptions) -> Result<Server> {
        check_listen(listen, opts.allow_remote)?;
        let root: PathBuf = root.as_ref().to_path_buf();
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (jobs_tx, jobs_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let stop2 = stop.clone();
        let engine = std::thread::Builder::new()
            .name("protoledger-serve-engine".into())
            .spawn(move || {
                let r = engine::run(&root, opts.follow, jobs_rx, &stop2, ready_tx);
                let _ = done_tx.send(r);
            })
            .context("starting the engine thread")?;
        let stopper = |e: anyhow::Error, engine: std::thread::JoinHandle<()>| {
            stop.store(true, Ordering::SeqCst);
            let _ = engine.join();
            e
        };
        match ready_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(stopper(e, engine)),
            Err(_) => return Err(stopper(anyhow!("the engine thread died while opening the store"), engine)),
        }
        let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().thread_name("protoledger-serve").build() {
            Ok(rt) => rt,
            Err(e) => return Err(stopper(e.into(), engine)),
        };
        let listener = match rt.block_on(tokio::net::TcpListener::bind(listen)) {
            Ok(l) => l,
            Err(e) => return Err(stopper(anyhow!(e).context(format!("listening on {listen}")), engine)),
        };
        let addr = listener.local_addr()?;
        let app = http::router(engine::Handle::new(jobs_tx));
        let task = rt.spawn(async move { axum::serve(listener, app).await });
        Ok(Server { addr, rt: Some(rt), task: Some(task), engine_done: Some(done_rx), engine: Some(engine), stop })
    }

    /// The address the server listens on.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Block until the server stops: the listener fails or the engine (the
    /// store's owner) ends. It does not stop on its own otherwise.
    pub fn wait(mut self) -> Result<()> {
        let rt = self.rt.as_ref().expect("running");
        let task = self.task.take().expect("running");
        let done = self.engine_done.take().expect("running");
        let r = rt.block_on(async move {
            tokio::select! {
                r = task => match r {
                    Ok(Ok(())) => Err(anyhow!("the listener stopped")),
                    Ok(Err(e)) => Err(anyhow!(e).context("the listener failed")),
                    Err(e) => Err(anyhow!(e).context("the listener panicked")),
                },
                r = done => match r {
                    Ok(Ok(())) => Err(anyhow!("the engine stopped")),
                    Ok(Err(e)) => Err(e.context("the engine failed")),
                    Err(_) => Err(anyhow!("the engine thread panicked")),
                },
            }
        });
        self.shutdown_inner();
        r
    }

    /// Stop listening, close every connection, and stop the engine (dropping
    /// the store, which releases its writer locks).
    pub fn shutdown(mut self) {
        self.shutdown_inner();
    }

    fn shutdown_inner(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.task.take() {
            t.abort();
        }
        if let Some(rt) = self.rt.take() {
            rt.shutdown_timeout(Duration::from_secs(2));
        }
        if let Some(e) = self.engine.take() {
            let _ = e.join();
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shutdown_inner();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    /// A host name resolving to a LAN address first and loopback second: with
    /// --allow-remote it listens on the first (where --listen says), without
    /// it on the loopback one. Opposed by the reverse order and by a name
    /// resolving to loopback only.
    #[test]
    fn allow_remote_listens_on_the_first_resolved_address_and_loopback_only_prefers_loopback() {
        let lan_first = [a("10.0.0.169:7417"), a("[::1]:7417")];
        assert_eq!(pick_listen(&lan_first, true), Some(a("10.0.0.169:7417")), "--allow-remote preferred loopback");
        assert_eq!(pick_listen(&lan_first, false), Some(a("[::1]:7417")));
        let loopback_first = [a("[::1]:7417"), a("10.0.0.169:7417")];
        assert_eq!(pick_listen(&loopback_first, true), Some(a("[::1]:7417")));
        assert_eq!(pick_listen(&loopback_first, false), Some(a("[::1]:7417")));
        let lan_only = [a("10.0.0.169:7417")];
        assert_eq!(pick_listen(&lan_only, false), Some(a("10.0.0.169:7417")), "check_listen refuses it, not pick_listen");
        assert!(check_listen(pick_listen(&lan_only, false).unwrap(), false).is_err());
        assert_eq!(pick_listen(&[], true), None);
    }
}
