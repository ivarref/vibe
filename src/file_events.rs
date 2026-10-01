// Forward host file changes into the guest.
//
// Virtiofs (as implemented by Apple's Virtualization framework) doesn't notify the guest when
// files change on the host, so inotify-based tools in the guest (watchexec, cargo watch, vite, ...)
// never see edits made on the Mac. We watch the shared directories with FSEvents on the host and
// send the corresponding guest paths, along with their current mtime, over a dedicated virtio
// console port. A small loop in the guest (file_events_guest.sh) sets each path's mtime to that
// same value, which makes the guest kernel emit an inotify IN_ATTRIB event without changing
// anything.
//
// Writes made by the guest also show up in FSEvents, and the host can't tell them apart from its
// own writes. So the guest records the mtime of every write it makes itself, and ignores forwarded
// paths whose mtime matches.

use std::{
    collections::HashMap,
    ffi::{CStr, CString, c_char, c_void},
    fs,
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Path, PathBuf},
    ptr,
    sync::Mutex,
    thread,
};

const GUEST_SCRIPT: &str = include_str!("file_events_guest.sh");
pub const FILE_EVENTS_PORT_NAME: &str = "vibe-file-events";

pub struct WatchedShare {
    pub host: PathBuf,
    pub guest: PathBuf,
}

type CFAllocatorRef = *const c_void;
type CFArrayRef = *const c_void;
type CFStringRef = *const c_void;
type CFRunLoopRef = *const c_void;
type FSEventStreamRef = *mut c_void;

#[repr(C)]
struct FSEventStreamContext {
    version: isize,
    info: *mut c_void,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
}

type FSEventStreamCallback = extern "C" fn(
    stream: FSEventStreamRef,
    info: *mut c_void,
    num_events: usize,
    event_paths: *mut c_void,
    event_flags: *const u32,
    event_ids: *const u64,
);

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFRunLoopDefaultMode: CFStringRef;
    static kCFTypeArrayCallBacks: c_void;
    fn CFStringCreateWithCString(
        alloc: CFAllocatorRef,
        c_str: *const c_char,
        encoding: u32,
    ) -> CFStringRef;
    fn CFArrayCreate(
        alloc: CFAllocatorRef,
        values: *const *const c_void,
        num_values: isize,
        callbacks: *const c_void,
    ) -> CFArrayRef;
    fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    fn CFRunLoopRun();
}

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn FSEventStreamCreate(
        allocator: CFAllocatorRef,
        callback: FSEventStreamCallback,
        context: *const FSEventStreamContext,
        paths_to_watch: CFArrayRef,
        since_when: u64,
        latency: f64,
        flags: u32,
    ) -> FSEventStreamRef;
    fn FSEventStreamScheduleWithRunLoop(
        stream: FSEventStreamRef,
        run_loop: CFRunLoopRef,
        run_loop_mode: CFStringRef,
    );
    fn FSEventStreamStart(stream: FSEventStreamRef) -> bool;
}

const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
const K_FS_EVENT_STREAM_EVENT_ID_SINCE_NOW: u64 = u64::MAX;
const K_FS_EVENT_STREAM_CREATE_FLAG_NO_DEFER: u32 = 0x02;
const K_FS_EVENT_STREAM_CREATE_FLAG_FILE_EVENTS: u32 = 0x10;
const K_FS_EVENT_STREAM_EVENT_FLAG_MUST_SCAN_SUB_DIRS: u32 = 0x01;
const K_FS_EVENT_STREAM_EVENT_FLAG_ITEM_CREATED: u32 = 0x100;
const K_FS_EVENT_STREAM_EVENT_FLAG_ITEM_REMOVED: u32 = 0x200;
const K_FS_EVENT_STREAM_EVENT_FLAG_ITEM_RENAMED: u32 = 0x800;
const K_FS_EVENT_STREAM_EVENT_FLAG_ITEM_MODIFIED: u32 = 0x1000;

