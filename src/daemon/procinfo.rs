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

/// (total cpu time in ns, resident bytes)
pub fn task_info(pid: i32) -> Option<(u64, u64)> {
    let mut ti: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
    let n = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTASKINFO, 0, &mut ti as *mut _ as *mut libc::c_void, size) };
    if n != size {
        return None;
    }
    let (numer, denom) = timebase();
    let ticks = ti.pti_total_user + ti.pti_total_system;
    Some(((ticks as u128 * numer as u128 / denom as u128) as u64, ti.pti_resident_size))
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
                    let Some((cpu_ns, rss)) = task_info(pid) else { continue };
                    let st = start_time(pid).unwrap_or(0) as u64;
                    s.mem += rss as f64;
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
