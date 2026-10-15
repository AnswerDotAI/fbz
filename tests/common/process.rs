use std::{io, mem::MaybeUninit, os::unix::process::ExitStatusExt, process::{Command, ExitStatus}, time::{Duration, Instant}};

struct FootprintSampler { stop: std::sync::Arc<std::sync::atomic::AtomicBool>, worker: std::thread::JoinHandle<Option<u64>> }

#[cfg(target_os = "macos")]
impl FootprintSampler {
    fn start(pid: libc::pid_t) -> Self {
        use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let initial = physical_footprint(pid);
        let worker = std::thread::spawn(move || {
            let mut maximum = initial;
            while !worker_stop.load(Ordering::Relaxed) {
                if let Some(value) = physical_footprint(pid) { maximum = Some(maximum.map_or(value, |previous: u64| previous.max(value))); }
                std::thread::sleep(Duration::from_millis(50));
            }
            maximum
        });
        Self { stop, worker }
    }
}

#[cfg(not(target_os = "macos"))]
impl FootprintSampler {
    fn start(_pid: libc::pid_t) -> Self {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker = std::thread::spawn(|| None);
        Self { stop, worker }
    }
}

#[cfg(target_os = "macos")]
fn physical_footprint(pid: libc::pid_t) -> Option<u64> {
    #[repr(C)]
    struct RusageInfoV4 { uuid: [u8; 16], values: [u64; 35] }
    unsafe extern "C" { fn proc_pid_rusage(pid: libc::c_int, flavor: libc::c_int, buffer: *mut libc::c_void) -> libc::c_int; }
    let mut usage = RusageInfoV4 { uuid: [0; 16], values: [0; 35] };
    // SAFETY: flavor 4 requests the exact repr(C) buffer above, which remains
    // writable for the call; `pid` is the benchmark process's own child.
    let result = unsafe { proc_pid_rusage(pid, 4, (&mut usage as *mut RusageInfoV4).cast()) };
    // `values` starts immediately after `ri_uuid`; in rusage_info_v4,
    // ri_phys_footprint is the eighth u64 field.
    (result == 0).then_some(usage.values[7])
}

#[derive(Clone, Copy, Debug)]
pub struct Timing { pub status: ExitStatus, pub wall: Duration }

#[derive(Clone, Copy, Debug)]
pub struct ProcessMetrics {
    pub status: ExitStatus, pub wall: Duration, pub user: Duration, pub system: Duration, pub peak_rss_bytes: u64, pub peak_phys_footprint_bytes: Option<u64>
}

pub fn measure_timing(command: &mut Command) -> io::Result<Timing> {
    let started = Instant::now();
    let status = command.status()?;
    Ok(Timing { status, wall: started.elapsed() })
}

pub fn measure(command: &mut Command) -> io::Result<ProcessMetrics> {
    let started = Instant::now();
    let child = command.spawn()?;
    let pid = child.id() as libc::pid_t;
    let sampler = FootprintSampler::start(pid);
    let mut status = 0;
    let mut usage = MaybeUninit::<libc::rusage>::uninit();
    loop {
        // SAFETY: `pid` names our live child, and both output pointers refer to
        // valid writable storage for the duration of the call.
        let result = unsafe { libc::wait4(pid, &mut status, 0, usage.as_mut_ptr()) };
        if result >= 0 { break; }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted { return Err(error); }
    }
    // SAFETY: a successful `wait4` initialized the complete `rusage` value.
    let usage = unsafe { usage.assume_init() };
    let wall = started.elapsed();
    let duration = |value: libc::timeval| Duration::new(value.tv_sec as u64, (value.tv_usec as u32) * 1_000);
    #[cfg(target_os = "macos")]
    let peak_rss_bytes = usage.ru_maxrss as u64;
    #[cfg(not(target_os = "macos"))]
    let peak_rss_bytes = (usage.ru_maxrss as u64).saturating_mul(1024);
    sampler.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let peak_phys_footprint_bytes = sampler.worker.join().unwrap_or(None);
    Ok(ProcessMetrics { status: ExitStatus::from_raw(status), wall, user: duration(usage.ru_utime), system: duration(usage.ru_stime), peak_rss_bytes,
        peak_phys_footprint_bytes })
}

#[test]
fn collects_child_metrics_without_platform_time_tools() {
    let metrics = measure(&mut Command::new("/usr/bin/true")).unwrap();
    assert!(metrics.status.success());
    assert!(metrics.wall > Duration::ZERO);
    assert!(metrics.peak_rss_bytes > 0);
    #[cfg(target_os = "macos")]
    assert!(metrics.peak_phys_footprint_bytes.is_some());
}
