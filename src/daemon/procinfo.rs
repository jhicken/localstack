//! Process metrics straight from the kernel via libproc (no `ps` forks).
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

fn timebase() -> (u64, u64) {
    static TB: OnceLock<(u64, u64)> = OnceLock::new();
    #[allow(deprecated)]
    *TB.get_or_init(|| {
        let mut info = libc::mach_timebase_info { numer: 0, denom: 0 };
        #[allow(deprecated)]
        let rc = unsafe { libc::mach_timebase_info(&mut info) };
        if rc != 0 || info.denom == 0 {
            (1, 1)
        } else {
            (info.numer as u64, info.denom as u64)
        }
    })
}

pub fn children(pid: i32) -> Vec<i32> {
    let mut buf = vec![0i32; 256];
    loop {
        let bytes = (buf.len() * 4) as libc::c_int;
        let n = unsafe { libc::proc_listchildpids(pid, buf.as_mut_ptr() as *mut libc::c_void, bytes) };
        if n <= 0 {
            return vec![];
        }
        let n = n as usize;
        // Some macOS versions return a byte count, others a pid count.
        let count = if n > buf.len() { n / 4 } else { n };
        if count >= buf.len() && buf.len() < 65_536 {
            buf.resize(buf.len() * 4, 0);
            continue;
        }
        buf.truncate(count.min(buf.len()));
        buf.retain(|p| *p > 0);
        return buf;
    }
}

/// `roots` plus all their descendants.
pub fn descendants(roots: &[i32]) -> Vec<i32> {
    let mut seen: HashSet<i32> = HashSet::new();
    let mut stack: Vec<i32> = roots.to_vec();
    let mut out = vec![];
    while let Some(p) = stack.pop() {
        if p <= 0 || !seen.insert(p) {
            continue;
        }
        out.push(p);
        stack.extend(children(p));
    }
    out
}

/// (total cpu time in ns, memory bytes). Memory is the physical footprint — Activity Monitor's
/// "Memory" column — rather than RSS, which counts shared pages (libraries, forked parents' pages)
/// in every process and so inflates a process tree's sum. Falls back to RSS if unavailable.
pub fn task_info(pid: i32) -> Option<(u64, u64)> {
    let mut ti: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
    let n = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTASKINFO, 0, &mut ti as *mut _ as *mut libc::c_void, size) };
    if n != size {
        return None;
    }
    let (numer, denom) = timebase();
    let ticks = ti.pti_total_user + ti.pti_total_system;
    let mem = phys_footprint(pid).unwrap_or(ti.pti_resident_size);
    Some(((ticks as u128 * numer as u128 / denom as u128) as u64, mem))
}

fn phys_footprint(pid: i32) -> Option<u64> {
    let mut ri: libc::rusage_info_v0 = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V0, &mut ri as *mut _ as *mut libc::rusage_info_t) };
    (rc == 0 && ri.ri_phys_footprint > 0).then_some(ri.ri_phys_footprint)
}

/// Process start time (ms since epoch).
pub fn start_time(pid: i32) -> Option<i64> {
    let mut bi: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let n = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, &mut bi as *mut _ as *mut libc::c_void, size) };
    if n != size {
        return None;
    }
    Some(bi.pbi_start_tvsec as i64 * 1000 + bi.pbi_start_tvusec as i64 / 1000)
}

/// Turns cumulative CPU time into a % of one core between successive samples.
#[derive(Default)]
pub struct CpuSampler {
    prev: HashMap<i32, (u64, u64)>, // pid -> (cpu ns, start time ms) for pid-reuse detection
    last_wall: Option<std::time::Instant>,
}

pub struct Sample {
    pub cpu: f64,
    pub mem: f64,
}

