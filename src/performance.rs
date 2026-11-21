//! 电脑性能监控：CPU、内存、硬盘与网络网速。
//!
//! 采样在独立线程里每秒执行一次，采集到的不可变快照经 `sync_channel(1)` 交给
//! 界面线程；界面只做 `try_recv`，任何系统 API 卡顿都不会拖慢岛仔。
//! Windows 下读取系统计数器，其他平台（录制器 / 开发机）不启动采集。

use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::time::Duration;

/// 采样间隔；CPU、磁盘和网络速率都是两次计数之差除以间隔。
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
/// 曲线保留的采样点数（约一分钟）。
pub const HISTORY_LEN: usize = 60;

/// 一个固定硬盘分区的容量。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Disk {
    /// 盘符，例如 `C:`。
    pub name: String,
    pub total: u64,
    pub free: u64,
}

impl Disk {
    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.free)
    }
    pub fn used_ratio(&self) -> f32 {
        if self.total == 0 { 0.0 } else { (self.used() as f64 / self.total as f64) as f32 }
    }
}

/// 一次采样的结果；界面只读，不持有任何系统句柄。
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    /// CPU 总占用率，0..100。
    pub cpu: f32,
    pub cpu_history: Vec<f32>,
    pub memory_used: u64,
    pub memory_total: u64,
    /// 物理内存占用率，0..1。
    pub memory_ratio: f32,
    pub memory_history: Vec<f32>,
    pub disks: Vec<Disk>,
    /// 物理磁盘读写速率（字节/秒）；系统不允许读取计数器时为 None。
    pub disk_read: Option<f64>,
    pub disk_write: Option<f64>,
    /// 所有启用网卡的合计下行 / 上行速率（字节/秒）。
    pub net_down: f64,
    pub net_up: f64,
    pub net_down_history: Vec<f64>,
    pub net_up_history: Vec<f64>,
    /// 逻辑处理器数。
    pub cores: usize,
}

impl Snapshot {
    /// 是否已经拿到过系统数据（首次采样前为 false）。
    pub fn ready(&self) -> bool {
        self.memory_total > 0
    }
}

/// 速率显示：B/s → KB/s → MB/s → GB/s。
pub fn format_rate(bytes_per_sec: f64) -> String {
    if !bytes_per_sec.is_finite() || bytes_per_sec < 0.0 { return "—".into(); }
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    if bytes_per_sec < KB { format!("{:.0} B/s", bytes_per_sec) }
    else if bytes_per_sec < MB { format!("{:.1} KB/s", bytes_per_sec / KB) }
    else if bytes_per_sec < GB { format!("{:.1} MB/s", bytes_per_sec / MB) }
    else { format!("{:.2} GB/s", bytes_per_sec / GB) }
}

/// 容量显示：GB / TB。
pub fn format_size(bytes: u64) -> String {
    let gb = bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    if gb >= 1024.0 { format!("{:.1} TB", gb / 1024.0) } else { format!("{:.1} GB", gb) }
}

/// 两次计数之差换算成每秒速率；计数器重置（新值小于旧值）时不显示突刺。
pub fn rate_per_sec(now: u64, prev: u64, dt: f64) -> f64 {
    if dt <= 0.0 || now < prev { 0.0 } else { (now - prev) as f64 / dt }
}

/// `GetSystemTimes` 的三个累计时间（idle / kernel / user，单位 100ns）转成总占用率。
/// kernel 时间包含 idle，所以总时间是 kernel + user，忙时间是 kernel - idle + user。
pub fn cpu_percent(prev: (u64, u64, u64), now: (u64, u64, u64)) -> f32 {
    let (pi, pk, pu) = prev;
    let (i, k, u) = now;
    let idle = i.saturating_sub(pi) as f64;
    let kernel = k.saturating_sub(pk) as f64;
    let user = u.saturating_sub(pu) as f64;
    let total = kernel + user;
    if total <= 0.0 { return 0.0; }
    let busy = (kernel - idle).max(0.0) + user;
    ((busy / total) * 100.0).clamp(0.0, 100.0) as f32
}

fn push<T: Clone>(history: &mut VecDeque<T>, value: T) {
    history.push_back(value);
    while history.len() > HISTORY_LEN { history.pop_front(); }
}

/// 后台采样服务：持有接收端与最近一次快照，`Drop` 时通知线程退出。
pub struct Service {
    rx: Receiver<Snapshot>,
    latest: Option<Snapshot>,
    #[cfg(windows)]
    stop: Option<mpsc::Sender<()>>,
}

