use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{self, ErrorKind, Write};
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use bytes::buf::FromBuf;
use futures::{Async, Future, Poll};
use futures::task::{self, Task};
use kcp::{Error as KcpError, Kcp, KcpResult, get_conv};
use tokio_core::net::UdpSocket;
use tokio_core::reactor::Handle;

use config::KcpConfig;

struct KcpOutputInner {
    udp: Rc<UdpSocket>,
    task: Option<Task>,
    pkt_queue: VecDeque<(SocketAddr, Bytes)>,
    is_finished: bool,
}

impl KcpOutputInner {
    fn new(udp: Rc<UdpSocket>) -> KcpOutputInner {
        KcpOutputInner {
            udp: udp,
            task: None,
            pkt_queue: VecDeque::new(),
            is_finished: false,
        }
    }

    fn notify(&mut self) {
        if let Some(task) = self.task.take() {
            task.notify();
        }
    }

    fn push_packet(&mut self, pkt: Bytes, peer: SocketAddr) {
        self.pkt_queue.push_back((peer, pkt));
        self.notify();
    }

    fn close(&mut self) {
        self.is_finished = true;
        self.notify();
    }

    fn is_empty(&self) -> bool {
        self.pkt_queue.is_empty()
    }

    fn send_or_push(&mut self, buf: &[u8], peer: &SocketAddr) -> io::Result<usize> {
        if self.is_empty() {
            match self.udp.send_to(buf, peer) {
                Ok(n) => {
                    trace!("[SEND] UDP peer={} conv={} size={} {:?}",
                           peer,
                           get_conv(buf),
                           buf.len(),
                           ::debug::BsDebug(buf));
                    if n != buf.len() {
                        error!("[SEND] UDP Sent size={}, but packet is size={}", n, buf.len());
                    }
                    return Ok(n);
                }
                Err(ref err) if err.kind() == ErrorKind::WouldBlock => {}
                Err(err) => return Err(err),
            }
        }

        trace!("[SEND] Delay UDP peer={} conv={} size={} qsize={} {:?}",
               peer,
               get_conv(buf),
               buf.len(),
               self.pkt_queue.len(),
               ::debug::BsDebug(buf));

        self.push_packet(Bytes::from_buf(buf), *peer);
        Ok(buf.len())
    }
}

impl Drop for KcpOutputInner {
    fn drop(&mut self) {
        self.close();
    }
}

struct KcpOutputQueue {
    inner: Rc<RefCell<KcpOutputInner>>,
}

impl Future for KcpOutputQueue {
    type Item = ();
    type Error = io::Error;

    fn poll(&mut self) -> Poll<(), io::Error> {
        let mut inner = self.inner.borrow_mut();

        while !inner.pkt_queue.is_empty() {
            {
                let &(ref peer, ref pkt) = &inner.pkt_queue[0];
                let n = try_nb!(inner.udp.send_to(&*pkt, peer));
                trace!("[SEND] Delayed UDP peer={} conv={} size={} {:?}", peer, get_conv(&pkt), pkt.len(), pkt);
                if n != pkt.len() {
                    error!("[SEND] Delayed Sent size={}, but packet is size={}", n, pkt.len());
                }
            }

            let _ = inner.pkt_queue.pop_front();
        }

        if inner.is_finished {
            Ok(Async::Ready(()))
        } else {
            inner.task = Some(task::current());
            Ok(Async::NotReady)
        }
    }
}

#[derive(Clone)]
pub struct KcpOutputHandle {
    inner: Rc<RefCell<KcpOutputInner>>,
}

impl KcpOutputHandle {
    pub fn new(udp: Rc<UdpSocket>, handle: &Handle) -> KcpOutputHandle {
        let inner = KcpOutputInner::new(udp);
        let inner = Rc::new(RefCell::new(inner));
        let queue = KcpOutputQueue { inner: inner.clone() };
        handle.spawn(queue.map_err(move |err| {
                                       error!("[SEND] UDP output queue failed, err: {:?}", err);
                                   }));
        KcpOutputHandle { inner: inner }
    }

