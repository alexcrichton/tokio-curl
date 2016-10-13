extern crate libc;
extern crate mio;
extern crate slab;

use std::cell::RefCell;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use curl::{self, Error};
use curl::easy::Easy;
use curl::multi::{Multi, EasyHandle, Socket, SocketEvents, Events};
use futures::{self, Future, Poll, Oneshot, Complete, Async};
use futures::task::{self, EventSet, UnparkEvent};
use futures::stream::{Stream, Fuse};
use tokio_core::reactor::{Timeout, Handle, PollEvented};
use tokio_core::channel::{channel, Sender, Receiver};
use self::mio::unix::EventedFd;
use self::slab::Slab;

use stack::Stack;

#[derive(Clone)]
pub struct Session {
    tx: Sender<Message>,
}

enum Message {
    Execute(Easy, Complete<io::Result<(Easy, Option<Error>)>>),
}

struct Data {
    multi: Multi,
    state: RefCell<State>,
    handle: Handle,
    rx: Fuse<Receiver<Message>>,
    stack: Arc<Stack<usize>>,
}

struct State {
    // Active HTTP requests, storing each `EasyHandle` as well as the `Complete`
    // half of the HTTP future.
    handles: Slab<HandleEntry>,

    // Sockets we've been requested to track by libcurl. Stores the I/O object
    // we associate with the event loop as well as other state about what
    // libcurl needs from the socket.
    sockets: Slab<SocketEntry>,

    // Last timeout requested by libcurl that we schedule.
    timeout: TimeoutState,
}

struct HandleEntry {
    complete: Complete<io::Result<(Easy, Option<Error>)>>,
    handle: EasyHandle,
}

struct SocketEntry {
    want: Option<SocketEvents>,
    changed: bool,
    stream: PollEvented<MioSocket>,
}

enum TimeoutState {
    Waiting(Timeout),
    Ready,
    None,
}

scoped_thread_local!(static DATA: Data);

pub struct Perform {
    inner: Oneshot<io::Result<(Easy, Option<Error>)>>,
}

impl Session {
    pub fn new(handle: Handle) -> Session {
        let mut m = Multi::new();

        let (tx, rx) = channel(&handle).unwrap();

        m.timer_function(move |dur| {
            DATA.with(|d| d.schedule_timeout(dur))
        }).unwrap();

        m.socket_function(move |socket, events, token| {
            DATA.with(|d| d.schedule_socket(socket, events, token))
        }).unwrap();

        handle.clone().spawn(Data {
            rx: rx.fuse(),
            multi: m,
            handle: handle,
            stack: Arc::new(Stack::new()),
            state: RefCell::new(State {
                handles: Slab::with_capacity(128),
                sockets: Slab::with_capacity(128),
                timeout: TimeoutState::None,
            }),
        }.map_err(|e| {
            panic!("error while processing http requests: {}", e)
        }));

        Session { tx: tx }
    }

    pub fn perform(&self, handle: Easy) -> Perform {
        let (tx, rx) = futures::oneshot();
        self.tx.send(Message::Execute(handle, tx))
            .expect("driver task has gone away");
        Perform { inner: rx }
    }
}

impl Future for Perform {
    type Item = (Easy, Option<Error>);
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, io::Error> {
        match self.inner.poll().expect("complete canceled") {
            Async::Ready(Ok(res)) => Ok(res.into()),
            Async::Ready(Err(e)) => Err(e),
            Async::NotReady => Ok(Async::NotReady),
        }
    }
}

impl Future for Data {
    type Item = ();
    type Error = io::Error;

    fn poll(&mut self) -> Poll<(), io::Error> {
        debug!("-------------------------- driver poll start");

        // First up, process any incoming messages which represent new HTTP
        // requests to execute.
        try!(self.check_messages());

        DATA.set(self, || {
            // Process events for each handle which have happened since we were
            // last here.
            //
            // Note that this implementation currently uses `with_unpark_event`
            // so we **do not poll all handles** but rather just those listed in
            // our `stack` where events were pushed onto. The
            // `with_unpark_event` method ensures that any notifications sent to
            // a task will also inform us why they're being notified.
            for idx in self.stack.drain() {
                let event = UnparkEvent::new(self.stack.clone(), idx);
                task::with_unpark_event(event, || {
                    self.check(idx);
                });
            }

            // Process a timeout, if one ocurred.
            self.check_timeout();

            // After all that's done, we check to see if any transfers have
            // completed.
            self.check_completions();
        });

        // If we're not receiving any messages and there are no active HTTP
        // requests then we're done, otherwise we should keep going.
        if self.rx.is_done() && self.state.borrow().handles.is_empty() {
            assert!(self.state.borrow().sockets.len() == 0);
            Ok(().into())
        } else {
            Ok(Async::NotReady)
        }
    }
}

