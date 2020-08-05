//! A generic connection pool, designed for asynchronous tokio-based connections
//! This is an asynchronous tokio-based version of r2d2.
//!
//! Opening a new database connection every time one is needed is both
//! inefficient and can lead to resource exhaustion under high traffic
//! conditions. A connection pool maintains a set of open connections to a
//! database, handing them out for repeated use.
//!
//! bb8 is agnostic to the connection type it is managing. Implementors of the
//! `ManageConnection` trait provide the database-specific logic to create and
//! check the health of connections.
//!
//! # Example
//!
//! Using an imaginary "foodb" database.
//!
//! ```ignore
//! use bb8;
//! use bb8_foodb;
//!
//! #[tokio::main]
//! async fn main() {
//!     let manager = bb8_foodb::FooConnectionManager::new("localhost:1234");
//!     let pool = bb8::Pool::builder().build(manager).await.unwrap();
//!
//!     for _ in 0..20 {
//!         let pool = pool.clone();
//!         tokio::spawn(async move {
//!             let conn = pool.get().await.unwrap();
//!             // use the connection
//!             // it will be returned to the pool when it falls out of scope.
//!         });
//!     }
//! }
//! ```
#![allow(clippy::needless_doctest_main)]
#![deny(missing_docs, missing_debug_implementations)]

use std::cmp::{max, min};
use std::collections::VecDeque;
use std::error;
use std::fmt;
use std::marker::PhantomData;
use std::mem;
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::channel::oneshot;
use futures::future::ok;
use futures::prelude::*;
use futures::stream::FuturesUnordered;
use tokio::spawn;
use tokio::sync::{Mutex, MutexGuard};
use tokio::time::{delay_for, interval_at, timeout, Interval};

/// A trait which provides connection-specific functionality.
#[async_trait]
pub trait ManageConnection: Send + Sync + 'static {
    /// The connection type this manager deals with.
    type Connection: Send + 'static;
    /// The error type returned by `Connection`s.
    type Error: std::error::Error + Send + 'static;

    /// Attempts to create a new connection.
    async fn connect(&self) -> Result<Self::Connection, Self::Error>;
    /// Determines if the connection is still connected to the database.
    async fn is_valid(&self, conn: &mut Self::Connection) -> Result<(), Self::Error>;
    /// Synchronously determine if the connection is no longer usable, if possible.
    fn has_broken(&self, conn: &mut Self::Connection) -> bool;
}

/// bb8's error type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunError<E> {
    /// An error returned from user code.
    User(E),
    /// bb8 attempted to get a connection but the provided timeout was exceeded.
    TimedOut,
}

impl<E> fmt::Display for RunError<E>
where
    E: error::Error + 'static,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            RunError::User(ref err) => write!(f, "{}", err),
            RunError::TimedOut => write!(f, "Timed out in bb8"),
        }
    }
}

impl<E> error::Error for RunError<E>
where
    E: error::Error + 'static,
{
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match *self {
            RunError::User(ref err) => Some(err),
            RunError::TimedOut => None,
        }
    }
}

impl<E> From<E> for RunError<E>
where
    E: error::Error,
{
    fn from(error: E) -> Self {
        Self::User(error)
    }
}

/// A trait to receive errors generated by connection management that aren't
/// tied to any particular caller.
pub trait ErrorSink<E>: fmt::Debug + Send + Sync + 'static {
    /// Receive an error
    fn sink(&self, error: E);

    /// Clone this sink.
    fn boxed_clone(&self) -> Box<dyn ErrorSink<E>>;
}

/// An `ErrorSink` implementation that does nothing.
#[derive(Debug, Clone, Copy)]
pub struct NopErrorSink;

impl<E> ErrorSink<E> for NopErrorSink {
    fn sink(&self, _: E) {}

    fn boxed_clone(&self) -> Box<dyn ErrorSink<E>> {
        Box::new(*self)
    }
}

