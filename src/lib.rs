//! A `Future` interface on top of libcurl
//!
//! This crate provides a futures-based interface to the libcurl HTTP library.
//! Building on top of the `curl` crate on crates.io, this allows using a
//! battle-tested C library for sending HTTP requests in an asynchronous
//! fashion.
//!
//! # Examples
//!
//! ```rust
//! extern crate curl;
//! extern crate futures;
//! extern crate tokio_core;
//! extern crate tokio_curl;
//!
//! use std::io::{self, Write};
//!
//! use curl::easy::Easy;
//! use futures::Future;
//! use tokio_core::reactor::Core;
//! use tokio_curl::Session;
//!
//! fn main() {
//!     // Create an event loop that we'll run on, as well as an HTTP `Session`
//!     // which we'll be routing all requests through.
//!     let mut lp = Core::new().unwrap();
//!     let session = Session::new(lp.handle());
//!
//!     // Prepare the HTTP request to be sent.
//!     let mut req = Easy::new();
//!     req.get(true).unwrap();
//!     req.url("https://www.rust-lang.org").unwrap();
//!     req.write_function(|data| {
//!         io::stdout().write_all(data).unwrap();
//!         Ok(data.len())
//!     }).unwrap();
//!
//!     // Once we've got our session, issue an HTTP request to download the
//!     // rust-lang home page
//!     let request = session.perform(req);
//!
//!     // Execute the request, and print the response code as well as the error
//!     // that happened (if any).
//!     let mut req = lp.run(request).unwrap();
//!     println!("{:?}", req.response_code());
//! }
//! ```
//!
//! # Platform support
//!
//! This crate works on both Unix and Windows, but note that it will not scale
//! well on Windows. Unfortunately the implementation (seemingly from libcurl)
//! relies on `select`, which does not scale very far on Windows.

#![doc(html_root_url = "https://docs.rs/tokio-curl/0.1")]
#![deny(missing_docs)]

// TODO: handle level a bit better by turning the event loop every so often

#[macro_use]
extern crate log;
extern crate futures;
extern crate tokio_core;
extern crate curl;

#[macro_use]
#[cfg(unix)]
extern crate scoped_tls;

#[cfg(windows)]
#[path = "windows.rs"]
mod imp;
#[cfg(unix)]
#[path = "unix.rs"]
mod imp;

mod stack;
mod atomic_task;

use std::error;
use std::fmt;
use std::io;

use futures::{Future, Poll, Async};
use curl::easy::Easy;
use tokio_core::reactor::Handle;

/// A shared cache for HTTP requests to pool data such as TCP connections
/// between.
///
/// All HTTP requests in this crate are performed through a `Session` type. A
/// `Session` can be cloned to acquire multiple references to the same session.
///
/// Sessions are created through the `Session::new` method, which returns a
/// future that will resolve to a session once it's been initialized.
#[derive(Clone)]
pub struct Session {
    inner: imp::Session,
}

/// A future returned from the `Session::perform` method.
///
/// This future represents the execution of an entire HTTP request. This future
/// will resolve to the original `Easy` handle provided once the HTTP request is
/// complete so metadata about the request can be inspected.
pub struct Perform {
    inner: imp::Perform,
}

/// Error returned by the future returned from `perform`.
///
/// This error can be converted to an `io::Error` or the underlying `Easy`
/// handle may also be extracted.
pub struct PerformError {
    error: io::Error,
    handle: Option<Easy>,
}

impl Session {
    /// Creates a new HTTP session object which will be bound to the given event
    /// loop.
    ///
    /// When using libcurl it will provide us with file descriptors to listen
    /// for events on, so we'll need raw access to an actual event loop in order
    /// to hook up all the pieces together. The event loop will also be the I/O
    /// home for this HTTP session. All HTTP I/O will occur on the event loop
    /// thread.
    ///
    /// This function returns a future which will resolve to a `Session` once
    /// it's been initialized.
    pub fn new(handle: Handle) -> Session {
        Session { inner: imp::Session::new(handle) }
    }

    /// Execute and HTTP request asynchronously, returning a future representing
    /// the request's completion.
    ///
    /// This method will consume the provided `Easy` handle, which should be
    /// configured appropriately to send off an HTTP request. The returned
    /// future will resolve back to the handle once the request is performed,
    /// along with any error that happened along the way.
    ///
    /// The `Item` of the returned future is `(Easy, Option<Error>)` so you can
    /// always get the `Easy` handle back, and the `Error` part of the future is
    /// `io::Error` which represents errors communicating with the event loop or
    /// otherwise fatal errors for the `Easy` provided.
    ///
    /// Note that if the `Perform` future is dropped it will cancel the
    /// outstanding HTTP request, cleaning up all resources associated with it.
    pub fn perform(&self, handle: Easy) -> Perform {
        Perform { inner: self.inner.perform(handle) }
    }
}

impl Future for Perform {
    type Item = Easy;
    type Error = PerformError;

    fn poll(&mut self) -> Poll<Easy, PerformError> {
        match self.inner.poll() {
            Err(e) => Err(PerformError { error: e, handle: None }),
            Ok(Async::Ready((handle, None))) => Ok(Async::Ready(handle)),
            Ok(Async::Ready((handle, Some(err)))) => {
                Err(PerformError { error: err.into(), handle: Some(handle) })
            }
            Ok(Async::NotReady) => Ok(Async::NotReady),
        }
    }
}

impl PerformError {
    /// Attempts to extract the underlying `Easy` handle, if one is available.
    ///
    /// For some HTTP requests that fail the `Easy` handle is still available to
    /// recycle for another request. Additionally, the handle may contain
    /// information about the failed request. If this is needed, then this
    /// method can be called to extract the easy handle.
    ///
    /// Note that not all failed HTTP requests will have an easy handle
    /// available to return. Some requests may end up destroying the original
    /// easy handle as it couldn't be reclaimed.
    pub fn take_easy(&mut self) -> Option<Easy> {
        self.handle.take()
    }

    /// Returns the underlying I/O error that caused this error.
    ///
    /// All `PerformError` structures will have an associated I/O error with
    /// them. This error indicates why the HTTP request failed, and is likely
    /// going to be backed by a `curl::Error` or a `curl::MultiError`.
    ///
    /// It's also likely if it is available the `Easy` handle recovered from
    /// `take_handle` will have even more detailed information about the error.
    pub fn into_error(self) -> io::Error {
        self.error
    }
}

impl From<PerformError> for io::Error {
    fn from(p: PerformError) -> io::Error {
        p.into_error()
    }
}

impl fmt::Display for PerformError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        "failed to execute request".fmt(f)
    }
}

impl fmt::Debug for PerformError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("PerformError")
         .field("error", &self.error)
         .finish()
    }
}

impl error::Error for PerformError {
    fn description(&self) -> &str {
        "failed to execute request"
    }

    fn cause(&self) -> Option<&error::Error> {
        Some(&self.error)
    }
}