impl Data {
    /// Function called whenever a new timeout is requested from libcurl.
    ///
    /// An argument of `None` indicates the current timeout can be cleared, and
    /// otherwise this indicates a new timeout to set for informing libcurl that
    /// a timeout has happened.
    fn schedule_timeout(&self, dur: Option<Duration>) -> bool {
        // First up, always clear the existing timeout
        let mut state = self.state.borrow_mut();
        state.timeout = TimeoutState::None;

        // If a timeout was requested, then we configure one. Note that we know
        // for sure that we're executing on the event loop because `Data` is
        // owned by the event loop thread. As a result the returned future from
        // `LoopHandle::timeout` should be immediately resolve-able, so we do so
        // here to pull out the actual timeout future.
        if let Some(dur) = dur {
            debug!("scheduling a new timeout in {:?}", dur);
            if dur == Duration::new(0, 0) {
                state.timeout = TimeoutState::Ready;
            } else {
                let mut timeout = Timeout::new(dur, &self.handle).unwrap();
                drop(state);
                let res = timeout.poll().unwrap();
                state = self.state.borrow_mut();
                match res {
                    Async::NotReady => {
                        state.timeout = TimeoutState::Waiting(timeout);
                    }
                    Async::Ready(()) => state.timeout = TimeoutState::Ready,
                }
            }
        }

        true
    }

    /// Function called whenever libcurl requests events to be listened for on a
    /// socket.
    ///
    /// This function is informed of the raw socket file descriptor, `socket`,
    /// the events that we're interested in, `events`, as well as a user-defined
    /// token, `token`. It's up to us to ensure that we're waiting appropriately
    /// for these events to happen, and then we'll later inform libcurl when
    /// they actually happen.
    fn schedule_socket(&self,
                       socket: Socket,
                       events: SocketEvents,
                       token: usize) {
        let mut state = self.state.borrow_mut();

        // First up, if libcurl wants us to forget about this socket, we do so!
        if events.remove() {
            assert!(token > 0);
            debug!("remove socket: {} / {}", socket, token - 1);
            state.sockets.remove(token - 1).unwrap();
            return
        }

        // If this is the first time we've seen the socket then we register a
        // new source with the event loop. Currently that's done through
        // `PollEvented` which handles registration and deregistration of
        // interest on the event loop itself.
        //
        // Like above with timeouts, the future returned from `PollEvented`
        // should be immediately resolve-able because we're guaranteed to be on
        // the event loop.
        let index = if token == 0 {
            let source = MioSocket { inner: socket };
            let stream = PollEvented::new(source, &self.handle).unwrap();
            if !state.sockets.has_available() {
                let len = state.sockets.len();
                state.sockets.reserve_exact(len);
            }
            let entry = state.sockets.vacant_entry().unwrap();
            let index = entry.index();
            entry.insert(SocketEntry {
                want: None,
                changed: false,
                stream: stream,
            });
            self.multi.assign(socket, index + 1).expect("failed to assign");
            debug!("schedule new socket {} / {}", socket, index);
            index
        } else {
            debug!("activity old socket {} / {}", socket, token - 1);
            token - 1
        };

        let event = UnparkEvent::new(self.stack.clone(), 2 * index + 1);
        let state = &mut state.sockets[index];
        state.want = Some(events);
        state.changed = true;

        // TODO: this pushes a duplicate unpark event if we're already inside of
        //       another unpark event.
        task::with_unpark_event(event, || {
            state.stream.need_read();
            state.stream.need_write();
        });
    }

    fn check_messages(&mut self) -> io::Result<()> {
        loop {
            let msg = match try!(self.rx.poll()) {
                Async::Ready(Some(msg)) => msg,
                Async::Ready(None) => break,
                Async::NotReady => break,
            };
            let (easy, tx) = match msg {
                Message::Execute(easy, tx) => (easy, tx),
            };

            // Add the easy handle to the multi handle, beginning the HTTP
            // request. This may entail libcurl requesting a new timeout or new
            // sockets to be tracked as part of the call to `add`.
            debug!("executing a new request");
            let mut handle = match DATA.set(self, || self.multi.add(easy)) {
                Ok(handle) => handle,
                Err(e) => {
                    tx.complete(Err(e.into()));
                    continue
                }
            };

            // Add the handle to the `handles` slab, acquiring its token we'll
            // use.
            let mut state = self.state.borrow_mut();
            if !state.handles.has_available() {
                let len = state.handles.len();
                state.handles.reserve_exact(len);
            }
            let entry = state.handles.vacant_entry().unwrap();
            let index = entry.index();
            handle.set_token(index).unwrap();
            entry.insert(HandleEntry {
                complete: tx,
                handle: handle,
            });

            // Enqueue a request to poll the state of the `complete` half so we
            // can get a notification when it goes away.
            self.stack.push(2 * index);
        }

        Ok(())
    }