    fn send_to(&mut self, buf: &[u8], peer: &SocketAddr) -> io::Result<usize> {
        let mut inner = self.inner.borrow_mut();
        inner.send_or_push(buf, peer)
    }

    fn udp(&self) -> Rc<UdpSocket> {
        let inner = self.inner.borrow();
        inner.udp.clone()
    }
}

pub struct KcpOutput {
    inner: KcpOutputHandle,
    peer: SocketAddr,
}

impl KcpOutput {
    pub fn new(udp: Rc<UdpSocket>, peer: SocketAddr, handle: &Handle) -> KcpOutput {
        KcpOutput::new_with_handle(KcpOutputHandle::new(udp, handle), peer)
    }

    pub fn new_with_handle(h: KcpOutputHandle, peer: SocketAddr) -> KcpOutput {
        KcpOutput {
            inner: h,
            peer: peer,
        }
    }

    fn udp(&self) -> Rc<UdpSocket> {
        self.inner.udp()
    }
}

impl Write for KcpOutput {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.send_to(buf, &self.peer)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct KcpCell {
    kcp: Kcp<KcpOutput>,
    last_update: Instant,
    is_closed: bool,
    send_task: Option<Task>,
    udp: Rc<UdpSocket>,
    recv_buf: Vec<u8>,
    expired: bool,
}

impl Drop for KcpCell {
    fn drop(&mut self) {
        let _ = self.kcp.flush();
    }
}

impl KcpCell {
    fn input(&mut self, buf: &[u8]) -> KcpResult<()> {
        match self.kcp.input(buf) {
            Ok(..) => {}
            Err(KcpError::ConvInconsistent(..)) => {}
            Err(err) => return Err(err),
        }
        self.last_update = Instant::now();
        Ok(())
    }

    fn input_self(&mut self, n: usize) -> KcpResult<()> {
        match self.kcp.input(&self.recv_buf[..n]) {
            Ok(..) => {}
            Err(KcpError::ConvInconsistent(..)) => {}
            Err(err) => return Err(err),
        }
        self.last_update = Instant::now();
        Ok(())
    }

    fn fetch(&mut self) -> KcpResult<()> {
        // Initialize with MTU
        if self.recv_buf.len() < self.kcp.mtu() {
            let mtu = self.kcp.mtu();
            self.recv_buf.resize(mtu, 0);
        }

        let n = match self.udp.recv_from(&mut self.recv_buf) {
            Ok((n, _)) => n,
            Err(ref err) if err.kind() == ErrorKind::WouldBlock => {
                return Ok(());
            }
            Err(err) => return Err(From::from(err)),
        };

        // Ah, we got something
        trace!("[RECV] Fetch. SharedKcp recv size={} {:?}", n, ::debug::BsDebug(&self.recv_buf[..n]));
        self.input_self(n)
    }
}

#[derive(Clone)]
pub struct SharedKcp {
    inner: Rc<RefCell<KcpCell>>,
}

impl SharedKcp {
    pub fn new(c: &KcpConfig, conv: u32, udp: Rc<UdpSocket>, peer: SocketAddr, handle: &Handle) -> SharedKcp {
        let output = KcpOutput::new(udp, peer, handle);
        SharedKcp::new_with_output(c, conv, output)
    }

    pub fn new_with_output(c: &KcpConfig, conv: u32, output: KcpOutput) -> SharedKcp {
        let udp = output.udp();
        let mut kcp = Kcp::new(conv, output);
        c.apply_config(&mut kcp);

        // Ask server to allocate one
        if conv == 0 {
            kcp.input_conv();
        }

        SharedKcp {
            inner: Rc::new(RefCell::new(KcpCell {
                                            kcp: kcp,
                                            last_update: Instant::now(),
                                            is_closed: false,
                                            send_task: None,
                                            udp: udp,
                                            recv_buf: Vec::new(), // Do not initialize it yet.
                                            expired: false,
                                        })),
        }
    }

    /// Call every time you got data from transmission
    pub fn input(&mut self, buf: &[u8]) -> KcpResult<()> {
        let mut inner = self.inner.borrow_mut();
        inner.input(buf)
    }

