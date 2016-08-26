//! A simple program to fetch two HTTP pages in parallel
//!
//! This example will fetch the rust-lang home page as well as GitHub's home
//! page. Both transfers are executed in parallel one thread using futures.

extern crate curl;
extern crate env_logger;
extern crate futures;
extern crate tokio_core;
extern crate tokio_curl;

use curl::easy::Easy;
use futures::Future;
use tokio_core::Loop;
use tokio_curl::Session;

fn main() {
    env_logger::init().unwrap();

    let mut lp = Loop::new().unwrap();
    let session = Session::new(lp.pin());

    // Once we've got our session available to us, execute our two requests.
    // Each request will be a GET request and for now we just ignore the actual
    // downloaded data.
    let mut a = Easy::new();
    a.get(true).unwrap();
    a.url("https://www.rust-lang.org").unwrap();
    a.write_function(|data| Ok(data.len())).unwrap();

    let mut b = Easy::new();
    b.get(true).unwrap();
    b.url("https://github.com").unwrap();
    b.write_function(|data| Ok(data.len())).unwrap();

    let requests = session.perform(a).join(session.perform(b));

    // Run both requests, waiting for them to finish. Once done we print out
    // their response codes and errors.
    let ((mut a, aerr), (mut b, berr)) = lp.run(requests).unwrap();
    println!("{:?} {:?}", a.response_code(), aerr);
    println!("{:?} {:?}", b.response_code(), berr);
}