/// Information about the state of a `Pool`.
pub struct State {
    /// The number of connections currently being managed by the pool.
    pub connections: u32,
    /// The number of idle connections.
    pub idle_connections: u32,
    _p: (),
}

impl fmt::Debug for State {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_struct("State")
            .field("connections", &self.connections)
            .field("idle_connections", &self.idle_connections)
            .finish()
    }
}

#[derive(Debug)]
struct Conn<C>
where
    C: Send,
{
    conn: C,
    birth: Instant,
}

struct IdleConn<C>
where
    C: Send,
{
    conn: Conn<C>,
    idle_start: Instant,
}

impl<C> IdleConn<C>
where
    C: Send,
{
    fn make_idle(conn: Conn<C>) -> IdleConn<C> {
        let now = Instant::now();
        IdleConn {
            conn,
            idle_start: now,
        }
    }
}

/// A builder for a connection pool.
#[derive(Debug)]
pub struct Builder<M: ManageConnection> {
    /// The maximum number of connections allowed.
    max_size: u32,
    /// The minimum idle connection count the pool will attempt to maintain.
    min_idle: Option<u32>,
    /// Whether or not to test the connection on checkout.
    test_on_check_out: bool,
    /// The maximum lifetime, if any, that a connection is allowed.
    max_lifetime: Option<Duration>,
    /// The duration, if any, after which idle_connections in excess of `min_idle` are closed.
    idle_timeout: Option<Duration>,
    /// The duration to wait to start a connection before giving up.
    connection_timeout: Duration,
    /// The error sink.
    error_sink: Box<dyn ErrorSink<M::Error>>,
    /// The time interval used to wake up and reap connections.
    reaper_rate: Duration,
    _p: PhantomData<M>,
}

impl<M: ManageConnection> Default for Builder<M> {
    fn default() -> Self {
        Builder {
            max_size: 10,
            min_idle: None,
            test_on_check_out: true,
            max_lifetime: Some(Duration::from_secs(30 * 60)),
            idle_timeout: Some(Duration::from_secs(10 * 60)),
            connection_timeout: Duration::from_secs(30),
            error_sink: Box::new(NopErrorSink),
            reaper_rate: Duration::from_secs(30),
            _p: PhantomData,
        }
    }
}

impl<M: ManageConnection> Builder<M> {
    /// Constructs a new `Builder`.
    ///
    /// Parameters are initialized with their default values.
    pub fn new() -> Builder<M> {
        Default::default()
    }

    /// Sets the maximum number of connections managed by the pool.
    ///
    /// Defaults to 10.
    pub fn max_size(mut self, max_size: u32) -> Builder<M> {
        assert!(max_size > 0, "max_size must be greater than zero!");
        self.max_size = max_size;
        self
    }

    /// Sets the minimum idle connection count maintained by the pool.
    ///
    /// If set, the pool will try to maintain at least this many idle
    /// connections at all times, while respecting the value of `max_size`.
    ///
    /// Defaults to None.
    pub fn min_idle(mut self, min_idle: Option<u32>) -> Builder<M> {
        self.min_idle = min_idle;
        self
    }

    /// If true, the health of a connection will be verified through a call to
    /// `ManageConnection::is_valid` before it is provided to a pool user.
    ///
    /// Defaults to true.
    pub fn test_on_check_out(mut self, test_on_check_out: bool) -> Builder<M> {
        self.test_on_check_out = test_on_check_out;
        self
    }

    /// Sets the maximum lifetime of connections in the pool.
    ///
    /// If set, connections will be closed at the next reaping after surviving
    /// past this duration.
    ///
    /// If a connection reachs its maximum lifetime while checked out it will be
    /// closed when it is returned to the pool.
    ///
    /// Defaults to 30 minutes.
    pub fn max_lifetime(mut self, max_lifetime: Option<Duration>) -> Builder<M> {
        assert!(
            max_lifetime != Some(Duration::from_secs(0)),
            "max_lifetime must be greater than zero!"
        );
        self.max_lifetime = max_lifetime;
        self
    }