// Only forward changes to contents/names; pure metadata changes are ignored.
const FORWARDED_FLAGS: u32 = K_FS_EVENT_STREAM_EVENT_FLAG_MUST_SCAN_SUB_DIRS
    | K_FS_EVENT_STREAM_EVENT_FLAG_ITEM_CREATED
    | K_FS_EVENT_STREAM_EVENT_FLAG_ITEM_REMOVED
    | K_FS_EVENT_STREAM_EVENT_FLAG_ITEM_RENAMED
    | K_FS_EVENT_STREAM_EVENT_FLAG_ITEM_MODIFIED;

struct Forwarder {
    shares: Vec<WatchedShare>,
    // Guest paths that are shadowed by some other mount (tmpfs masks, unwatched shares nested
    // inside a watched one). Host changes under these must not be forwarded.
    masked_guest_paths: Vec<PathBuf>,
    out: OwnedFd,
    // Last mtime sent per guest path. The guest's touch triggers another FSEvent for the same path,
    // and FSEvents may repeat accumulated flags, so it can look like a modification; since the
    // mtime is unchanged we know not to send it again.
    last_sent: Mutex<HashMap<PathBuf, String>>,
}

impl Forwarder {
    fn guest_path(&self, host_path: &Path) -> Option<PathBuf> {
        let share = self
            .shares
            .iter()
            .filter(|s| host_path.starts_with(&s.host))
            .max_by_key(|s| s.host.as_os_str().len())?;
        let rel = host_path.strip_prefix(&share.host).ok()?;
        // Joining an empty path would add a trailing slash.
        let guest = if rel.as_os_str().is_empty() { share.guest.clone() } else { share.guest.join(rel) };
        let masked = self
            .masked_guest_paths
            .iter()
            .any(|m| m.starts_with(&share.guest) && m != &share.guest && guest.starts_with(m));
        (!masked).then_some(guest)
    }

    fn handle(&self, events: impl Iterator<Item = (PathBuf, u32)>) {
        let mut last_sent = self.last_sent.lock().unwrap();
        if last_sent.len() > 100_000 {
            last_sent.clear();
        }

        let mut paths = Vec::new();
        for (host_path, flags) in events {
            if flags & FORWARDED_FLAGS == 0 {
                continue;
            }
            // A removal changes the parent directory, which is what the guest touches instead.
            // (Except at the root of a share: virtiofs doesn't allow setting its times.)
            if !host_path.exists() {
                if let Some(parent) = host_path.parent() {
                    paths.push(host_path.clone());
                    paths.push(parent.to_path_buf());
                    continue;
                }
            }
            paths.push(host_path);
        }

        let mut buf = Vec::new();
        for host_path in paths {
            let Some(guest) = self.guest_path(&host_path) else {
                continue;
            };
            let bytes = guest.as_os_str().as_bytes();
            if bytes.contains(&b'\n') {
                continue;
            }
            // Same format as the guest's `stat -c %.9Y`; "-" means the path no longer exists.
            let mtime = match fs::symlink_metadata(&host_path) {
                Ok(m) => format!("{}.{:09}", m.mtime(), m.mtime_nsec()),
                Err(_) => "-".to_string(),
            };
            if last_sent.get(&guest) == Some(&mtime) {
                continue;
            }
            buf.extend_from_slice(mtime.as_bytes());
            buf.push(b' ');
            buf.extend_from_slice(bytes);
            buf.push(b'\n');
            last_sent.insert(guest, mtime);
        }

        // The fd is non-blocking: if the guest isn't reading (not booted yet, or overwhelmed),
        // drop events rather than stall.
        let mut written = 0;
        while written < buf.len() {
            let n = unsafe {
                libc::write(
                    self.out.as_raw_fd(),
                    buf[written..].as_ptr() as *const _,
                    buf.len() - written,
                )
            };
            if n <= 0 {
                break;
            }
            written += n as usize;
        }
    }
}