impl Service {
    pub fn start() -> Self {
        let (tx, rx) = mpsc::sync_channel::<Snapshot>(1);
        #[cfg(windows)]
        let stop = {
            let (signal, wait) = mpsc::channel::<()>();
            let spawned = std::thread::Builder::new()
                .name("daozai-performance".into())
                .spawn(move || run(tx, wait));
            if spawned.is_ok() { Some(signal) } else { None }
        };
        #[cfg(not(windows))]
        drop(tx);
        Self { rx, latest: None, #[cfg(windows)] stop }
    }

    /// 取走本帧新到的采样；没有新数据时返回 None，界面沿用 `latest()`。
    pub fn poll(&mut self) -> Option<Snapshot> {
        let mut fresh = None;
        loop {
            match self.rx.try_recv() {
                Ok(snapshot) => fresh = Some(snapshot),
                Err(_) => break,
            }
        }
        if let Some(snapshot) = &fresh { self.latest = Some(snapshot.clone()); }
        fresh
    }

    pub fn latest(&self) -> Option<&Snapshot> { self.latest.as_ref() }
}

impl Drop for Service {
    fn drop(&mut self) {
        #[cfg(windows)]
        if let Some(stop) = &self.stop { let _ = stop.send(()); }
    }
}

#[cfg(windows)]
fn run(tx: SyncSender<Snapshot>, stop: Receiver<()>) {
    let mut sampler = win::Sampler::new();
    loop {
        match stop.recv_timeout(SAMPLE_INTERVAL) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let snapshot = sampler.sample();
                // 界面没来得及取时丢掉旧样本，绝不让采样线程被通道堵住。
                let _ = tx.try_send(snapshot);
            }
        }
    }
}

#[cfg(not(windows))]
fn run(_tx: SyncSender<Snapshot>, _stop: Receiver<()>) {}