    /// Sets the idle timeout used by the pool.
    ///
    /// If set, idle connections in excess of `min_idle` will be closed at the
    /// next reaping after remaining idle past this duration.
    ///
    /// Defaults to 10 minutes.
    pub fn idle_timeout(mut self, idle_timeout: Option<Duration>) -> Builder<M> {
        assert!(
            idle_timeout != Some(Duration::from_secs(0)),
            "idle_timeout must be greater than zero!"
        );
        self.idle_timeout = idle_timeout;
        self
    }

    /// Sets the connection timeout used by the pool.
    ///
    /// Futures returned by `Pool::get` will wait this long before giving up and
    /// resolving with an error.
    ///
    /// Defaults to 30 seconds.
    pub fn connection_timeout(mut self, connection_timeout: Duration) -> Builder<M> {
        assert!(
            connection_timeout > Duration::from_secs(0),
            "connection_timeout must be non-zero"
        );
        self.connection_timeout = connection_timeout;
        self
    }

    /// Set the sink for errors that are not associated with any particular operation
    /// on the pool. This can be used to log and monitor failures.
    ///
    /// Defaults to `NopErrorSink`.
    pub fn error_sink(mut self, error_sink: Box<dyn ErrorSink<M::Error>>) -> Builder<M> {
        self.error_sink = error_sink;
        self
    }

    /// Used by tests
    #[allow(dead_code)]
    pub fn reaper_rate(mut self, reaper_rate: Duration) -> Builder<M> {
        self.reaper_rate = reaper_rate;
        self
    }

    fn build_inner(self, manager: M) -> Pool<M> {
        if let Some(min_idle) = self.min_idle {
            assert!(
                self.max_size >= min_idle,
                "min_idle must be no larger than max_size"
            );
        }

        Pool::new_inner(self, manager)
    }

    /// Consumes the builder, returning a new, initialized `Pool`.
    ///
    /// The `Pool` will not be returned until it has established its configured
    /// minimum number of connections, or it times out.
    pub async fn build(self, manager: M) -> Result<Pool<M>, M::Error> {
        let pool = self.build_inner(manager);
        pool.replenish_idle_connections().await.map(|()| pool)
    }

    /// Consumes the builder, returning a new, initialized `Pool`.
    ///
    /// Unlike `build`, this does not wait for any connections to be established
    /// before returning.
    pub fn build_unchecked(self, manager: M) -> Pool<M> {
        let p = self.build_inner(manager);
        p.clone().spawn_replenishing();
        p
    }
}

/// The pool data that must be protected by a lock.
#[allow(missing_debug_implementations)]
struct PoolInternals<C>
where
    C: Send,
{
    waiters: VecDeque<oneshot::Sender<Conn<C>>>,
    conns: VecDeque<IdleConn<C>>,
    num_conns: u32,
    pending_conns: u32,
}

impl<C> PoolInternals<C>
where
    C: Send,
{
    fn put_idle_conn(&mut self, mut conn: IdleConn<C>) {
        loop {
            if let Some(waiter) = self.waiters.pop_front() {
                // This connection is no longer idle, send it back out.
                match waiter.send(conn.conn) {
                    Ok(_) => break,
                    // Oops, that receiver was gone. Loop and try again.
                    Err(c) => conn.conn = c,
                }
            } else {
                // Queue it in the idle queue.
                self.conns.push_back(conn);
                break;
            }
        }
    }
}

/// The guts of a `Pool`.
#[allow(missing_debug_implementations)]
struct SharedPool<M>
where
    M: ManageConnection + Send,
{
    statics: Builder<M>,
    manager: M,
    internals: Mutex<PoolInternals<M::Connection>>,
}