extern "C" fn fsevents_callback(
    _stream: FSEventStreamRef,
    info: *mut c_void,
    num_events: usize,
    event_paths: *mut c_void,
    event_flags: *const u32,
    _event_ids: *const u64,
) {
    let forwarder = unsafe { &*(info as *const Forwarder) };
    let paths = event_paths as *const *const c_char;
    forwarder.handle((0..num_events).map(|i| unsafe {
        let path = CStr::from_ptr(*paths.add(i));
        (
            PathBuf::from(std::ffi::OsStr::from_bytes(path.to_bytes())),
            *event_flags.add(i),
        )
    }));
}

/// The guest side: GUEST_SCRIPT, preceded by the guest paths to watch for the guest's own writes.
pub fn guest_script(shares: &[WatchedShare], masked_guest_paths: &[PathBuf]) -> String {
    let watched: Vec<String> = shares
        .iter()
        .map(|s| shell_quote(&s.guest.to_string_lossy()))
        .collect();
    let masked: Vec<String> = masked_guest_paths
        .iter()
        .filter(|m| shares.iter().any(|s| m.starts_with(&s.guest) && **m != s.guest))
        .map(|m| regex_escape(&m.to_string_lossy()))
        .collect();
    // Matches nothing when there's nothing masked.
    let exclude = if masked.is_empty() {
        "^$".to_string()
    } else {
        format!("^({})(/|$)", masked.join("|"))
    };
    format!(
        "#!/bin/bash\nWATCHED=({})\nEXCLUDE={}\n{GUEST_SCRIPT}",
        watched.join(" "),
        shell_quote(&exclude)
    )
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn regex_escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if r"\.+*?()|[]{}^$".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

pub fn spawn_host_change_forwarder(
    shares: Vec<WatchedShare>,
    masked_guest_paths: Vec<PathBuf>,
    out: OwnedFd,
) {
    // FSEvents reports resolved paths (e.g. /private/tmp rather than /tmp).
    let shares: Vec<WatchedShare> = shares
        .into_iter()
        .filter_map(|s| {
            Some(WatchedShare {
                host: s.host.canonicalize().ok()?,
                guest: s.guest,
            })
        })
        .collect();
    if shares.is_empty() {
        return;
    }

    unsafe {
        let flags = libc::fcntl(out.as_raw_fd(), libc::F_GETFL);
        libc::fcntl(out.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
    }

    thread::spawn(move || {
        let paths: Vec<CString> = shares
            .iter()
            .filter_map(|s| CString::new(s.host.as_os_str().as_bytes()).ok())
            .collect();

        // Leaked on purpose: the stream lives as long as the daemon.
        let forwarder = Box::into_raw(Box::new(Forwarder {
            shares,
            masked_guest_paths,
            out,
            last_sent: Mutex::new(HashMap::new()),
        }));

        unsafe {
            let cf_paths: Vec<CFStringRef> = paths
                .iter()
                .map(|p| {
                    CFStringCreateWithCString(ptr::null(), p.as_ptr(), K_CF_STRING_ENCODING_UTF8)
                })
                .collect();
            let cf_array = CFArrayCreate(
                ptr::null(),
                cf_paths.as_ptr(),
                cf_paths.len() as isize,
                &kCFTypeArrayCallBacks as *const c_void,
            );
            let context = FSEventStreamContext {
                version: 0,
                info: forwarder as *mut c_void,
                retain: ptr::null(),
                release: ptr::null(),
                copy_description: ptr::null(),
            };
            let stream = FSEventStreamCreate(
                ptr::null(),
                fsevents_callback,
                &context,
                cf_array,
                K_FS_EVENT_STREAM_EVENT_ID_SINCE_NOW,
                0.05,
                K_FS_EVENT_STREAM_CREATE_FLAG_FILE_EVENTS | K_FS_EVENT_STREAM_CREATE_FLAG_NO_DEFER,
            );
            if stream.is_null() {
                eprintln!("[file events] Failed to create FSEvents stream");
                return;
            }
            FSEventStreamScheduleWithRunLoop(stream, CFRunLoopGetCurrent(), kCFRunLoopDefaultMode);
            if !FSEventStreamStart(stream) {
                eprintln!("[file events] Failed to start FSEvents stream");
                return;
            }
            CFRunLoopRun();
        }
    });
}

