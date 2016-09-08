extern crate curl;
extern crate env_logger;
extern crate futures;
extern crate tokio_core;
extern crate tokio_curl;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use curl::easy::Easy;
use futures::Future;
use tokio_core::reactor::{Core, Timeout};
use tokio_curl::Session;

#[test]
fn download_rust_lang() {
    let mut lp = Core::new().unwrap();

    let session = Session::new(lp.handle());
    let response = Arc::new(Mutex::new(Vec::new()));
    let headers = Arc::new(Mutex::new(Vec::new()));

    let mut req = Easy::new();
    req.get(true).unwrap();
    req.url("https://www.rust-lang.org").unwrap();
    let response2 = response.clone();
    req.write_function(move |data| {
        response2.lock().unwrap().extend_from_slice(data);
        Ok(data.len())
    }).unwrap();
    let headers2 = headers.clone();
    req.header_function(move |header| {
        headers2.lock().unwrap().push(header.to_vec());
        true
    }).unwrap();

    let requests = session.perform(req).map(move |mut resp| {
        assert_eq!(resp.response_code().unwrap(), 200);
        let response = response.lock().unwrap();
        let response = String::from_utf8_lossy(&response);
        assert!(response.contains("<html>"));
        assert!(headers.lock().unwrap().len() > 0);
    });

    lp.run(requests).unwrap();
}

#[test]
fn timeout_download_rust_lang() {
    let mut lp = Core::new().unwrap();

    let session = Session::new(lp.handle());

    let mut req = Easy::new();
    req.get(true).unwrap();
    req.url("https://www.rust-lang.org").unwrap();
    req.write_function(|data| Ok(data.len())).unwrap();
    let req = session.perform(req).map_err(|err| err.into_error());

    let timeout = Timeout::new(Duration::from_millis(5), &lp.handle()).unwrap();
    let result = req.map(Ok).select(timeout.map(Err)).then(|res| {
        match res {
            Ok((Ok(_), _)) => {
                panic!("should have timed out");
            }
            Ok((Err(()), _)) => futures::finished::<(), ()>(()),
            Err((e, _)) => panic!("I/O error: {}", e),
        }
    });

    lp.run(result).unwrap();
}