impl CpuSampler {
    /// Sample many pid groups at once; returns one aggregate per group.
    pub fn sample(&mut self, groups: &[Vec<i32>]) -> Vec<Sample> {
        let now = std::time::Instant::now();
        let dt_ns = self.last_wall.map(|t| now.duration_since(t).as_nanos() as f64).unwrap_or(0.0);
        self.last_wall = Some(now);
        let mut next = HashMap::new();
        let out = groups
            .iter()
            .map(|pids| {
                let mut s = Sample { cpu: 0.0, mem: 0.0 };
                for &pid in pids {
                    let Some((cpu_ns, mem)) = task_info(pid) else { continue };
                    let st = start_time(pid).unwrap_or(0) as u64;
                    s.mem += mem as f64;
                    if let Some(&(prev, pst)) = self.prev.get(&pid) {
                        if pst == st && dt_ns > 0.0 && cpu_ns >= prev {
                            s.cpu += (cpu_ns - prev) as f64 / dt_ns * 100.0;
                        }
                    }
                    next.insert(pid, (cpu_ns, st));
                }
                s
            })
            .collect();
        self.prev = next;
        out
    }
}

/// Whole-machine CPU and memory, for the "how loaded is this Mac" view.
#[derive(Default)]
pub struct HostSampler {
    prev: Option<[u64; 4]>,
}

pub fn host_cores() -> u32 {
    std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(1)
}

pub fn host_mem_total() -> f64 {
    let mut v: u64 = 0;
    let mut len = std::mem::size_of::<u64>();
    let rc = unsafe { libc::sysctlbyname(c"hw.memsize".as_ptr(), &mut v as *mut u64 as *mut _, &mut len, std::ptr::null_mut(), 0) };
    if rc == 0 { v as f64 } else { 0.0 }
}

#[allow(deprecated)]
fn host_port() -> libc::mach_port_t {
    static PORT: OnceLock<libc::mach_port_t> = OnceLock::new();
    *PORT.get_or_init(|| unsafe { libc::mach_host_self() })
}

/// Cumulative (user, system, idle, nice) CPU ticks across all cores.
fn host_ticks() -> Option<[u64; 4]> {
    let mut info = libc::host_cpu_load_info { cpu_ticks: [0; 4] };
    let mut count = libc::HOST_CPU_LOAD_INFO_COUNT;
    let rc = unsafe { libc::host_statistics(host_port(), libc::HOST_CPU_LOAD_INFO, &mut info as *mut _ as libc::host_info_t, &mut count) };
    (rc == 0).then(|| info.cpu_ticks.map(|t| t as u64))
}

/// "Memory Used" as Activity Monitor computes it: app memory + wired + compressed.
pub fn host_mem_used() -> Option<f64> {
    let mut vm: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
    let mut count = libc::HOST_VM_INFO64_COUNT;
    let rc = unsafe { libc::host_statistics64(host_port(), libc::HOST_VM_INFO64, &mut vm as *mut _ as libc::host_info64_t, &mut count) };
    if rc != 0 {
        return None;
    }
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as f64;
    let app = (vm.internal_page_count as f64 - vm.purgeable_count as f64).max(0.0);
    Some((app + vm.wire_count as f64 + vm.compressor_page_count as f64) * page)
}

impl HostSampler {
    /// Machine CPU busy % (0–100 of all cores) since the previous call.
    pub fn cpu(&mut self) -> Option<f64> {
        let now = host_ticks()?;
        let prev = self.prev.replace(now)?;
        let d: Vec<u64> = now.iter().zip(prev).map(|(a, b)| a.saturating_sub(b)).collect();
        let total: u64 = d.iter().sum();
        (total > 0).then(|| (total - d[2]) as f64 / total as f64 * 100.0)
    }
}

/// Cumulative per-pid network bytes from `nettop` (rx, tx).
pub async fn network_table() -> Option<HashMap<i32, (f64, f64)>> {
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        tokio::process::Command::new("/usr/bin/nettop")
            .args(["-P", "-L", "1", "-x", "-J", "bytes_in,bytes_out"])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    Some(parse_nettop(&String::from_utf8_lossy(&out.stdout)))
}

