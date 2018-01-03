extern crate winapi;

use std::io::{self, Read, Write};
use std::mem;
use std::net::{TcpStream, TcpListener};
use std::os::windows::prelude::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use curl::Error;
use curl::easy::Easy;
use curl::multi::{Multi, EasyHandle};
use futures::{Future, Poll, Async};
use futures::sync::oneshot;
use futures::executor::{self, Notify};
use tokio_core::reactor::Handle;
use self::winapi::ctypes::*;
use self::winapi::um::winsock2::*;

pub struct Session {
    tx: Sender<Message>,
    cnt: Arc<AtomicUsize>,
}

enum Message {
    Run(Easy, oneshot::Sender<io::Result<(Easy, Option<Error>)>>),
    Done,
}

struct Sender<T> {
    tx: mpsc::Sender<T>,
    inner: Arc<Channel>,
}

struct Receiver<T> {
    rx: mpsc::Receiver<T>,
    inner: Arc<Channel>,
}

struct Channel {
    ready: AtomicBool,
    tx: TcpStream,
    rx: TcpStream,
}

pub struct Perform {
    inner: oneshot::Receiver<io::Result<(Easy, Option<Error>)>>,
}

impl Session {
    pub fn new(_handle: Handle) -> Session {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let conn1 = TcpStream::connect(&addr).unwrap();
        let conn2 = listener.accept().unwrap().0;
        drop(listener);
        conn1.set_nonblocking(true).unwrap();
        conn2.set_nonblocking(true).unwrap();
        let inner = Arc::new(Channel {
            ready: AtomicBool::new(false),
            tx: conn1,
            rx: conn2,
        });
        let (tx, rx) = mpsc::channel();

        let tx = Sender { tx: tx, inner: inner.clone() };
        let tx2 = tx.clone();
        let rx = Receiver { rx: rx, inner: inner };

        thread::spawn(|| {
            run(tx2, rx);
        });

        Session {
            tx: tx,
            cnt: Arc::new(AtomicUsize::new(1)),
        }
    }

    pub fn perform(&self, handle: Easy) -> Perform {
        let (tx, rx) = oneshot::channel();
        self.tx.send(Message::Run(handle, tx));
        Perform { inner: rx }
    }
}

impl Clone for Session {
    fn clone(&self) -> Session {
        self.cnt.fetch_add(1, Ordering::SeqCst);
        Session {
            tx: self.tx.clone(),
            cnt: self.cnt.clone(),
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if self.cnt.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.tx.send(Message::Done);
        }
    }
}

impl Future for Perform {
    type Item = (Easy, Option<Error>);
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, io::Error> {
        match self.inner.poll().expect("canceled") {
            Async::Ready(Ok(res)) => Ok(res.into()),
            Async::Ready(Err(e)) => Err(e),
            Async::NotReady => Ok(Async::NotReady),
        }
    }
}