    /// Recv then input, ignore WouldBlock
    pub fn fetch(&mut self) -> KcpResult<()> {
        let mut inner = self.inner.borrow_mut();
        inner.fetch()
    }

    /// Call if you want to send some data
    pub fn send(&mut self, buf: &[u8]) -> KcpResult<usize> {
        let mut inner = self.inner.borrow_mut();

        if inner.kcp.wait_snd() >= inner.kcp.snd_wnd() as usize {
            trace!("[SEND] waitsnd={} sndwnd={} excceeded", inner.kcp.wait_snd(), inner.kcp.snd_wnd());
            inner.send_task = Some(task::current());
            return Err(From::from(io::Error::new(ErrorKind::WouldBlock, "too many pending packets")));
        }

        let n = inner.kcp.send(buf)?;
        inner.last_update = Instant::now();
        Ok(n)
    }

    /// Try to notify writable
    pub fn try_notify_writable(&mut self) {
        let mut inner = self.inner.borrow_mut();
        if inner.kcp.wait_snd() < inner.kcp.snd_wnd() as usize {
            if let Some(task) = inner.send_task.take() {
                task.notify();
            }
        }
    }

    /// Call if you want to get some data
    /// Always call right after input
    pub fn recv(&mut self, buf: &mut [u8]) -> KcpResult<usize> {
        let mut inner = self.inner.borrow_mut();
        if inner.expired {
            // If it is already expired, return EOF
            return Ok(0);
        }

        let n = inner.kcp.recv(buf)?;
        inner.last_update = Instant::now();
        Ok(n)
    }

    /// Call if you want to flush all pending data in queue
    pub fn flush(&mut self) -> KcpResult<()> {
        let mut inner = self.inner.borrow_mut();
        inner.kcp.flush()?;
        inner.last_update = Instant::now();
        Ok(())
    }

    /// Tell me how long this session have no interaction
    pub fn elapsed(&self) -> Duration {
        let inner = self.inner.borrow();
        inner.last_update.elapsed()
    }

    /// Make this session expire, all read apis will return 0 (EOF)
    /// It will flush the buffer when it is called
    pub fn set_expired(&mut self) -> KcpResult<()> {
        let mut inner = self.inner.borrow_mut();
        inner.expired = true;
        inner.kcp.flush()
    }

    /// Call in every tick
    /// Returns when to call this function again
    pub fn update(&mut self) -> KcpResult<Instant> {
        let mut inner = self.inner.borrow_mut();
        inner.kcp.update(::current())?;
        let next = inner.kcp.check(::current());
        Ok(Instant::now() + Duration::from_millis(next as u64))
    }

    /// Check if send queue is empty
    pub fn has_waitsnd(&self) -> bool {
        let inner = self.inner.borrow();
        inner.kcp.wait_snd() != 0
    }

    /// Get mtu
    pub fn mtu(&self) -> usize {
        let inner = self.inner.borrow();
        inner.kcp.mtu()
    }

    /// Set is close
    pub fn close(&mut self) {
        let mut inner = self.inner.borrow_mut();
        inner.is_closed = true;
    }

    /// Check if it is closed
    pub fn is_closed(&self) -> bool {
        let inner = self.inner.borrow();
        inner.is_closed
    }

    /// Check if it can read
    pub fn can_read(&self) -> bool {
        let inner = self.inner.borrow();
        inner.kcp.peeksize().unwrap_or(0) != 0
    }

    /// Peek
    pub fn peeksize(&self) -> usize {
        let inner = self.inner.borrow();
        inner.kcp.peeksize().unwrap_or(0)
    }

    /// Check if waitsnd > snd_wnd
    pub fn can_send(&self) -> bool {
        let inner = self.inner.borrow();
        inner.kcp.wait_snd() < inner.kcp.snd_wnd() as usize
    }

    /// Get conv
    pub fn conv(&self) -> u32 {
        let inner = self.inner.borrow();
        inner.kcp.conv()
    }
}