impl<M> SharedPool<M>
where
    M: ManageConnection,
{
    async fn sink_error<'a, E, F, T>(&self, f: F) -> Result<T, ()>
    where
        F: Future<Output = Result<T, E>> + Send + 'a,
        E: Into<M::Error>,
    {
        let sink = self.statics.error_sink.boxed_clone();
        f.await.map_err(|e| sink.sink(e.into()))
    }

    async fn or_timeout<'a, E, F, T>(&self, f: F) -> Result<Option<T>, E>
    where
        F: Future<Output = Result<T, E>> + Send + 'a,
        T: Send + 'a,
        E: Send + ::std::fmt::Debug + 'a,
    {
        timeout(self.statics.connection_timeout, f)
            .map(|r| match r {
                Ok(Ok(item)) => Ok(Some(item)),
                Ok(Err(e)) => Err(e),
                Err(_) => Ok(None),
            })
            .await
    }
}

/// A generic connection pool.
pub struct Pool<M>
where
    M: ManageConnection,
{
    inner: Arc<SharedPool<M>>,
}

impl<M> Clone for Pool<M>
where
    M: ManageConnection,
{
    fn clone(&self) -> Self {
        Pool {
            inner: self.inner.clone(),
        }
    }
}

impl<M> fmt::Debug for Pool<M>
where
    M: ManageConnection,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_fmt(format_args!("Pool({:p})", self.inner))
    }
}

// Outside of Pool to avoid borrow splitting issues on self
async fn add_connection<M>(pool: Arc<SharedPool<M>>) -> Result<(), M::Error>
where
    M: ManageConnection,
{
    let mut internals = pool.internals.lock().await;
    if internals.num_conns + internals.pending_conns >= pool.statics.max_size {
        return Ok(());
    }

    internals.pending_conns += 1;
    mem::drop(internals);

    let new_shared = Arc::downgrade(&pool);
    let shared = match new_shared.upgrade() {
        None => return Ok(()),
        Some(shared) => shared,
    };

    let start = Instant::now();
    let mut delay = Duration::from_secs(0);
    loop {
        match shared.manager.connect().await {
            Ok(conn) => {
                let now = Instant::now();
                let conn = IdleConn {
                    conn: Conn { conn, birth: now },
                    idle_start: now,
                };

                let mut locked = shared.internals.lock().await;
                locked.pending_conns -= 1;
                locked.num_conns += 1;
                locked.put_idle_conn(conn);
                return Ok(());
            }
            Err(e) => {
                if Instant::now() - start > pool.statics.connection_timeout {
                    let mut locked = shared.internals.lock().await;
                    locked.pending_conns -= 1;
                    return Err(e);
                } else {
                    delay = max(Duration::from_millis(200), delay);
                    delay = min(pool.statics.connection_timeout / 2, delay * 2);
                    delay_for(delay).await;
                }
            }
        }
    }
}

// Drop connections
// NB: This is called with the pool lock held.
fn drop_connections<'a, M>(
    pool: &Arc<SharedPool<M>>,
    internals: &mut MutexGuard<'a, PoolInternals<M::Connection>>,
    dropped: usize,
) where
    M: ManageConnection,
{
    internals.num_conns -= dropped as u32;
    // We might need to spin up more connections to maintain the idle limit, e.g.
    // if we hit connection lifetime limits
    if internals.num_conns + internals.pending_conns < pool.statics.max_size {
        Pool {
            inner: pool.clone(),
        }
        .spawn_replenishing();
    }
}

fn schedule_reaping<M>(mut interval: Interval, weak_shared: Weak<SharedPool<M>>)
where
    M: ManageConnection,
{
    spawn(async move {
        loop {
            let _ = interval.tick().await;
            if let Some(pool) = weak_shared.upgrade() {
                let mut internals = pool.internals.lock().await;
                let now = Instant::now();
                let before = internals.conns.len();

                internals.conns.retain(|conn| {
                    let mut keep = true;
                    if let Some(timeout) = pool.statics.idle_timeout {
                        keep &= now - conn.idle_start < timeout;
                    }
                    if let Some(lifetime) = pool.statics.max_lifetime {
                        keep &= now - conn.conn.birth < lifetime;
                    }
                    keep
                });

                let dropped = before - internals.conns.len();
                drop_connections(&pool, &mut internals, dropped);
            } else {
                break;
            }
        }
    });
}

