//! Vibe's default NAT uses a bundled gVisor/Lima-style user-mode network stack.
//! The helper receives a connected VZ datagram fd and exits when Vibe drops the liveness fd.

use std::{
    fs,
    io::{self, BufRead, BufReader, Read},
    os::{
        fd::{AsRawFd, OwnedFd, RawFd},
        unix::{fs::PermissionsExt, net::UnixDatagram, process::CommandExt},
    },
    path::Path,
    process::{Child, Command, Stdio},
};

const USERNET_HELPER: &[u8] = include_bytes!(env!("BUNDLED_USERNET_HELPER_PATH"));

const USERNET_HELPER_FD: RawFd = 3;
const USERNET_PARENT_LIVENESS_FD: RawFd = 4;
const USERNET_SENDBUF_BYTES: libc::c_int = 1024 * 1024;
const USERNET_RECVBUF_BYTES: libc::c_int = 1024 * 1024;
const USERNET_READY_MESSAGE: &str = "ready";

pub const USERNET_MAC_ADDRESS: &str = "5a:94:ef:e4:0c:df";

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NetworkMode {
    Nat,
    VzNat,
}

pub enum PreparedNetworkBackend {
    VzNat,
    Usernet {
        vm_socket_fd: Option<OwnedFd>,
        _liveness: OwnedFd,
        _helper: UsernetHelperProcess,
    },
}

impl NetworkMode {
    pub fn parse(value: &str) -> Result<Self, Box<dyn std::error::Error>> {
        match value {
            "nat" => Ok(Self::Nat),
            "vznat" => Ok(Self::VzNat),
            _ => Err(
                format!("Unsupported --network value '{value}'; expected 'nat' or 'vznat'").into(),
            ),
        }
    }

    pub fn prepare(
        &self,
        usernet_helper_path: &Path,
        log_path: Option<&Path>,
    ) -> Result<PreparedNetworkBackend, Box<dyn std::error::Error>> {
        match self {
            NetworkMode::VzNat => Ok(PreparedNetworkBackend::VzNat),
            NetworkMode::Nat => {
                ensure_usernet_helper_extracted(usernet_helper_path);

                let mut command = Command::new(usernet_helper_path);
                command
                    .arg("--fd")
                    .arg(USERNET_HELPER_FD.to_string())
                    .arg("--parent-liveness-fd")
                    .arg(USERNET_PARENT_LIVENESS_FD.to_string())
                    .arg("--mac")
                    .arg(USERNET_MAC_ADDRESS)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());

                let (vm_socket_fd, helper_socket_fd) = create_datagram_pair();
                let helper_raw_fd = helper_socket_fd.as_raw_fd();

                let (helper_liveness_read_fd, helper_liveness_write_fd) = crate::create_pipe();
                let helper_liveness_raw_fd = helper_liveness_read_fd.as_raw_fd();

                unsafe {
                    command.pre_exec(move || {
                        let fds = [
                            (helper_raw_fd, USERNET_HELPER_FD),
                            (helper_liveness_raw_fd, USERNET_PARENT_LIVENESS_FD),
                        ];

                        for (src_fd, dst_fd) in fds {
                            if libc::dup2(src_fd, dst_fd) == -1 {
                                return Err(io::Error::last_os_error());
                            }
                            if src_fd != dst_fd {
                                libc::close(src_fd);
                            }
                        }
                        Ok(())
                    });
                }

                let mut helper = UsernetHelperProcess {
                    child: command.spawn()?,
                };
                helper.wait_until_ready(log_path)?;

                Ok(PreparedNetworkBackend::Usernet {
                    vm_socket_fd: Some(vm_socket_fd),
                    _liveness: helper_liveness_write_fd,
                    _helper: helper,
                })
            }
        }
    }
}

fn configure_usernet_socket(fd: RawFd) {
    for (name, opt, value) in [
        ("SO_SNDBUF", libc::SO_SNDBUF, USERNET_SENDBUF_BYTES),
        ("SO_RCVBUF", libc::SO_RCVBUF, USERNET_RECVBUF_BYTES),
    ] {
        let status = unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                opt,
                (&raw const value).cast::<libc::c_void>(),
                std::mem::size_of_val(&value) as libc::socklen_t,
            )
        };
        if status != 0 {
            eprintln!(
                "Warning: failed to set {name} on usernet socket: {}",
                io::Error::last_os_error()
            );
        }
    }
}

fn create_datagram_pair() -> (OwnedFd, OwnedFd) {
    let (left, right) = UnixDatagram::pair().expect("Failed to create datagram pair");
    configure_usernet_socket(left.as_raw_fd());
    configure_usernet_socket(right.as_raw_fd());
    (left.into(), right.into())
}

fn ensure_usernet_helper_extracted(path: &Path) {
    if fs::read(path).ok().as_deref() == Some(USERNET_HELPER) {
        return;
    }
    let temp_path = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&temp_path, USERNET_HELPER).unwrap();
    fs::set_permissions(&temp_path, fs::Permissions::from_mode(0o755)).unwrap();
    fs::rename(&temp_path, path).unwrap();
}

pub struct UsernetHelperProcess {
    child: Child,
}

impl UsernetHelperProcess {
    fn wait_until_ready(
        &mut self,
        log_path: Option<&Path>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let stdout = self
            .child
            .stdout
            .take()
            .ok_or("vibe-usernet stdout was not captured")?;
        let mut reader = BufReader::new(stdout);
        let mut pollfd = libc::pollfd {
            fd: reader.get_ref().as_raw_fd(),
            events: libc::POLLIN | libc::POLLHUP,
            revents: 0,
        };

        let timeout_ms = 3000;
        let poll_result = unsafe { libc::poll(&raw mut pollfd, 1, timeout_ms) };
        if poll_result < 0 {
            return Err(io::Error::last_os_error().into());
        }
        if poll_result == 0 {
            return Err("Timed out waiting for vibe-usernet to become ready.".into());
        }

        let mut line = String::new();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            let status = self.child.wait()?;
            let stderr = self.read_stderr();

            return Err(format!(
                "vibe-usernet exited before becoming ready ({}){}",
                match status.code() {
                    Some(code) => format!("exit code {code}"),
                    None => "terminated by signal".to_string(),
                },
                stderr
                    .filter(|stderr| !stderr.is_empty())
                    .map(|stderr| format!(": {stderr}"))
                    .unwrap_or_default(),
            )
            .into());
        }

        let line = line.trim_end_matches(&['\r', '\n'][..]);
        if line != USERNET_READY_MESSAGE {
            return Err(format!("vibe-usernet sent unexpected ready line: {line:?}").into());
        }

        // Drain vibe-usernet's stdout/stderr so OS pipe buffers don't fill up and block the helper.
        // Log stderr to a path, if one was provided.
        let mut stderr_sink = log_path
            .and_then(|dir| {
                fs::create_dir_all(dir).ok()?;
                fs::File::create(dir.join("vibe-usernet.log")).ok()
            })
            .map(|file| Box::new(file) as Box<dyn io::Write + Send>)
            .unwrap_or_else(|| Box::new(io::sink()));

        std::thread::spawn(move || {
            let _ = io::copy(&mut reader, &mut io::sink());
        });
        if let Some(mut stderr) = self.child.stderr.take() {
            std::thread::spawn(move || {
                let _ = io::copy(&mut stderr, &mut stderr_sink);
            });
        }

        Ok(())
    }

    fn read_stderr(&mut self) -> Option<String> {
        let mut stderr = String::new();
        self.child.stderr.take()?.read_to_string(&mut stderr).ok()?;
        Some(stderr.trim().to_string())
    }
}
