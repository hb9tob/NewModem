//! CPU time and host description for the modem journal.
//!
//! The journal compares the wall time of an activity with the CPU time of
//! the thread that ran it: wall >> CPU means the thread was starved (the
//! signature of an AVIF encode competing with the RX worker), wall ~= CPU
//! means the activity itself is expensive.
//!
//! Every reader returns `None` where the platform has no cheap answer, so
//! the journal records "unknown" rather than a misleading zero.

use std::time::Duration;

#[cfg(unix)]
fn clock(id: libc::clockid_t) -> Option<Duration> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, writable timespec for the duration of the call.
    let rc = unsafe { libc::clock_gettime(id, &mut ts) };
    (rc == 0).then(|| Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32))
}

/// CPU time (user + system) consumed by the calling thread.
#[cfg(unix)]
pub fn thread_cpu() -> Option<Duration> {
    clock(libc::CLOCK_THREAD_CPUTIME_ID)
}

/// CPU time (user + system) consumed by the whole process, all threads.
#[cfg(unix)]
pub fn process_cpu() -> Option<Duration> {
    clock(libc::CLOCK_PROCESS_CPUTIME_ID)
}

#[cfg(windows)]
fn filetimes_to_duration(
    kernel: &windows_sys::Win32::Foundation::FILETIME,
    user: &windows_sys::Win32::Foundation::FILETIME,
) -> Duration {
    let ticks = |t: &windows_sys::Win32::Foundation::FILETIME| {
        (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime)
    };
    // FILETIME counts 100 ns intervals.
    Duration::from_nanos((ticks(kernel) + ticks(user)) * 100)
}

/// CPU time (user + system) consumed by the calling thread. The Windows
/// counter advances in ~15.6 ms steps: meaningful over a 1 s probe window,
/// not over a single short activity.
#[cfg(windows)]
pub fn thread_cpu() -> Option<Duration> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: pseudo-handle of the current thread, four valid out-pointers.
    let ok = unsafe {
        GetThreadTimes(
            GetCurrentThread(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    };
    (ok != 0).then(|| filetimes_to_duration(&kernel, &user))
}

/// CPU time (user + system) consumed by the whole process, all threads.
#[cfg(windows)]
pub fn process_cpu() -> Option<Duration> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: pseudo-handle of the current process, four valid out-pointers.
    let ok = unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    };
    (ok != 0).then(|| filetimes_to_duration(&kernel, &user))
}

/// Resident set size of the process, MiB (Linux only).
pub fn rss_mib() -> Option<u64> {
    proc_kib("/proc/self/status", "VmRSS:").map(|kib| kib / 1024)
}

/// Static description of the host, written once in the journal header.
///
/// Deliberately free of anything identifying: no host name, no user name,
/// no serial number. Journals are published.
#[derive(Debug, Clone, serde::Serialize)]
pub struct HostInfo {
    pub os: &'static str,
    pub arch: &'static str,
    pub cpu: Option<String>,
    pub cores: Option<usize>,
    pub ram_mib: Option<u64>,
}

impl HostInfo {
    pub fn detect() -> Self {
        Self {
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            cpu: cpu_model(),
            cores: std::thread::available_parallelism().ok().map(|n| n.get()),
            ram_mib: total_ram_mib(),
        }
    }
}

fn cpu_model() -> Option<String> {
    if let Ok(info) = std::fs::read_to_string("/proc/cpuinfo") {
        if let Some(model) = field_value(&info, "model name") {
            return Some(model);
        }
    }
    // ARM boards have no "model name": use the board model (Pi 4, Pi 5...).
    if let Ok(board) = std::fs::read_to_string("/proc/device-tree/model") {
        let board = board.trim_end_matches('\0').trim();
        if !board.is_empty() {
            return Some(board.to_string());
        }
    }
    std::env::var("PROCESSOR_IDENTIFIER").ok()
}

/// Value of the first `key : value` line of a procfs-style text.
fn field_value(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let (k, v) = line.split_once(':')?;
        (k.trim() == key).then(|| v.trim().to_string())
    })
}

#[cfg(windows)]
fn total_ram_mib() -> Option<u64> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: MEMORYSTATUSEX is plain data, valid when zeroed; `dwLength`
    // is set as the API requires before the call.
    unsafe {
        let mut st: MEMORYSTATUSEX = std::mem::zeroed();
        st.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        (GlobalMemoryStatusEx(&mut st) != 0).then(|| st.ullTotalPhys / (1024 * 1024))
    }
}

#[cfg(not(windows))]
fn total_ram_mib() -> Option<u64> {
    proc_kib("/proc/meminfo", "MemTotal:").map(|kib| kib / 1024)
}

fn proc_kib(path: &str, key: &str) -> Option<u64> {
    let text = std::fs::read_to_string(path).ok()?;
    text.lines()
        .find_map(|line| line.strip_prefix(key))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn thread_cpu_ignores_sleep_and_counts_work() {
        let before = thread_cpu().expect("thread CPU clock");
        std::thread::sleep(Duration::from_millis(100));
        let slept = thread_cpu().expect("thread CPU clock") - before;
        assert!(slept < Duration::from_millis(40), "sleep billed {slept:?}");

        // Spin on the CPU clock itself so a preempted test cannot flake.
        let before = thread_cpu().expect("thread CPU clock");
        let deadline = Instant::now() + Duration::from_secs(10);
        while thread_cpu().expect("thread CPU clock") - before < Duration::from_millis(40) {
            assert!(Instant::now() < deadline, "thread CPU clock does not advance");
        }
    }

    #[test]
    fn process_cpu_is_monotonic() {
        let a = process_cpu().expect("process CPU clock");
        let b = process_cpu().expect("process CPU clock");
        assert!(b >= a);
    }

    #[test]
    fn field_value_matches_exact_key() {
        let text = "model\t\t: 126\nmodel name\t: Intel(R) Core(TM) i5-1035G4\n";
        assert_eq!(
            field_value(text, "model name").as_deref(),
            Some("Intel(R) Core(TM) i5-1035G4")
        );
        assert_eq!(field_value(text, "flags"), None);
    }
}