impl<M: ManageConnection> Pool<M> {
    fn new_inner(builder: Builder<M>, manager: M) -> Pool<M> {
        let internals = PoolInternals {
            waiters: VecDeque::new(),
            conns: VecDeque::new(),
            num_conns: 0,
            pending_conns: 0,
        };

        let shared = Arc::new(SharedPool {
            statics: builder,
            manager,
            internals: Mutex::new(internals),
        });

        if shared.statics.max_lifetime.is_some() || shared.statics.idle_timeout.is_some() {
            let s = Arc::downgrade(&shared);
            if let Some(shared) = s.upgrade() {
                let start = Instant::now() + shared.statics.reaper_rate;
                let interval = interval_at(start.into(), shared.statics.reaper_rate);
                schedule_reaping(interval, s);
            }
        }

        Pool { inner: shared }
    }

    async fn sink_error<'a, E, F, T>(&self, f: F) -> Result<T, ()>
    where
        F: Future<Output = Result<T, E>> + Send + 'a,
        E: Into<M::Error> + 'a,
        T: 'a,
    {
        self.inner.sink_error(f).await
    }

    async fn replenish_idle_connections(&self) -> Result<(), M::Error> {
        let internals = self.inner.internals.lock().await;
        let pool = self.inner.clone();
        let slots_available = pool.statics.max_size - internals.num_conns - internals.pending_conns;
        let idle = internals.conns.len() as u32;
        let desired = pool.statics.min_idle.unwrap_or(0);

        mem::drop(internals);

        let stream = FuturesUnordered::new();
        for _ in idle..max(idle, min(desired, idle + slots_available)) {
            stream.push(add_connection(pool.clone()));
        }

        stream.try_fold((), |_, _| ok(())).await
    }

    fn spawn_replenishing(self) {
        spawn(async move {
            let f = self.replenish_idle_connections();
            self.sink_error(f).map(|_| ()).await
        });
    }

    /// Returns a `Builder` instance to configure a new pool.
    pub fn builder() -> Builder<M> {
        Builder::new()
    }

    /// Returns information about the current state of the pool.
    pub fn state(&self) -> State {
        let locked = loop {
            if let Ok(internals) = self.inner.internals.try_lock() {
                break internals;
            }
        };

        State {
            connections: locked.num_conns,
            idle_connections: locked.conns.len() as u32,
            _p: (),
        }
    }

    /// Run a closure with a `Connection`.
    pub async fn run<'a, T, E, U, F>(&self, f: F) -> Result<T, RunError<E>>
    where
        F: FnOnce(M::Connection) -> U + Send + 'a,
        U: Future<Output = Result<(T, M::Connection), (E, M::Connection)>> + Send + 'a,
        E: From<M::Error> + Send + 'a,
        T: Send + 'a,
    {
        let conn = match self.get_conn::<E>().await {
            Ok(conn) => conn,
            Err(e) => return Err(e),
        };

        let birth = conn.birth;
        let (r, conn): (Result<_, E>, _) = match f(conn.conn).await {
            Ok((t, conn)) => (Ok(t), conn),
            Err((e, conn)) => (Err(e), conn),
        };

        self.put_back(birth, conn).await;

        r.map_err(RunError::User)
    }