    fn check(&self, idx: usize) {
        if idx % 2 == 0 {
            self.check_cancel(idx / 2)
        } else {
            self.check_socket(idx / 2)
        }
    }

    fn check_cancel(&self, idx: usize) {
        debug!("\tevent cancel {}", idx);
        // See if this request has been canceled
        let mut state = self.state.borrow_mut();
        if state.handles.get_mut(idx).is_none() {
            return
        }
        if let Ok(Async::Ready(())) = state.handles[idx].complete.poll_cancel() {
            let entry = state.handles.remove(idx).unwrap();
            drop(state);
            let handle = entry.handle;
            drop(self.multi.remove(handle));
        }
    }

    fn check_socket(&self, idx: usize) {
        debug!("\tevent socket {}", idx);
        let mut state = self.state.borrow_mut();
        let mut events = Events::new();
        let mut set = false;
        if state.sockets[idx].stream.poll_read().is_ready() {
            debug!("\treadable");
            events.input(true);
            set = true;
        }
        if state.sockets[idx].stream.poll_write().is_ready() {
            debug!("\twritable");
            events.output(true);
            set = true;
        }
        if !set {
            return
        }

        state.sockets[idx].changed = false;
        let socket = state.sockets[idx].stream.get_ref().inner;
        drop(state);
        debug!("\tactivity on {}", socket);
        self.multi.action(socket, &events).expect("action error");
        state = self.state.borrow_mut();

        let state = match state.sockets.get_mut(idx) {
            Some(state) => state,
            None => return,
        };
        if state.changed {
            return
        }
        let want = match state.want {
            Some(ref want) => want,
            None => return,
        };

        let mut fd = libc::pollfd {
            fd: socket,
            events: 0,
            revents: 0,
        };
        if want.input() {
            fd.events |= libc::POLLIN;
        }
        if want.output() {
            fd.events |= libc::POLLOUT;
        }
        unsafe {
            libc::poll(&mut fd, 1, 0);
        }
        if want.input() && (fd.revents & libc::POLLIN) == 0 {
            state.stream.need_read();
            return
        }
        if want.output() && (fd.revents & libc::POLLOUT) == 0 {
            state.stream.need_write();
            return
        }
        task::park().unpark();
    }

    fn check_timeout(&self) {
        // Sometimes telling libcurl that we timed out causes it to request
        // again that we should time out, so execute this in a loop.
        loop {
            match self.state.borrow_mut().timeout {
                TimeoutState::Waiting(ref mut t) => {
                    match t.poll() {
                        Ok(Async::Ready(())) => {}
                        _ => return,
                    }
                }
                TimeoutState::Ready => {}
                TimeoutState::None => return
            }
            debug!("timeout fired");
            self.state.borrow_mut().timeout = TimeoutState::None;
            self.multi.timeout().expect("timeout error");
        }
    }

    fn check_completions(&self) {
        self.multi.messages(|m| {
            let mut state = self.state.borrow_mut();
            let transfer_err = m.result().unwrap();
            let idx = m.token().unwrap();
            let entry = state.handles.remove(idx).unwrap();
            debug!("request is now finished: {}", idx);
            drop(state);
            assert!(m.is_for(&entry.handle));

            // If `remove_err` fails then that's super fatal, so that'll end
            // up in the `Error` of the `Perform` future. If, however, the
            // transfer just failed, then that's communicated through
            // `transfer_err`, so we just put that next to the handle if we
            // get it out successfully.
            let remove_err = self.multi.remove(entry.handle);
            let res = remove_err.map(|e| (e, transfer_err.err()))
                                .map_err(|e| e.into());
            entry.complete.complete(res);
        });
    }
}

struct MioSocket {
    inner: curl::multi::Socket,
}

impl mio::Evented for MioSocket {
    fn register(&self,
                poll: &mio::Poll,
                token: mio::Token,
                interest: mio::Ready,
                opts: mio::PollOpt) -> io::Result<()> {
        EventedFd(&self.inner).register(poll, token, interest, opts)
    }

    fn reregister(&self,
                  poll: &mio::Poll,
                  token: mio::Token,
                  interest: mio::Ready,
                  opts: mio::PollOpt) -> io::Result<()> {
        EventedFd(&self.inner).reregister(poll, token, interest, opts)
    }

    fn deregister(&self, poll: &mio::Poll) -> io::Result<()> {
        EventedFd(&self.inner).deregister(poll)
    }
}

impl EventSet for Stack<usize> {
    fn insert(&self, id: usize) {
        self.push(id);
    }
}
