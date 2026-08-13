use super::{
    cached_avatar_path, discovery_error, display_id_for_peer, normalize_avatar_hash,
    normalize_nickname, remove_discovered_service, unix_time_ms, upsert_discovered_peer,
    NetworkError, PeerState, Shared,
};
use libc::{c_char, c_int, c_void, poll, pollfd, POLLERR, POLLHUP, POLLIN, POLLNVAL};
use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString};
use std::io;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::ptr;
use std::slice;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tossit_identity::DeviceIdentity;
use tossit_protocol::PROTOCOL_VERSION;
use tossit_storage::TrustState;

type DNSServiceRef = *mut c_void;
type DNSServiceFlags = u32;
type DNSServiceErrorType = i32;

const SERVICE_REGTYPE: &str = "_tossit._tcp";
const DNS_SERVICE_NO_ERROR: DNSServiceErrorType = 0;
const DNS_SERVICE_FLAGS_ADD: DNSServiceFlags = 0x2;
const DNS_SERVICE_INTERFACE_ANY: u32 = 0;
const POLL_INTERVAL_MS: c_int = 200;
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(4);

type DNSServiceRegisterReply = unsafe extern "C" fn(
    DNSServiceRef,
    DNSServiceFlags,
    DNSServiceErrorType,
    *const c_char,
    *const c_char,
    *const c_char,
    *mut c_void,
);

type DNSServiceBrowseReply = unsafe extern "C" fn(
    DNSServiceRef,
    DNSServiceFlags,
    u32,
    DNSServiceErrorType,
    *const c_char,
    *const c_char,
    *const c_char,
    *mut c_void,
);

type DNSServiceResolveReply = unsafe extern "C" fn(
    DNSServiceRef,
    DNSServiceFlags,
    u32,
    DNSServiceErrorType,
    *const c_char,
    *const c_char,
    u16,
    u16,
    *const u8,
    *mut c_void,
);

#[link(name = "System")]
unsafe extern "C" {
    fn DNSServiceRegister(
        service_ref: *mut DNSServiceRef,
        flags: DNSServiceFlags,
        interface_index: u32,
        name: *const c_char,
        regtype: *const c_char,
        domain: *const c_char,
        host: *const c_char,
        port: u16,
        txt_len: u16,
        txt_record: *const c_void,
        callback: Option<DNSServiceRegisterReply>,
        context: *mut c_void,
    ) -> DNSServiceErrorType;

    fn DNSServiceBrowse(
        service_ref: *mut DNSServiceRef,
        flags: DNSServiceFlags,
        interface_index: u32,
        regtype: *const c_char,
        domain: *const c_char,
        callback: Option<DNSServiceBrowseReply>,
        context: *mut c_void,
    ) -> DNSServiceErrorType;

    fn DNSServiceResolve(
        service_ref: *mut DNSServiceRef,
        flags: DNSServiceFlags,
        interface_index: u32,
        name: *const c_char,
        regtype: *const c_char,
        domain: *const c_char,
        callback: Option<DNSServiceResolveReply>,
        context: *mut c_void,
    ) -> DNSServiceErrorType;

    fn DNSServiceRefSockFD(service_ref: DNSServiceRef) -> c_int;
    fn DNSServiceProcessResult(service_ref: DNSServiceRef) -> DNSServiceErrorType;
    fn DNSServiceRefDeallocate(service_ref: DNSServiceRef);
}

pub(super) struct SystemBonjour {
    shared: Arc<Shared>,
    registration: Mutex<Option<DnsServiceRunner>>,
    browser: DnsServiceRunner,
}

impl SystemBonjour {
    pub(super) fn start(shared: Arc<Shared>) -> Result<Self, NetworkError> {
        let nickname = shared.nickname.read().expect("nickname lock").clone();
        let avatar_hash = shared
            .avatar
            .read()
            .expect("avatar lock")
            .as_ref()
            .map(|avatar| avatar.hash.clone());
        let registration = register_service(&shared, &nickname, avatar_hash.as_deref())?;
        let browser = browse_services(Arc::clone(&shared))?;
        Ok(Self {
            shared,
            registration: Mutex::new(Some(registration)),
            browser,
        })
    }

