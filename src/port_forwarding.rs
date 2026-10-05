// Forward TCP ports the guest listens on to localhost on the host.
//
// A loop in the guest (port_forwarding_guest.sh) polls the guest's listening TCP sockets and reports
// "+<port>" / "-<port>" lines over a virtio console port. For each listening port it also runs a
// relay from vsock port <port> to that socket. The host listens on localhost:<port> and carries
// each connection over vsock. Going over vsock rather than the VM's IP reaches servers that only
// listen on the guest's loopback interface (the default for most dev servers), and works the same
// in every network mode.

use std::{
    collections::HashMap,
    fs::File,
    io::{self, BufRead, BufReader},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream},
    os::{
        fd::{FromRawFd, OwnedFd},
        unix::net::UnixStream,
    },
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use block2::RcBlock;
use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2_foundation::NSError;
use objc2_virtualization::{VZVirtioSocketConnection, VZVirtioSocketDevice};

pub const PORT_REPORTS_PORT_NAME: &str = "vibe-ports";
pub const PORT_FORWARDING_GUEST_SCRIPT: &str = include_str!("port_forwarding_guest.sh");

const VSOCK_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

// The device may only be used from the VM's queue (the main queue); we only ever touch it there.
struct SocketDevice(Retained<VZVirtioSocketDevice>);
unsafe impl Send for SocketDevice {}
unsafe impl Sync for SocketDevice {}

struct Forward {
    stop: Arc<AtomicBool>,
    addrs: Vec<SocketAddr>,
    threads: Vec<JoinHandle<()>>,
}

impl Forward {
    fn start(device: &Arc<SocketDevice>, port: u16) -> Option<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let mut addrs = Vec::new();
        let mut threads = Vec::new();
        // Clients may resolve localhost to either address; IPv6 is best-effort.
        for ip in [IpAddr::V4(Ipv4Addr::LOCALHOST), IpAddr::V6(Ipv6Addr::LOCALHOST)] {
            let addr = SocketAddr::new(ip, port);
            let Ok(listener) = TcpListener::bind(addr) else {
                continue;
            };
            let device = Arc::clone(device);
            let stop = Arc::clone(&stop);
            addrs.push(addr);
            threads.push(thread::spawn(move || {
                for stream in listener.incoming() {
                    if stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let Ok(stream) = stream else { continue };
                    let device = Arc::clone(&device);
                    thread::spawn(move || {
                        if let Some(vsock) = vsock_connect_with_retry(&device, port) {
                            relay(stream, UnixStream::from(vsock));
                        }
                    });
                }
            }));
        }
        // Without the IPv4 listener the port is most likely taken on the host; don't forward it.
        if !addrs.first().is_some_and(|a| a.is_ipv4()) {
            let mut forward = Self { stop, addrs, threads };
            forward.stop();
            return None;
        }
        Some(Self { stop, addrs, threads })
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake up the blocked accept() calls so the listeners get dropped.
        for addr in &self.addrs {
            let _ = TcpStream::connect_timeout(addr, Duration::from_secs(1));
        }
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

// The guest reports a port right after starting its relay, which may not be listening yet.
fn vsock_connect_with_retry(device: &Arc<SocketDevice>, port: u16) -> Option<OwnedFd> {
    for _ in 0..100 {
        if let Some(fd) = vsock_connect(device, port) {
            return Some(fd);
        }
        thread::sleep(Duration::from_millis(10));
    }
    None
}

fn vsock_connect(device: &Arc<SocketDevice>, port: u16) -> Option<OwnedFd> {
    let (tx, rx) = mpsc::channel::<Option<OwnedFd>>();
    let device = Arc::clone(device);
    DispatchQueue::main().exec_async(move || {
        let completion_handler = RcBlock::new(
            move |connection: *mut VZVirtioSocketConnection, error: *mut NSError| {
                if connection.is_null() || !error.is_null() {
                    let _ = tx.send(None);
                    return;
                }
                // The connection closes its fd when it's deallocated, so keep our own copy.
                let fd = unsafe { libc::dup((*connection).fileDescriptor()) };
                let _ = tx.send((fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd) }));
            },
        );
        unsafe {
            device
                .0
                .connectToPort_completionHandler(port as u32, &completion_handler)
        };
    });
    rx.recv_timeout(VSOCK_CONNECT_TIMEOUT).ok().flatten()
}

fn relay(tcp: TcpStream, vsock: UnixStream) {
    let (Ok(mut tcp_read), Ok(mut vsock_write)) = (tcp.try_clone(), vsock.try_clone()) else {
        return;
    };
    let upstream = thread::spawn(move || {
        let _ = io::copy(&mut tcp_read, &mut vsock_write);
        let _ = vsock_write.shutdown(Shutdown::Write);
    });
    let (mut vsock_read, mut tcp_write) = (vsock, tcp);
    let _ = io::copy(&mut vsock_read, &mut tcp_write);
    let _ = tcp_write.shutdown(Shutdown::Write);
    let _ = upstream.join();
}

pub fn spawn_port_forwarder(device: Retained<VZVirtioSocketDevice>, guest_reports: OwnedFd) {
    let device = Arc::new(SocketDevice(device));
    thread::spawn(move || {
        let mut forwards: HashMap<u16, Forward> = HashMap::new();
        for line in BufReader::new(File::from(guest_reports)).lines() {
            let Ok(line) = line else { break };
            let line = line.trim();
            let Some((sign, Ok(port))) = line.chars().next().zip(line.get(1..).map(str::parse::<u16>))
            else {
                continue;
            };
            match sign {
                '+' if !forwards.contains_key(&port) => {
                    if let Some(forward) = Forward::start(&device, port) {
                        forwards.insert(port, forward);
                    }
                }
                '-' => {
                    if let Some(mut forward) = forwards.remove(&port) {
                        forward.stop();
                    }
                }
                _ => {}
            }
        }
    });
}