fn run(tx: Sender<Message>, rx: Receiver<Message>) {
    let multi = Multi::new();
    let mut active = Vec::new();
    let mut rx_done = false;
    let mut to_remove = Vec::new();
    let notify = Arc::new(MyNotify { inner: tx.inner });

    loop {
        trace!("turn of the loop");
        if !rx_done {
            enqueue(&rx, &multi, &mut active, &mut rx_done);
        }
        if rx_done && active.len() == 0 {
            break
        }

        multi.perform().expect("perform error");

        multi.messages(|msg| {
            let idx = active.iter()
                            .position(|m| msg.is_for(&m.0))
                            .expect("done but not in array?");
            let (handle, complete) = active.remove(idx);
            let res = match multi.remove(handle) {
                Ok(easy) => Ok((easy, msg.result().unwrap().err())),
                Err(e) => Err(e.into()),
            };
            trace!("finishing a request");
            drop(complete.send(res));
        });

        to_remove.truncate(0);
        for (i, &mut (_, ref mut complete)) in active.iter_mut().enumerate() {
            let mut t = executor::spawn(CheckCancel { inner: complete });
            if let Ok(Async::Ready(())) = t.poll_future_notify(&notify, 0) {
                to_remove.push(i);
            }
        }
        for i in to_remove.drain(..).rev() {
            trace!("cancelling a request");
            let (handle, _) = active.remove(i);
            multi.remove(handle).expect("failed to remove");
        }

        unsafe {
            let mut read: fd_set = mem::zeroed();
            let mut write: fd_set = mem::zeroed();
            let mut except: fd_set = mem::zeroed();
            let nfds = multi.fdset2(Some(&mut read),
                                    Some(&mut write),
                                    Some(&mut except)).expect("fdset failure");
            read.fd_array[read.fd_count as usize] = rx.inner.rx.as_raw_socket() as usize;
            read.fd_count += 1;

            let timeout = multi.get_timeout().expect("get_timeout failure");
            let mut timeout = timeout.or_else(|| {
                nfds.map(|_| Duration::from_millis(100))
            }).map(|dur| {
                timeval {
                    tv_sec: dur.as_secs() as c_long,
                    tv_usec: (dur.subsec_nanos() / 1000) as c_long,
                }
            });

            match timeout {
                Some(ref timeout) => {
                    trace!("waiting w/ timeout {}.{:06}", timeout.tv_sec,
                           timeout.tv_usec);
                }
                None => trace!("no timeout"),
            }

            let timeout = timeout.as_mut().map(|t| t as *mut _);
            let timeout = timeout.unwrap_or(0 as *mut _);

            let n = select(0, &mut read, &mut write, &mut except, timeout);
            if n == SOCKET_ERROR {
                panic!("select error: {}", io::Error::last_os_error());
            }
        }
    }

    trace!("we're outta here");

    fn enqueue(rx: &Receiver<Message>,
               multi: &Multi,
               active: &mut Vec<(EasyHandle,
                                 oneshot::Sender<io::Result<(Easy, Option<Error>)>>)>,
               done: &mut bool) {
        if !rx.drain() {
            trace!("no messages available");
            return
        }
        trace!("looking for some messages");
        while let Some(msg) = rx.recv() {
            match msg {
                Message::Done => {
                    debug!("done");
                    *done = true;
                }
                Message::Run(easy, complete) => {
                    trace!("starting a new request");
                    match multi.add(easy) {
                        Ok(handle) => active.push((handle, complete)),
                        Err(e) => drop(complete.send(Err(e.into()))),
                    }
                }
            }
        }
    }
}

impl<T> Sender<T> {
    fn send(&self, t: T) {
        self.tx.send(t).unwrap();
        self.inner.notify();
    }
}

struct MyNotify {
    inner: Arc<Channel>,
}

impl Notify for MyNotify {
    fn notify(&self, _id: usize) {
        self.inner.notify()
    }
}

impl Channel {
    fn notify(&self) {
        if !self.ready.swap(true, Ordering::SeqCst) {
            drop((&self.tx).write(&[1]));
        }
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Sender<T> {
        Sender {
            tx: self.tx.clone(),
            inner: self.inner.clone(),
        }
    }
}

impl<T> Receiver<T> {
    fn recv(&self) -> Option<T> {
        self.rx.try_recv().ok()
    }

    /// Returns whether there are messages to look at
    fn drain(&self) -> bool {
        if !self.inner.ready.swap(false, Ordering::SeqCst) {
            return false
        }
        loop {
            match (&self.inner.rx).read(&mut [0; 32]) {
                Ok(_) => {}
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("I/O error: {}", e),
            }
        }
        return true
    }
}

struct CheckCancel<'a, T: 'a> {
    inner: &'a mut oneshot::Sender<T>,
}

impl<'a, T> Future for CheckCancel<'a, T> {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<(), ()> {
        self.inner.poll_cancel()
    }
}