    pub(super) fn replace_registration(
        &self,
        current_nickname: &str,
        current_avatar_hash: Option<&str>,
        replacement_nickname: &str,
        replacement_avatar_hash: Option<&str>,
    ) -> Result<(), NetworkError> {
        let previous = self
            .registration
            .lock()
            .expect("Bonjour registration lock")
            .take();
        drop(previous);

        match register_service(&self.shared, replacement_nickname, replacement_avatar_hash) {
            Ok(replacement) => {
                *self.registration.lock().expect("Bonjour registration lock") = Some(replacement);
                Ok(())
            }
            Err(error) => {
                let restored =
                    register_service(&self.shared, current_nickname, current_avatar_hash).ok();
                *self.registration.lock().expect("Bonjour registration lock") = restored;
                Err(error)
            }
        }
    }
}

impl Drop for SystemBonjour {
    fn drop(&mut self) {
        self.registration
            .get_mut()
            .expect("Bonjour registration lock")
            .take();
        self.browser.stop_and_join();
    }
}

struct DnsServiceRunner {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    shared_keepalive: Option<Arc<Shared>>,
}

impl DnsServiceRunner {
    fn spawn(
        service_ref: DNSServiceRef,
        thread_name: &str,
        shared_keepalive: Option<Arc<Shared>>,
    ) -> Result<Self, NetworkError> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let raw_ref = service_ref as usize;
        let join = match thread::Builder::new()
            .name(thread_name.to_owned())
            .spawn(move || {
                let service_ref = raw_ref as DNSServiceRef;
                process_until_stopped(service_ref, &thread_stop);
                unsafe { DNSServiceRefDeallocate(service_ref) };
            }) {
            Ok(join) => join,
            Err(error) => {
                unsafe { DNSServiceRefDeallocate(service_ref) };
                return Err(NetworkError::Io(error));
            }
        };
        Ok(Self {
            stop,
            join: Some(join),
            shared_keepalive,
        })
    }

    fn stop_and_join(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
        self.shared_keepalive.take();
    }
}