// ---------------------------------------------------------------------------
// Windows 采集：全部走系统 API，不引入第三方监控库。
// ---------------------------------------------------------------------------
#[cfg(windows)]
mod win {
    use super::*;
    use std::time::Instant;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
    use windows::Win32::NetworkManagement::{
        IpHelper::{FreeMibTable, GetIfTable2, MIB_IF_ROW2, MIB_IF_TABLE2},
        Ndis::IfOperStatusUp,
    };
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDriveStringsW,
        FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_MODE, OPEN_EXISTING,
    };
    use windows::Win32::System::{
        Ioctl::{DISK_PERFORMANCE, IOCTL_DISK_PERFORMANCE},
        IO::DeviceIoControl,
        SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX},
        Threading::GetSystemTimes,
    };

    const IF_TYPE_SOFTWARE_LOOPBACK: u32 = 24;
    const DRIVE_FIXED: u32 = 3;

    fn filetime(t: FILETIME) -> u64 {
        ((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64
    }

    fn cpu_times() -> Option<(u64, u64, u64)> {
        let mut idle = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        unsafe {
            GetSystemTimes(
                Some(std::ptr::addr_of_mut!(idle)),
                Some(std::ptr::addr_of_mut!(kernel)),
                Some(std::ptr::addr_of_mut!(user)),
            )
        }
        .ok()?;
        Some((filetime(idle), filetime(kernel), filetime(user)))
    }

    fn memory() -> (u64, u64) {
        let mut status = MEMORYSTATUSEX {
            dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
            ..Default::default()
        };
        if unsafe { GlobalMemoryStatusEx(&mut status) }.is_err() { return (0, 0); }
        (status.ullTotalPhys.saturating_sub(status.ullAvailPhys), status.ullTotalPhys)
    }

    /// 所有处于 Up 状态的非回环网卡的累计收发字节。
    fn net_bytes() -> (u64, u64) {
        let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
        let code = unsafe { GetIfTable2(&mut table) };
        if code.0 != 0 || table.is_null() { return (0, 0); }
        let (mut down, mut up) = (0u64, 0u64);
        unsafe {
            let count = (*table).NumEntries as usize;
            let rows = std::slice::from_raw_parts(
                std::ptr::addr_of!((*table).Table).cast::<MIB_IF_ROW2>(),
                count,
            );
            for row in rows {
                if row.Type == IF_TYPE_SOFTWARE_LOOPBACK || row.OperStatus.0 != IfOperStatusUp.0 {
                    continue;
                }
                down = down.saturating_add(row.InOctets);
                up = up.saturating_add(row.OutOctets);
            }
            FreeMibTable(table.cast::<std::ffi::c_void>() as *const _);
        }
        (down, up)
    }

    fn disks() -> Vec<Disk> {
        let mut buffer = [0u16; 512];
        let len = unsafe { GetLogicalDriveStringsW(Some(buffer.as_mut_slice())) } as usize;
        if len == 0 || len > buffer.len() { return Vec::new(); }
        let mut result = Vec::new();
        let mut root = Vec::<u16>::new();
        for &ch in &buffer[..len] {
            if ch != 0 { root.push(ch); continue; }
            if root.is_empty() { continue; }
            let name = String::from_utf16_lossy(&root).trim_end_matches('\\').to_string();
            root.clear();
            let mut wide: Vec<u16> = name.encode_utf16().collect();
            wide.push('\\' as u16);
            wide.push(0);
            if unsafe { GetDriveTypeW(PCWSTR(wide.as_ptr())) } != DRIVE_FIXED { continue; }
            let (mut free, mut total) = (0u64, 0u64);
            let ok = unsafe {
                GetDiskFreeSpaceExW(
                    PCWSTR(wide.as_ptr()),
                    Some(std::ptr::addr_of_mut!(free)),
                    Some(std::ptr::addr_of_mut!(total)),
                    None,
                )
            }
            .is_ok();
            if ok && total > 0 { result.push(Disk { name, total, free }); }
        }
        result
    }

    /// `\\.\PhysicalDriveN` 句柄；磁盘性能计数器需要设备句柄，无权限时列表为空。
    struct DiskHandle(*mut std::ffi::c_void);
    impl Drop for DiskHandle {
        fn drop(&mut self) {
            unsafe { let _ = CloseHandle(HANDLE(self.0)); }
        }
    }

    fn open_disks() -> Vec<DiskHandle> {
        let mut handles = Vec::new();
        for index in 0..8 {
            let mut wide: Vec<u16> = format!("\\\\.\\PhysicalDrive{index}")
                .encode_utf16()
                .collect();
            wide.push(0);
            let handle = unsafe {
                CreateFileW(
                    PCWSTR(wide.as_ptr()),
                    0,
                    FILE_SHARE_MODE(3), // FILE_SHARE_READ | FILE_SHARE_WRITE
                    None,
                    OPEN_EXISTING,
                    FILE_FLAGS_AND_ATTRIBUTES(0),
                    HANDLE(std::ptr::null_mut()),
                )
            };
            match handle {
                Ok(h) if !h.0.is_null() => handles.push(DiskHandle(h.0)),
                _ => {}
            }
        }
        handles
    }

    fn disk_bytes(handles: &[DiskHandle]) -> Option<(i64, i64)> {
        if handles.is_empty() { return None; }
        let (mut read, mut write) = (0i64, 0i64);
        let mut ok_any = false;
        for handle in handles {
            let mut perf = DISK_PERFORMANCE::default();
            let mut returned = 0u32;
            let ok = unsafe {
                DeviceIoControl(
                    HANDLE(handle.0),
                    IOCTL_DISK_PERFORMANCE,
                    None,
                    0,
                    Some((&mut perf as *mut DISK_PERFORMANCE).cast()),
                    std::mem::size_of::<DISK_PERFORMANCE>() as u32,
                    Some(&mut returned),
                    None,
                )
            }
            .is_ok();
            if ok && returned as usize >= std::mem::size_of::<DISK_PERFORMANCE>() {
                read = read.saturating_add(perf.BytesRead);
                write = write.saturating_add(perf.BytesWritten);
                ok_any = true;
            }
        }
        ok_any.then_some((read, write))
    }

    pub(super) struct Sampler {
        last: Instant,
        cpu_prev: Option<(u64, u64, u64)>,
        net_prev: Option<(u64, u64)>,
        disk_prev: Option<(i64, i64)>,
        handles: Vec<DiskHandle>,
        cpu_history: VecDeque<f32>,
        memory_history: VecDeque<f32>,
        down_history: VecDeque<f64>,
        up_history: VecDeque<f64>,
        cores: usize,
    }

    impl Sampler {
        pub(super) fn new() -> Self {
            let handles = open_disks();
            let disk_prev = disk_bytes(&handles);
            Self {
                last: Instant::now(),
                cpu_prev: cpu_times(),
                net_prev: Some(net_bytes()),
                disk_prev,
                handles,
                cpu_history: VecDeque::new(),
                memory_history: VecDeque::new(),
                down_history: VecDeque::new(),
                up_history: VecDeque::new(),
                cores: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
            }
        }

        pub(super) fn sample(&mut self) -> Snapshot {
            let now = Instant::now();
            let dt = now.duration_since(self.last).as_secs_f64().max(0.05);
            self.last = now;

            let cpu_now = cpu_times();
            let cpu = match (self.cpu_prev, cpu_now) {
                (Some(prev), Some(now_times)) => cpu_percent(prev, now_times),
                _ => 0.0,
            };
            if cpu_now.is_some() { self.cpu_prev = cpu_now; }

            let (used, total) = memory();
            let memory_ratio = if total > 0 { (used as f64 / total as f64) as f32 } else { 0.0 };

            let (down_total, up_total) = net_bytes();
            let net_down = self.net_prev.map_or(0.0, |(d, _)| rate_per_sec(down_total, d, dt));
            let net_up = self.net_prev.map_or(0.0, |(_, u)| rate_per_sec(up_total, u, dt));
            self.net_prev = Some((down_total, up_total));

            let disk_now = disk_bytes(&self.handles);
            let (disk_read, disk_write) = match (disk_now, self.disk_prev) {
                (Some((r, w)), Some((pr, pw))) => (
                    Some(((r.saturating_sub(pr)) as f64 / dt).max(0.0)),
                    Some(((w.saturating_sub(pw)) as f64 / dt).max(0.0)),
                ),
                _ => (None, None),
            };
            if disk_now.is_some() { self.disk_prev = disk_now; }

            push(&mut self.cpu_history, cpu);
            push(&mut self.memory_history, memory_ratio);
            push(&mut self.down_history, net_down);
            push(&mut self.up_history, net_up);

            Snapshot {
                cpu,
                cpu_history: self.cpu_history.iter().copied().collect(),
                memory_used: used,
                memory_total: total,
                memory_ratio,
                memory_history: self.memory_history.iter().copied().collect(),
                disks: disks(),
                disk_read,
                disk_write,
                net_down,
                net_up,
                net_down_history: self.down_history.iter().copied().collect(),
                net_up_history: self.up_history.iter().copied().collect(),
                cores: self.cores,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_percent_uses_busy_time_over_total_time() {
        // idle +10，kernel +20（含 idle），user +10：忙 20 / 总 30。
        let value = cpu_percent((10, 30, 10), (20, 50, 20));
        assert!((value - 66.7).abs() < 0.2, "CPU 占用率应为 66.7%，实际 {value}");
        assert_eq!(cpu_percent((0, 0, 0), (0, 0, 0)), 0.0);
        assert_eq!(cpu_percent((10, 30, 10), (10, 30, 10)), 0.0);
        // 计数器异常回绕时不应出现负数或超过 100%。
        assert_eq!(cpu_percent((100, 100, 100), (0, 0, 0)), 0.0);
    }

    #[test]
    fn rates_switch_units_and_ignore_counter_resets() {
        assert_eq!(format_rate(0.0), "0 B/s");
        assert_eq!(format_rate(512.0), "512 B/s");
        assert_eq!(format_rate(2048.0), "2.0 KB/s");
        assert_eq!(format_rate(3.0 * 1024.0 * 1024.0), "3.0 MB/s");
        assert_eq!(format_rate(f64::NAN), "—");
        assert_eq!(rate_per_sec(2048, 1024, 2.0), 512.0);
        assert_eq!(rate_per_sec(10, 1000, 1.0), 0.0, "计数器重置不会产生负速率");
        assert_eq!(rate_per_sec(1000, 0, 0.0), 0.0);
    }

    #[test]
    fn disk_ratio_and_history_window() {
        let disk = Disk { name: "C:".into(), total: 1000, free: 250 };
        assert_eq!(disk.used(), 750);
        assert!((disk.used_ratio() - 0.75).abs() < f32::EPSILON);
        assert_eq!(Disk::default().used_ratio(), 0.0);
        let mut history = VecDeque::new();
        for value in 0..(HISTORY_LEN + 10) { push(&mut history, value); }
        assert_eq!(history.len(), HISTORY_LEN);
        assert_eq!(*history.front().unwrap(), 10);
        assert_eq!(*history.back().unwrap(), HISTORY_LEN + 9);
    }

    #[test]
    fn size_formats_gigabytes_and_terabytes() {
        assert_eq!(format_size(16 * 1024 * 1024 * 1024), "16.0 GB");
        assert_eq!(format_size(2 * 1024 * 1024 * 1024 * 1024), "2.0 TB");
    }

    /// 真正调用一次 Windows 计数器，确认系统 API 可用且数值在合理范围。
    #[cfg(windows)]
    #[test]
    fn windows_counters_return_plausible_values() {
        let mut sampler = super::win::Sampler::new();
        let first = sampler.sample();
        assert!(first.memory_total > 0, "应读到物理内存总量");
        assert!(first.memory_ratio >= 0.0 && first.memory_ratio <= 1.0);
        assert!(!first.disks.is_empty(), "应至少找到一个固定分区");
        assert!(first.cpu >= 0.0 && first.cpu <= 100.0);
        std::thread::sleep(Duration::from_millis(1200));
        let second = sampler.sample();
        assert!(second.cpu >= 0.0 && second.cpu <= 100.0);
        assert!(second.net_down >= 0.0 && second.net_up >= 0.0);
        assert_eq!(second.cpu_history.len(), 2);
        assert_eq!(second.net_down_history.len(), 2);
    }
}
