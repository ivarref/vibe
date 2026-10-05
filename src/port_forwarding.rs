// Forward TCP ports the guest listens on to localhost on the host.
//
// A loop in the guest (port_forwarding_guest.sh) polls the guest's listening TCP sockets and reports
// "+<port>" / "-<port>" lines over a virtio console port. For each listening port it also runs a
// relay from vsock port <port> to that socket. The host listens on localhost:<port> and carries
// each connection over vsock. Going over vsock rather than the VM's IP reaches servers that only
// listen on the guest's loopback interface (the default for most dev servers), and works the same
// in every network mode.
//
// The guest's Docker socket is forwarded the same way, to a Unix socket on the host.

use std::{
    collections::HashMap,
    fs::{self, File},
    io::{self, BufRead, BufReader, Read, Write},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream},
    os::{
        fd::{FromRawFd, OwnedFd},
        unix::net::{UnixListener, UnixStream},
    },
    path::Path,
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

// Above the TCP port range, which the port forwarding relays use as vsock ports.
// Must match the port in port_forwarding_guest.sh.
const DOCKER_VSOCK_PORT: u32 = 100_000;

const VSOCK_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

// The device may only be used from the VM's queue (the main queue); we only ever touch it there.
pub struct VsockDevice(Retained<VZVirtioSocketDevice>);
unsafe impl Send for VsockDevice {}
unsafe impl Sync for VsockDevice {}

impl VsockDevice {
    pub fn new(device: Retained<VZVirtioSocketDevice>) -> Arc<Self> {
        Arc::new(Self(device))
    }
}

struct Forward {
    stop: Arc<AtomicBool>,
    addrs: Vec<SocketAddr>,
    threads: Vec<JoinHandle<()>>,
}

impl Forward {
    fn start(device: &Arc<VsockDevice>, port: u16) -> Option<Self> {
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
                        if let Some(vsock) = vsock_connect_with_retry(&device, port as u32) {
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
fn vsock_connect_with_retry(device: &Arc<VsockDevice>, port: u32) -> Option<OwnedFd> {
    for _ in 0..100 {
        if let Some(fd) = vsock_connect(device, port) {
            return Some(fd);
        }
        thread::sleep(Duration::from_millis(10));
    }
    None
}

fn vsock_connect(device: &Arc<VsockDevice>, port: u32) -> Option<OwnedFd> {
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
                .connectToPort_completionHandler(port, &completion_handler)
        };
    });
    rx.recv_timeout(VSOCK_CONNECT_TIMEOUT).ok().flatten()
}

trait Stream: Read + Write + Send + Sized + 'static {
    fn try_clone(&self) -> io::Result<Self>;
    fn shutdown_write(&self);
}

impl Stream for TcpStream {
    fn try_clone(&self) -> io::Result<Self> {
        TcpStream::try_clone(self)
    }
    fn shutdown_write(&self) {
        let _ = self.shutdown(Shutdown::Write);
    }
}

impl Stream for UnixStream {
    fn try_clone(&self) -> io::Result<Self> {
        UnixStream::try_clone(self)
    }
    fn shutdown_write(&self) {
        let _ = self.shutdown(Shutdown::Write);
    }
}

fn relay(client: impl Stream, vsock: UnixStream) {
    let (Ok(mut client_read), Ok(mut vsock_write)) = (client.try_clone(), vsock.try_clone()) else {
        return;
    };
    let upstream = thread::spawn(move || {
        let _ = io::copy(&mut client_read, &mut vsock_write);
        vsock_write.shutdown_write();
    });
    let (mut vsock_read, mut client_write) = (vsock, client);
    let _ = io::copy(&mut vsock_read, &mut client_write);
    client_write.shutdown_write();
    let _ = upstream.join();
}

/// Serve the guest's Docker socket at `path` on the host. Returns false if another VM is already
/// serving it (only one can), or the socket couldn't be created.
pub fn spawn_docker_socket_forwarder(device: Arc<VsockDevice>, path: &Path) -> bool {
    if UnixStream::connect(path).is_ok() {
        return false;
    }
    // Left over from a VM that didn't shut down cleanly.
    let _ = fs::remove_file(path);
    let Ok(listener) = UnixListener::bind(path) else {
        return false;
    };
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let device = Arc::clone(&device);
            thread::spawn(move || {
                if let Some(vsock) = vsock_connect_with_retry(&device, DOCKER_VSOCK_PORT) {
                    relay(stream, UnixStream::from(vsock));
                }
            });
        }
    });
    true
}

pub fn spawn_port_forwarder(device: Arc<VsockDevice>, guest_reports: OwnedFd) {
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
