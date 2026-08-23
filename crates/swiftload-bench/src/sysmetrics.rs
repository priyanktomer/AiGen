//! Process CPU and memory sampling.
//!
//! Throughput alone is a misleading benchmark for a download manager: a client that saturates
//! the link while burning a core and 400 MB of RSS is not obviously better than one that is a
//! little slower. These numbers are reported alongside every result.

/// Total CPU time (user + system) used by this process so far, in milliseconds.
#[cfg(target_os = "linux")]
pub fn cpu_millis() -> u64 {
    let Ok(stat) = std::fs::read_to_string("/proc/self/stat") else {
        return 0;
    };
    // The comm field can itself contain spaces and parentheses, so parse from the *last*
    // closing paren rather than splitting the whole line on whitespace.
    let Some((_, rest)) = stat.rsplit_once(')') else {
        return 0;
    };
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // After the comm field, index 0 is state; utime and stime are indices 11 and 12.
    if fields.len() <= 12 {
        return 0;
    }
    let ticks: u64 = fields[11].parse().unwrap_or(0) + fields[12].parse().unwrap_or(0);
    const USER_HZ: u64 = 100; // 100 on effectively every Linux build
    ticks * 1000 / USER_HZ
}

#[cfg(windows)]
pub fn cpu_millis() -> u64 {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

    let (mut created, mut exited, mut kernel, mut user): (FILETIME, FILETIME, FILETIME, FILETIME) =
        unsafe { std::mem::zeroed() };
    let ok = unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    };
    if ok == 0 {
        return 0;
    }
    // FILETIME counts 100-nanosecond intervals.
    let to_ms = |f: FILETIME| (((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64) / 10_000;
    to_ms(kernel) + to_ms(user)
}

#[cfg(not(any(target_os = "linux", windows)))]
pub fn cpu_millis() -> u64 {
    0
}

/// Peak resident set size in bytes — the high-water mark, not the current value.
#[cfg(target_os = "linux")]
pub fn peak_rss_bytes() -> u64 {
    let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
        return 0;
    };
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|v| v.split_whitespace().next())
        .and_then(|kb| kb.parse::<u64>().ok())
        .map_or(0, |kb| kb * 1024)
}

#[cfg(windows)]
pub fn peak_rss_bytes() -> u64 {
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    let mut pmc: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
    pmc.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    let ok = unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut pmc, pmc.cb) };
    if ok == 0 {
        return 0;
    }
    pmc.PeakWorkingSetSize as u64
}

#[cfg(not(any(target_os = "linux", windows)))]
pub fn peak_rss_bytes() -> u64 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_plausible_values_on_a_supported_platform() {
        // Zero is the documented "unsupported platform" answer, so only assert when we are on
        // one of the platforms that actually implements this.
        if cfg!(any(target_os = "linux", windows)) {
            assert!(peak_rss_bytes() > 1024 * 1024, "peak RSS looks implausible");
            // CPU time can legitimately still be 0 ms on a fast machine, so just check it
            // does not panic or return something absurd.
            assert!(cpu_millis() < 60 * 60 * 1000);
        }
    }

    #[test]
    fn cpu_time_is_monotonic() {
        let before = cpu_millis();
        let mut acc = 0u64;
        for i in 0..3_000_000u64 {
            acc = acc.wrapping_add(i * i);
        }
        std::hint::black_box(acc);
        assert!(cpu_millis() >= before, "cpu time went backwards");
    }
}