    /// Return connection back in to the pool
    async fn put_back(&self, birth: Instant, mut conn: M::Connection) {
        let inner = self.inner.clone();

        // Supposed to be fast, but do it before locking anyways.
        let broken = inner.manager.has_broken(&mut conn);

        let mut locked = inner.internals.lock().await;
        if broken {
            drop_connections(&inner, &mut locked, 1);
        } else {
            let conn = IdleConn::make_idle(Conn { conn, birth });
            locked.put_idle_conn(conn);
        }
    }

    async fn get_conn<E>(&self) -> Result<Conn<M::Connection>, RunError<E>> {
        let inner = self.inner.clone();

        loop {
            let mut internals = inner.internals.lock().await;
            if let Some(conn) = internals.conns.pop_front() {
                // Spin up a new connection if necessary to retain our minimum idle count
                if internals.num_conns + internals.pending_conns < inner.statics.max_size {
                    Pool {
                        inner: inner.clone(),
                    }
                    .spawn_replenishing();
                }

                if inner.statics.test_on_check_out {
                    let (mut conn, birth) = (conn.conn.conn, conn.conn.birth);

                    match inner.manager.is_valid(&mut conn).await {
                        Ok(()) => return Ok(Conn { conn, birth }),
                        Err(_) => {
                            mem::drop(conn);
                            drop_connections(&inner, &mut internals, 1);
                        }
                    }
                    continue;
                } else {
                    return Ok(conn.conn);
                }
            } else {
                break;
            }
        }

        let (tx, rx) = oneshot::channel();
        {
            let mut locked = inner.internals.lock().await;
            locked.waiters.push_back(tx);
            if locked.num_conns + locked.pending_conns < inner.statics.max_size {
                let inner = inner.clone();
                spawn(async move {
                    let f = add_connection(inner.clone());
                    inner.sink_error(f).map(|_| ()).await;
                });
            }
        }

        match inner.or_timeout(rx).await {
            Ok(Some(conn)) => Ok(conn),
            _ => Err(RunError::TimedOut),
        }
    }

    /// Retrieves a connection from the pool.
    pub async fn get(&self) -> Result<PooledConnection<'_, M>, RunError<M::Error>> {
        self.get_conn::<M::Error>()
            .map(move |res| {
                res.map(|conn| PooledConnection {
                    pool: self,
                    checkout: Instant::now(),
                    conn: Some(conn),
                })
            })
            .await
    }

    /// Get a new dedicated connection that will not be managed by the pool.
    /// An application may want a persistent connection (e.g. to do a
    /// postgres LISTEN) that will not be closed or repurposed by the pool.
    ///
    /// This method allows reusing the manager's configuration but otherwise
    /// bypassing the pool
    pub async fn dedicated_connection(&self) -> Result<M::Connection, M::Error> {
        let inner = self.inner.clone();
        inner.manager.connect().await
    }
}

/// A smart pointer wrapping a connection.
pub struct PooledConnection<'a, M>
where
    M: ManageConnection,
{
    pool: &'a Pool<M>,
    checkout: Instant,
    conn: Option<Conn<M::Connection>>,
}

impl<'a, M> Deref for PooledConnection<'a, M>
where
    M: ManageConnection,
{
    type Target = M::Connection;

    fn deref(&self) -> &M::Connection {
        &self.conn.as_ref().unwrap().conn
    }
}

impl<'a, M> DerefMut for PooledConnection<'a, M>
where
    M: ManageConnection,
{
    fn deref_mut(&mut self) -> &mut M::Connection {
        &mut self.conn.as_mut().unwrap().conn
    }
}

impl<'a, M> fmt::Debug for PooledConnection<'a, M>
where
    M: ManageConnection,
    M::Connection: fmt::Debug,
{
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(&self.conn.as_ref().unwrap().conn, fmt)
    }
}

impl<'a, M> Drop for PooledConnection<'a, M>
where
    M: ManageConnection,
{
    fn drop(&mut self) {
        futures::executor::block_on(async {
            self.pool
                .put_back(self.checkout, self.conn.take().unwrap().conn)
                .await;
        })
    }
}