pub fn parse_nettop(text: &str) -> HashMap<i32, (f64, f64)> {
    let mut m = HashMap::new();
    for line in text.lines() {
        let line = line.trim_end().trim_end_matches(',');
        let mut parts = line.rsplitn(3, ',');
        let (Some(tx), Some(rx), Some(name)) = (parts.next(), parts.next(), parts.next()) else { continue };
        let Some(pid) = name.rsplit('.').next().and_then(|p| p.parse::<i32>().ok()) else { continue };
        if let (Ok(rx), Ok(tx)) = (rx.parse::<f64>(), tx.parse::<f64>()) {
            m.insert(pid, (rx, tx));
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn self_info() {
        let me = std::process::id() as i32;
        let (cpu, rss) = task_info(me).unwrap();
        assert!(rss > 0 && cpu > 0);
        let st = start_time(me).unwrap();
        assert!((crate::paths::now_ms() - st).abs() < 3_600_000);
        let mut child = std::process::Command::new("sleep").arg("5").spawn().unwrap();
        assert!(children(me).contains(&(child.id() as i32)));
        assert!(descendants(&[me]).contains(&(child.id() as i32)));
        child.kill().ok();
        child.wait().ok();
    }
    #[test]
    fn nettop() {
        let m = parse_nettop(",bytes_in,bytes_out,\nlaunchd.1,0,0,\nGoogle Chrome H.123,4567,890,\n");
        assert_eq!(m[&123], (4567.0, 890.0));
        assert_eq!(m[&1], (0.0, 0.0));
    }
}

/// One listening TCP socket: bound address and port.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Listen {
    pub ip: String,
    pub port: u16,
}

impl Listen {
    pub fn loopback(&self) -> bool {
        self.ip.starts_with("127.") || self.ip == "::1" || self.ip == "localhost"
    }
}

/// Listening TCP sockets of every process we can see, by pid (one `lsof` run).
pub async fn listening_ports() -> Option<HashMap<i32, Vec<Listen>>> {
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::process::Command::new("/usr/sbin/lsof")
            .args(["-nP", "-iTCP", "-sTCP:LISTEN", "-F", "pn"])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    Some(parse_lsof(&String::from_utf8_lossy(&out.stdout)))
}

pub fn parse_lsof(text: &str) -> HashMap<i32, Vec<Listen>> {
    let mut m: HashMap<i32, Vec<Listen>> = HashMap::new();
    let mut pid = 0;
    for line in text.lines() {
        if let Some(p) = line.strip_prefix('p') {
            pid = p.parse().unwrap_or(0);
        } else if let Some(n) = line.strip_prefix('n') {
            let Some((ip, port)) = n.rsplit_once(':') else { continue };
            let Ok(port) = port.parse::<u16>() else { continue };
            let ip = ip.trim_start_matches('[').trim_end_matches(']');
            let ip = if ip == "*" { "0.0.0.0" } else { ip }.to_string();
            let v = m.entry(pid).or_default();
            let l = Listen { ip, port };
            if pid > 0 && !v.contains(&l) {
                v.push(l);
            }
        }
    }
    m
}

/// Ports a set of listeners serves on → (port, loopback-only), sorted and de-duplicated.
pub fn ports_of<'a>(listens: impl IntoIterator<Item = &'a Listen>) -> Vec<(u16, bool)> {
    let mut by: std::collections::BTreeMap<u16, bool> = std::collections::BTreeMap::new();
    for l in listens {
        let e = by.entry(l.port).or_insert(true);
        *e = *e && l.loopback();
    }
    by.into_iter().collect()
}

#[cfg(test)]
mod port_tests {
    use super::*;

    #[test]
    fn lsof_parsing() {
        let m = parse_lsof("p100\nf5\nn*:3000\nf6\nn[::1]:3000\np200\nf3\nn127.0.0.1:5432\nf4\nn[::1]:5432\n");
        assert_eq!(ports_of(&m[&100]), vec![(3000, false)]);
        assert_eq!(ports_of(&m[&200]), vec![(5432, true)]);
    }

    #[tokio::test]
    async fn finds_own_listener() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let m = listening_ports().await.expect("lsof failed or timed out");
        let mine = m.get(&(std::process::id() as i32)).cloned().unwrap_or_default();
        assert!(mine.iter().any(|x| x.port == port && x.loopback()), "{mine:?}");
    }
}

/// Every pid on the machine.
pub fn all_pids() -> Vec<i32> {
    let n = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if n <= 0 {
        return vec![];
    }
    let mut buf = vec![0i32; n as usize + 64];
    let got = unsafe { libc::proc_listallpids(buf.as_mut_ptr() as *mut libc::c_void, (buf.len() * 4) as libc::c_int) };
    if got <= 0 {
        return vec![];
    }
    buf.truncate((got as usize).min(buf.len()));
    buf.retain(|p| *p > 0);
    buf
}