impl Drop for DnsServiceRunner {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

fn register_service(
    shared: &Arc<Shared>,
    nickname: &str,
    avatar_hash: Option<&str>,
) -> Result<DnsServiceRunner, NetworkError> {
    let instance_name = CString::new(format!("TossIt {}", shared.identity_summary.display_id))
        .map_err(discovery_error)?;
    let regtype = CString::new(SERVICE_REGTYPE).expect("Bonjour service type has no NUL");
    let protocol_version = PROTOCOL_VERSION.to_string();
    let avatar_hash = avatar_hash.unwrap_or_default();
    let txt_record = encode_txt_record(&[
        ("peer", shared.identity_summary.peer_id.as_str()),
        ("display", shared.identity_summary.display_id.as_str()),
        ("nickname", nickname),
        ("avatar", avatar_hash),
        ("key", shared.identity_summary.public_key.as_str()),
        ("cert", shared.certificate_fingerprint.as_str()),
        ("v", protocol_version.as_str()),
        ("caps", "text,attachment-v1,avatar-v1"),
    ])?;
    let txt_len = u16::try_from(txt_record.len())
        .map_err(|_| discovery_error("Bonjour TXT record is too large"))?;
    let mut service_ref = ptr::null_mut();
    let error = unsafe {
        DNSServiceRegister(
            &mut service_ref,
            0,
            DNS_SERVICE_INTERFACE_ANY,
            instance_name.as_ptr(),
            regtype.as_ptr(),
            ptr::null(),
            ptr::null(),
            shared.listening_port.to_be(),
            txt_len,
            txt_record.as_ptr().cast(),
            Some(register_callback),
            ptr::null_mut(),
        )
    };
    check_dns_service(error, "register Bonjour service")?;
    DnsServiceRunner::spawn(service_ref, "tossit-bonjour-register", None)
}

fn browse_services(shared: Arc<Shared>) -> Result<DnsServiceRunner, NetworkError> {
    let regtype = CString::new(SERVICE_REGTYPE).expect("Bonjour service type has no NUL");
    let context = Arc::as_ptr(&shared).cast_mut().cast();
    let mut service_ref = ptr::null_mut();
    let error = unsafe {
        DNSServiceBrowse(
            &mut service_ref,
            0,
            DNS_SERVICE_INTERFACE_ANY,
            regtype.as_ptr(),
            ptr::null(),
            Some(browse_callback),
            context,
        )
    };
    check_dns_service(error, "browse Bonjour services")?;
    DnsServiceRunner::spawn(service_ref, "tossit-bonjour-browser", Some(shared))
}

unsafe extern "C" fn register_callback(
    _service_ref: DNSServiceRef,
    _flags: DNSServiceFlags,
    error: DNSServiceErrorType,
    _name: *const c_char,
    _regtype: *const c_char,
    _domain: *const c_char,
    _context: *mut c_void,
) {
    if error != DNS_SERVICE_NO_ERROR {
        eprintln!("TossIt Bonjour registration failed asynchronously: {error}");
    }
}

unsafe extern "C" fn browse_callback(
    _service_ref: DNSServiceRef,
    flags: DNSServiceFlags,
    interface_index: u32,
    error: DNSServiceErrorType,
    service_name: *const c_char,
    regtype: *const c_char,
    domain: *const c_char,
    context: *mut c_void,
) {
    if error != DNS_SERVICE_NO_ERROR {
        eprintln!("TossIt Bonjour browse failed asynchronously: {error}");
        return;
    }
    if context.is_null() {
        return;
    }
    let shared = unsafe { &*(context as *const Shared) };
    if shared.shutdown.load(Ordering::Relaxed) {
        return;
    }
    let (Some(service_name), Some(regtype), Some(domain)) =
        (c_string(service_name), c_string(regtype), c_string(domain))
    else {
        return;
    };
    let service_key = service_key(&service_name, &regtype, &domain, interface_index);
    if flags & DNS_SERVICE_FLAGS_ADD == 0 {
        remove_discovered_service(shared, &service_key);
        return;
    }
    let shared_pointer = shared as *const Shared;
    unsafe { Arc::increment_strong_count(shared_pointer) };
    let shared = unsafe { Arc::from_raw(shared_pointer) };
    if let Err(error) = resolve_service(
        shared,
        &service_name,
        &regtype,
        &domain,
        interface_index,
        service_key,
    ) {
        eprintln!("TossIt could not resolve Bonjour service: {error}");
    }
}

struct ResolveContext {
    shared: Arc<Shared>,
    service_key: String,
}

fn resolve_service(
    shared: Arc<Shared>,
    service_name: &str,
    regtype: &str,
    domain: &str,
    interface_index: u32,
    service_key: String,
) -> Result<(), NetworkError> {
    let service_name = CString::new(service_name).map_err(discovery_error)?;
    let regtype = CString::new(regtype).map_err(discovery_error)?;
    let domain = CString::new(domain).map_err(discovery_error)?;
    let context = Box::new(ResolveContext {
        shared,
        service_key,
    });
    let context_ptr = Box::into_raw(context);
    let mut service_ref = ptr::null_mut();
    let error = unsafe {
        DNSServiceResolve(
            &mut service_ref,
            0,
            interface_index,
            service_name.as_ptr(),
            regtype.as_ptr(),
            domain.as_ptr(),
            Some(resolve_callback),
            context_ptr.cast(),
        )
    };
    if error != DNS_SERVICE_NO_ERROR {
        unsafe { drop(Box::from_raw(context_ptr)) };
        return Err(discovery_error(format!(
            "resolve Bonjour service returned {error}"
        )));
    }

    let raw_ref = service_ref as usize;
    let raw_context = context_ptr as usize;
    match thread::Builder::new()
        .name("tossit-bonjour-resolve".to_owned())
        .spawn(move || {
            let service_ref = raw_ref as DNSServiceRef;
            process_one(service_ref, RESOLVE_TIMEOUT);
            unsafe {
                DNSServiceRefDeallocate(service_ref);
                drop(Box::from_raw(raw_context as *mut ResolveContext));
            }
        }) {
        Ok(_) => Ok(()),
        Err(error) => {
            unsafe {
                DNSServiceRefDeallocate(service_ref);
                drop(Box::from_raw(context_ptr));
            }
            Err(NetworkError::Io(error))
        }
    }
}

unsafe extern "C" fn resolve_callback(
    _service_ref: DNSServiceRef,
    _flags: DNSServiceFlags,
    _interface_index: u32,
    error: DNSServiceErrorType,
    _fullname: *const c_char,
    hosttarget: *const c_char,
    port: u16,
    txt_len: u16,
    txt_record: *const u8,
    context: *mut c_void,
) {
    if error != DNS_SERVICE_NO_ERROR || context.is_null() {
        if error != DNS_SERVICE_NO_ERROR {
            eprintln!("TossIt Bonjour resolve failed asynchronously: {error}");
        }
        return;
    }
    let context = unsafe { &*(context as *const ResolveContext) };
    if context.shared.shutdown.load(Ordering::Relaxed) {
        return;
    }
    let Some(hosttarget) = c_string(hosttarget) else {
        return;
    };
    if txt_record.is_null() && txt_len != 0 {
        return;
    }
    let txt = if txt_len == 0 {
        &[][..]
    } else {
        unsafe { slice::from_raw_parts(txt_record, usize::from(txt_len)) }
    };
    let Some(properties) = decode_txt_record(txt) else {
        return;
    };
    let Some(endpoint) = resolve_ipv4_endpoint(&hosttarget, u16::from_be(port)) else {
        return;
    };
    let Some(peer) =
        peer_from_properties(&properties, endpoint, &context.service_key, &context.shared)
    else {
        return;
    };
    upsert_discovered_peer(&context.shared, &context.service_key, peer);
}

fn peer_from_properties(
    properties: &HashMap<String, String>,
    endpoint: SocketAddr,
    service_key: &str,
    shared: &Shared,
) -> Option<PeerState> {
    let peer_id = properties.get("peer")?.to_owned();
    if peer_id == shared.identity_summary.peer_id {
        return None;
    }
    let display_id = properties.get("display")?.to_owned();
    let public_key = properties.get("key")?.to_owned();
    let certificate_fingerprint = properties.get("cert")?.to_owned();
    let version = properties.get("v")?;
    if version != &PROTOCOL_VERSION.to_string()
        || DeviceIdentity::peer_id_for_public_key(&public_key).ok()? != peer_id
        || display_id_for_peer(&peer_id)? != display_id
        || certificate_fingerprint.len() != 64
    {
        return None;
    }
    let nickname = properties.get("nickname").map(String::as_str).unwrap_or("");
    let avatar_hash = properties
        .get("avatar")
        .and_then(|value| normalize_avatar_hash(value));
    let mut service_fullnames = HashSet::new();
    service_fullnames.insert(service_key.to_owned());
    let avatar_path = avatar_hash
        .as_deref()
        .and_then(|hash| cached_avatar_path(shared, &peer_id, hash));
    Some(PeerState {
        alias: normalize_nickname(nickname, &display_id),
        peer_id,
        display_id,
        public_key,
        certificate_fingerprint,
        endpoint: Some(endpoint),
        service_fullnames,
        last_seen_unix_ms: unix_time_ms(),
        trust_state: TrustState::Discovered,
        avatar_path,
        avatar_hash,
    })
}

fn resolve_ipv4_endpoint(host: &str, port: u16) -> Option<SocketAddr> {
    let mut endpoints = (host, port)
        .to_socket_addrs()
        .ok()?
        .filter(|endpoint| matches!(endpoint.ip(), IpAddr::V4(_)))
        .collect::<Vec<_>>();
    endpoints.sort_by_key(|endpoint| {
        let address = endpoint.ip();
        let is_link_local = match address {
            IpAddr::V4(address) => address.is_link_local(),
            IpAddr::V6(_) => false,
        };
        (
            address.is_unspecified() || address.is_multicast(),
            address.is_loopback(),
            is_link_local,
            address,
        )
    });
    endpoints.into_iter().find(|endpoint| {
        let address = endpoint.ip();
        !address.is_unspecified() && !address.is_multicast()
    })
}

fn encode_txt_record(properties: &[(&str, &str)]) -> Result<Vec<u8>, NetworkError> {
    let mut record = Vec::new();
    for (key, value) in properties {
        let entry = format!("{key}={value}");
        let entry_len = u8::try_from(entry.len())
            .map_err(|_| discovery_error(format!("Bonjour TXT field {key} is too large")))?;
        record.push(entry_len);
        record.extend_from_slice(entry.as_bytes());
    }
    Ok(record)
}

fn decode_txt_record(record: &[u8]) -> Option<HashMap<String, String>> {
    let mut properties = HashMap::new();
    let mut offset = 0;
    while offset < record.len() {
        let length = usize::from(*record.get(offset)?);
        offset += 1;
        let end = offset.checked_add(length)?;
        let entry = std::str::from_utf8(record.get(offset..end)?).ok()?;
        offset = end;
        let (key, value) = entry.split_once('=')?;
        if key.is_empty() {
            return None;
        }
        properties.insert(key.to_owned(), value.to_owned());
    }
    Some(properties)
}

fn service_key(name: &str, regtype: &str, domain: &str, interface_index: u32) -> String {
    format!(
        "{}.{}.{}#{interface_index}",
        name.trim_end_matches('.'),
        regtype.trim_matches('.'),
        domain.trim_start_matches('.')
    )
}

fn process_until_stopped(service_ref: DNSServiceRef, stop: &AtomicBool) {
    let socket = unsafe { DNSServiceRefSockFD(service_ref) };
    if socket < 0 {
        eprintln!("TossIt Bonjour returned an invalid event socket");
        return;
    }
    while !stop.load(Ordering::Relaxed) {
        match poll_service(socket, POLL_INTERVAL_MS) {
            Ok(false) => continue,
            Ok(true) => {
                let error = unsafe { DNSServiceProcessResult(service_ref) };
                if error != DNS_SERVICE_NO_ERROR {
                    eprintln!("TossIt Bonjour event processing failed: {error}");
                    break;
                }
            }
            Err(error) => {
                eprintln!("TossIt Bonjour event polling failed: {error}");
                break;
            }
        }
    }
}

fn process_one(service_ref: DNSServiceRef, timeout: Duration) {
    let socket = unsafe { DNSServiceRefSockFD(service_ref) };
    if socket < 0 {
        return;
    }
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let timeout_ms = i32::try_from(remaining.as_millis().min(POLL_INTERVAL_MS as u128))
            .unwrap_or(POLL_INTERVAL_MS);
        match poll_service(socket, timeout_ms) {
            Ok(false) => continue,
            Ok(true) => {
                let error = unsafe { DNSServiceProcessResult(service_ref) };
                if error != DNS_SERVICE_NO_ERROR {
                    eprintln!("TossIt Bonjour resolve processing failed: {error}");
                }
                return;
            }
            Err(_) => return,
        }
    }
}

fn poll_service(socket: c_int, timeout_ms: c_int) -> io::Result<bool> {
    let mut descriptor = pollfd {
        fd: socket,
        events: POLLIN,
        revents: 0,
    };
    loop {
        let result = unsafe { poll(&mut descriptor, 1, timeout_ms) };
        if result > 0 {
            if descriptor.revents & (POLLERR | POLLHUP | POLLNVAL) != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "Bonjour event socket closed",
                ));
            }
            return Ok(descriptor.revents & POLLIN != 0);
        }
        if result == 0 {
            return Ok(false);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn check_dns_service(error: DNSServiceErrorType, operation: &str) -> Result<(), NetworkError> {
    if error == DNS_SERVICE_NO_ERROR {
        Ok(())
    } else {
        Err(discovery_error(format!("{operation} returned {error}")))
    }
}

unsafe fn c_string(value: *const c_char) -> Option<String> {
    if value.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(value) }
        .to_str()
        .ok()
        .map(str::to_owned)
}