/// The pid (of this user's processes) holding the client end of a local TCP connection
/// `127.0.0.1:client_port → :server_port`. Other users' processes can't be inspected, so `None` also
/// means "not this user". `hint` pids are checked first.
pub fn tcp_client_owner(client_port: u16, server_port: u16, hint: &[i32]) -> Option<i32> {
    const PROC_PIDLISTFDS: libc::c_int = 1;
    const PROC_PIDFDSOCKETINFO: libc::c_int = 3;
    const PROX_FDTYPE_SOCKET: u32 = 2;
    const SOCKINFO_TCP: i32 = 2;
    // Offsets in `struct socket_fdinfo` (sys/proc_info.h), stable since 10.x.
    const SIZE: usize = 792;
    const SOI_KIND: usize = 256;
    const FPORT: usize = 264;
    const LPORT: usize = 268;
    let me = unsafe { libc::getuid() };
    let owns = |pid: i32| -> bool {
        let mut bi: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let n = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, &mut bi as *mut _ as *mut libc::c_void, size) };
        if n != size || bi.pbi_uid != me {
            return false;
        }
        let mut fds = vec![0u64; 512];
        let n = unsafe { libc::proc_pidinfo(pid, PROC_PIDLISTFDS, 0, fds.as_mut_ptr() as *mut libc::c_void, (fds.len() * 8) as libc::c_int) };
        if n <= 0 {
            return false;
        }
        let count = n as usize / 8;
        let count = if count == fds.len() {
            fds = vec![0u64; 8192];
            let n = unsafe { libc::proc_pidinfo(pid, PROC_PIDLISTFDS, 0, fds.as_mut_ptr() as *mut libc::c_void, (fds.len() * 8) as libc::c_int) };
            if n <= 0 {
                return false;
            }
            n as usize / 8
        } else {
            count
        };
        let mut info = vec![0u8; SIZE];
        for raw in &fds[..count] {
            // struct proc_fdinfo { int32_t proc_fd; uint32_t proc_fdtype; }
            let fd = (*raw & 0xffff_ffff) as i32;
            let kind = (*raw >> 32) as u32;
            if kind != PROX_FDTYPE_SOCKET {
                continue;
            }
            let n = unsafe { libc::proc_pidfdinfo(pid, fd, PROC_PIDFDSOCKETINFO, info.as_mut_ptr() as *mut libc::c_void, SIZE as libc::c_int) };
            if n as usize != SIZE {
                continue;
            }
            let rd = |o: usize| i32::from_ne_bytes([info[o], info[o + 1], info[o + 2], info[o + 3]]);
            if rd(SOI_KIND) != SOCKINFO_TCP {
                continue;
            }
            let (lport, fport) = (u16::from_be(rd(LPORT) as u16), u16::from_be(rd(FPORT) as u16));
            if lport == client_port && fport == server_port {
                return true;
            }
        }
        false
    };
    let rest = all_pids();
    hint.iter().copied().chain(rest.into_iter().filter(|p| !hint.contains(p))).find(|&p| p != std::process::id() as i32 && owns(p))
}

/// Short process name (as in Activity Monitor), or None if we can't see the process.
pub fn proc_name(pid: i32) -> Option<String> {
    let mut buf = [0u8; 256];
    let n = unsafe { libc::proc_name(pid, buf.as_mut_ptr() as *mut libc::c_void, buf.len() as u32) };
    (n > 0).then(|| String::from_utf8_lossy(&buf[..n as usize]).into_owned())
}

#[cfg(test)]
mod pid_tests {
    #[test]
    fn sees_self() {
        let me = std::process::id() as i32;
        assert!(super::all_pids().contains(&me));
        assert!(super::proc_name(me).is_some());
    }

    #[test]
    fn finds_client_socket_owner() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        // The client side lives in a child process so it isn't this (excluded) pid.
        let mut child = std::process::Command::new("/usr/bin/nc").args(["127.0.0.1", &port.to_string()]).stdin(std::process::Stdio::piped()).spawn().unwrap();
        let (_s, peer) = l.accept().unwrap();
        let owner = super::tcp_client_owner(peer.port(), port, &[]);
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(owner, Some(child.id() as i32));
        assert_eq!(super::tcp_client_owner(1, port, &[]), None);
    }
}
