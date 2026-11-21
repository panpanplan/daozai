//! 罗技 HID++ 外设电量：直接查询接收器，不读取 Options+ 的缓存或数据库。

use std::time::{Duration, Instant};

/// 状态检测周期：只读枚举接收器接口，插拔变化约 2 秒后触发读取。
const PROBE_INTERVAL: Duration = Duration::from_secs(2);
/// 电量与充电状态的完整读取周期。
const BATTERY_INTERVAL: Duration = Duration::from_secs(60);
/// 状态需要连续稳定的轮数，避免枚举抖动造成反复读取。
const SETTLE_ROUNDS: u8 = 2;
/// 两次完整读取之间的最短间隔。
const SCAN_COOLDOWN: Duration = Duration::from_secs(5);
/// 完整扫描时探测空槽位的等待上限，避免 6 个空槽位各等满超时。
const PROBE_TIMEOUT: Duration = Duration::from_millis(1000);
/// 完整读取时单次请求的等待上限，兼容响应稍慢的接收器。
const QUERY_TIMEOUT: Duration = Duration::from_millis(2000);
/// 连续读不到设备的轮数上限：单次失败沿用上次读数，避免偶发超时显示「电量未知」。
const MISS_LIMIT: u8 = 2;

/// 分层调度：只读状态稳定变化或按需请求时重读，否则按电量周期兜底。
#[derive(Default)]
struct Schedule {
    fingerprint: Vec<String>,
    pending: Option<(Vec<String>, u8)>,
    battery_at: Option<Instant>,
    scanned_at: Option<Instant>,
}

impl Schedule {
    fn cooled(&self, now: Instant) -> bool {
        self.scanned_at
            .is_none_or(|at| now.duration_since(at) >= SCAN_COOLDOWN)
    }

    /// 电量周期无条件兜底；状态变化需连续稳定，按需刷新也要过冷却。
    fn due(&mut self, fingerprint: &[String], now: Instant, force: bool) -> bool {
        if self.battery_at.is_none_or(|at| now.duration_since(at) >= BATTERY_INTERVAL) {
            self.pending = None;
            return true;
        }
        if !self.cooled(now) {
            return false;
        }
        if force {
            self.pending = None;
            return true;
        }
        if fingerprint == self.fingerprint.as_slice() {
            self.pending = None;
            return false;
        }
        match &mut self.pending {
            Some((seen, rounds)) if seen.as_slice() == fingerprint => *rounds += 1,
            _ => self.pending = Some((fingerprint.to_vec(), 1)),
        }
        self.pending
            .as_ref()
            .is_some_and(|(_, rounds)| *rounds >= SETTLE_ROUNDS)
    }

    fn commit(&mut self, fingerprint: Vec<String>, now: Instant) {
        self.fingerprint = fingerprint;
        self.pending = None;
        self.battery_at = Some(now);
        self.scanned_at = Some(now);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub battery: Option<u8>,
    pub charging: bool,
    pub kind: crate::bt::DeviceKind,
}

impl Device {
    pub fn summary(&self) -> String {
        match self.battery {
            Some(level) => format!(
                "{} · 电量 {level}%{}",
                self.name,
                if self.charging { " · 充电中" } else { "" }
            ),
            None => format!("{} · 电量未知", self.name),
        }
    }
}

#[derive(Default)]
struct Cache {
    devices: Vec<Device>,
    /// 每个设备连续读取失败的次数，用于区分偶发超时与真正休眠。
    misses: Vec<(String, u8)>,
}

impl Cache {
    fn update(
        &mut self,
        found: Vec<Device>,
        receivers: &[String],
        failed: &[String],
    ) -> Vec<Device> {
        self.devices
            .retain(|old| receivers.iter().any(|path| old.id.starts_with(path)));
        self.misses
            .retain(|(id, _)| self.devices.iter().any(|old| &old.id == id));
        let mut devices = found;
        for device in &mut devices {
            if device.name.starts_with("罗技设备 ") {
                if let Some(old) = self.devices.iter().find(|old| old.id == device.id) {
                    device.name = old.name.clone();
                    device.kind = old.kind;
                }
            }
        }
        for device in &devices {
            self.misses.retain(|(id, _)| id != &device.id);
        }
        for id in failed {
            let Some(old) = self.devices.iter().find(|old| &old.id == id) else {
                continue;
            };
            let misses = match self.misses.iter_mut().find(|(missed, _)| missed == id) {
                Some((_, count)) => {
                    *count = count.saturating_add(1);
                    *count
                }
                None => {
                    self.misses.push((id.clone(), 1));
                    1
                }
            };
            devices.push(if misses >= MISS_LIMIT {
                // 连续多轮都读不到才认为设备确实休眠，短暂超时沿用上次读数。
                Device {
                    battery: None,
                    charging: false,
                    ..old.clone()
                }
            } else {
                old.clone()
            });
        }
        devices.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
        devices.dedup_by(|a, b| {
            if a.name.eq_ignore_ascii_case(&b.name) && a.kind == b.kind {
                // dedup_by 保留 b、移除 a；把更完整的数据写回保留项。
                if b.battery.is_none() && a.battery.is_some() {
                    *b = a.clone();
                }
                true
            } else {
                false
            }
        });
        self.devices = devices.clone();
        devices
    }
}

enum Command {
    Stop,
    Refresh,
}

pub struct Service {
    pub rx: std::sync::mpsc::Receiver<Vec<Device>>,
    cmd: std::sync::mpsc::Sender<Command>,
}

impl Drop for Service {
    fn drop(&mut self) {
        let _ = self.cmd.send(Command::Stop);
    }
}

impl Service {
    /// 用户打开外设页时请求一次立即读取；仍受最小扫描间隔限制。
    pub fn refresh(&self) {
        let _ = self.cmd.send(Command::Refresh);
    }
}

pub fn spawn() -> Service {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let (cmd, commands) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("daozai-logitech".into())
        .spawn(move || {
            #[cfg(windows)]
            let mut scan = Scan::default();
            let mut force = true;
            loop {
                match commands.try_recv() {
                    Ok(Command::Stop) | Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
                    Ok(Command::Refresh) => force = true,
                    Err(std::sync::mpsc::TryRecvError::Empty) => {}
                }
                let started = Instant::now();
                #[cfg(windows)]
                if let Some(devices) = scan.poll(started, force) {
                    if matches!(
                        tx.try_send(devices),
                        Err(std::sync::mpsc::TrySendError::Disconnected(_))
                    ) {
                        return;
                    }
                }
                #[cfg(not(windows))]
                let _ = &tx;
                force = false;
                let wait = PROBE_INTERVAL
                    .checked_sub(started.elapsed())
                    .unwrap_or(Duration::ZERO);
                match commands.recv_timeout(wait) {
                    Ok(Command::Stop) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        return;
                    }
                    Ok(Command::Refresh) => force = true,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                }
            }
        })
        .ok();
    Service { rx, cmd }
}

fn device_kind(name: &str) -> crate::bt::DeviceKind {
    let name = name.to_ascii_lowercase();
    if ["keyboard", "mechanical", "keys", "键盘"]
        .iter()
        .any(|part| name.contains(part))
    {
        crate::bt::DeviceKind::Keyboard
    } else if ["mouse", "anywhere", "master", "lift", "鼠标"]
        .iter()
        .any(|part| name.contains(part))
    {
        crate::bt::DeviceKind::Mouse
    } else {
        crate::bt::DeviceKind::Other
    }
}

fn battery_status(data: &[u8]) -> Option<(u8, bool)> {
    let level = *data.first()?;
    if level > 100 {
        return None;
    }
    // UNIFIED_BATTERY (0x1004) GetStatus 回复 [电量百分比, 档位, 充电状态]：
    // 充电状态 0=放电，1/2/3=充电中，4=已充满（仍接电）；5..=7 是电池错误，按电量未知处理。
    match data.get(2) {
        Some(1..=4) => Some((level, true)),
        Some(5..=7) => None,
        _ => Some((level, false)),
    }
}

#[cfg(windows)]
struct Connection {
    device: hidapi::HidDevice,
    path: String,
    software_id: u8,
    timeout: Duration,
}

#[cfg(windows)]
impl Connection {
    fn request(
        &mut self,
        slot: u8,
        feature: u8,
        function: u8,
        params: &[u8],
    ) -> Result<Vec<u8>, ()> {
        if params.len() > 16 || function > 15 {
            return Err(());
        }
        self.software_id = self.software_id % 15 + 1;
        let software_id = self.software_id;
        let mut report = [0u8; 20];
        report[0] = 0x11; // HID++ 长报告
        report[1] = slot;
        report[2] = feature;
        report[3] = function << 4 | software_id;
        report[4..4 + params.len()].copy_from_slice(params);
        if self.device.write(&report).map_err(|_| ())? != report.len() {
            return Err(());
        }

        let start = Instant::now();
        loop {
            let left = self.timeout.checked_sub(start.elapsed()).ok_or(())?;
            let mut response = [0u8; 64];
            let count = self
                .device
                .read_timeout(&mut response, left.as_millis().max(1) as i32)
                .map_err(|_| ())?;
            if count == 0 {
                return Err(());
            }
            if count < 5
                || response[0] != 0x11
                || response[1] != slot
                || response[3] & 0x0f != software_id
            {
                continue;
            }
            // HID++ 的错误回复使用固定功能号 0x8F。
            if response[2] == 0x8f {
                return Err(());
            }
            if response[2] == feature && response[3] >> 4 == function {
                return Ok(response[4..count].to_vec());
            }
        }
    }

    fn feature_index(&mut self, slot: u8, feature: u16) -> Result<u8, ()> {
        let response = self.request(slot, 0, 0, &feature.to_be_bytes())?;
        let index = *response.first().ok_or(())?;
        if index == 0 {
            Err(())
        } else {
            Ok(index)
        }
    }

    fn name(&mut self, slot: u8) -> Option<String> {
        let feature = self.feature_index(slot, 0x0005).ok()?;
        let length = *self.request(slot, feature, 0, &[]).ok()?.first()? as usize;
        if length == 0 || length > 64 {
            return None;
        }
        let mut bytes = Vec::with_capacity(length);
        while bytes.len() < length {
            let offset = bytes.len() as u8;
            let response = self.request(slot, feature, 1, &[offset]).ok()?;
            // GetDeviceName 的回复直接是本段 UTF-8 名称，不回显请求偏移。
            let chunk = response.as_slice();
            if chunk.is_empty() {
                return None;
            }
            bytes.extend_from_slice(chunk);
        }
        bytes.truncate(length);
        let name = String::from_utf8_lossy(&bytes)
            .trim_matches(char::from(0))
            .trim()
            .to_string();
        (!name.is_empty()).then_some(name)
    }
}

#[cfg(windows)]
#[derive(Default)]
struct Scan {
    api: Option<hidapi::HidApi>,
    cache: Cache,
    schedule: Schedule,
}

#[cfg(windows)]
impl Scan {
    /// 每轮只读枚举接收器接口，不发 HID++ 请求；状态稳定变化、按需请求或电量周期到点才完整读取。
    fn poll(&mut self, now: Instant, force: bool) -> Option<Vec<Device>> {
        if self.api.is_none() {
            self.api = hidapi::HidApi::new().ok();
        }
        let api = self.api.as_mut()?;
        api.refresh_devices().ok()?;
        let api: &hidapi::HidApi = api;

        // 设备休眠/唤醒不会改变 HID 接口；主动 ping 会干扰未完成的 HID++ 响应，
        // 让同一设备在读数和「电量未知」之间来回跳，所以状态只看接口集合。
        let mut fingerprint: Vec<String> = api
            .device_list()
            .filter(|info| {
                info.vendor_id() == 0x046d && info.usage_page() == 0xff00 && info.usage() == 0x0002
            })
            .map(|info| info.path().to_string_lossy().into_owned())
            .collect();
        fingerprint.sort();
        fingerprint.dedup();
        if !self.schedule.due(&fingerprint, now, force) {
            return None;
        }
        // 已知设备（含休眠的）给足超时，空槽位仍用短超时快速跳过。
        let known: Vec<(String, u8)> = self
            .cache
            .devices
            .iter()
            .filter_map(|device| {
                let (path, slot) = device.id.rsplit_once('#')?;
                Some((path.to_string(), slot.parse::<u8>().ok()?))
            })
            .collect();
        let (found, receivers, failed) = query(api, &known);
        let devices = self.cache.update(found, &receivers, &failed);
        self.schedule.commit(fingerprint, now);
        Some(devices)
    }
}

#[cfg(windows)]
fn query(api: &hidapi::HidApi, known: &[(String, u8)]) -> (Vec<Device>, Vec<String>, Vec<String>) {
    let mut found = Vec::new();
    let mut receivers = Vec::new();
    let mut failed = Vec::new();
    for info in api.device_list().filter(|info| {
        info.vendor_id() == 0x046d && info.usage_page() == 0xff00 && info.usage() == 0x0002
    }) {
        let path = info.path().to_string_lossy().into_owned();
        receivers.push(path.clone());
        let Ok(device) = info.open_device(api) else {
            failed.extend((1..=6).map(|slot| format!("{path}#{slot}")));
            continue;
        };
        let mut connection = Connection {
            device,
            path,
            software_id: 0,
            timeout: QUERY_TIMEOUT,
        };
        for slot in 1..=6 {
            let id = format!("{}#{slot}", connection.path);
            // 已知设备给足超时，空槽位用短超时快速跳过。
            let familiar = known
                .iter()
                .any(|(path, known_slot)| path == &connection.path && *known_slot == slot);
            connection.timeout = if familiar { QUERY_TIMEOUT } else { PROBE_TIMEOUT };
            let Ok(feature) = connection.feature_index(slot, 0x1004) else {
                failed.push(id);
                continue;
            };
            connection.timeout = QUERY_TIMEOUT;
            // GetStatus 是 0x1004 的功能 1（真实硬件验证：返回 [电量百分比, 档位, 充电状态]）。
            let Some((level, charging)) = connection
                .request(slot, feature, 1, &[])
                .ok()
                .and_then(|data| battery_status(&data))
            else {
                failed.push(id);
                continue;
            };
            let name = connection
                .name(slot)
                .unwrap_or_else(|| format!("罗技设备 {slot}"));
            found.push(Device {
                id,
                name: name.clone(),
                battery: Some(level),
                charging,
                kind: device_kind(&name),
            });
        }
    }
    (found, receivers, failed)
}

impl crate::app::App {
    pub(crate) fn page_logitech(&mut self, c: &crate::paint::Card, env: &crate::app::Env) {
        use crate::paint::H;
        use crate::theme::{self, size, W};
        use eframe::egui::Color32;

        if !env.peripherals_ready {
            c.centered_text(
                44.0,
                80.0,
                "正在读取外设电量…",
                size::BODY,
                W::Regular,
                theme::GRAY,
                H::Left,
            );
            return;
        }
        if env.peripherals.is_empty() {
            c.centered_text(
                44.0,
                68.0,
                "未发现罗技外设",
                14.0,
                W::Semibold,
                theme::WHITE,
                H::Left,
            );
            c.centered_text(
                44.0,
                92.0,
                "请确认鼠标或键盘已通过接收器连接",
                size::BODY,
                W::Regular,
                theme::GRAY,
                H::Left,
            );
            return;
        }
        let single = env.peripherals.len() == 1;
        for (index, device) in env.peripherals.iter().take(2).enumerate() {
            let x = if single {
                129.5
            } else {
                44.0 + index as f32 * 171.0
            };
            c.fill(x, 43.0, 160.0, 72.0, 12.0, theme::CARD);
            crate::extras::device_icon(c, device.kind, x + 12.0, 55.0, 22.0);
            c.text(
                x + 44.0,
                68.0,
                &c.fit(&device.name, 12.0, W::Semibold, 102.0),
                12.0,
                W::Semibold,
                theme::WHITE,
                H::Left,
            );
            if let Some(level) = device.battery {
                crate::paint::battery(c, x + 23.0, 96.0, Some(level), device.charging, 1.0);
            } else {
                c.ring(x + 23.0, 96.0, 8.2, 2.6, Color32::from_gray(80));
                c.text(x + 23.0, 99.0, "?", 8.0, W::Bold, theme::GRAY, H::Center);
            }
            let status = match (device.battery, device.charging) {
                (Some(level), true) => format!("{level}% · 充电中"),
                (Some(level), false) => format!("{level}%"),
                (None, _) => "电量未知".into(),
            };
            let color = if device.battery.is_some_and(|level| level <= 20) && !device.charging {
                theme::RED_TOP
            } else if device.charging {
                theme::CHARGE
            } else {
                theme::GRAY
            };
            c.text(x + 44.0, 100.0, &status, 12.0, W::Semibold, color, H::Left);
        }
        c.centered_text(
            44.0,
            126.0,
            "状态 2 秒 · 打开页面即刷新",
            9.5,
            W::Regular,
            theme::GRAY,
            H::Left,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_settles_before_rescan_and_battery_stays_minute_based() {
        let start = Instant::now();
        let mut schedule = Schedule::default();
        let idle = vec!["receiver".to_string()];
        assert!(schedule.due(&idle, start, true), "首轮应完整读取");
        schedule.commit(idle.clone(), start);
        for second in (2..60).step_by(2) {
            assert!(
                !schedule.due(&idle, start + Duration::from_secs(second), false),
                "状态未变化时不应重复读取电量"
            );
        }
        assert!(
            schedule.due(&idle, start + Duration::from_secs(60), false),
            "每分钟兜底刷新电量"
        );
        schedule.commit(idle.clone(), start + Duration::from_secs(60));

        // 60 秒兜底也会进入冷却，稳定性判断从冷却结束后开始。
        let changed = vec!["receiver".to_string(), "receiver-b".to_string()];
        assert!(
            !schedule.due(&changed, start + Duration::from_secs(66), false),
            "单轮状态抖动不应触发读取"
        );
        assert!(
            schedule.due(&changed, start + Duration::from_secs(68), false),
            "状态连续两轮一致才认定变化"
        );
        schedule.commit(changed, start + Duration::from_secs(68));
        assert!(
            !schedule.due(&idle, start + Duration::from_secs(70), false),
            "冷却期内不重复扫描"
        );
        assert!(
            !schedule.due(&idle, start + Duration::from_secs(70), true),
            "冷却期内按需刷新也要限频"
        );
        assert!(
            schedule.due(&idle, start + Duration::from_secs(74), true),
            "冷却结束后按需刷新生效"
        );
    }

    fn device(id: &str) -> Device {
        Device {
            id: id.into(),
            name: "MX Anywhere 3S".into(),
            battery: Some(80),
            charging: false,
            kind: crate::bt::DeviceKind::Mouse,
        }
    }

    #[test]
    fn battery_status_preserves_unknown_and_charging_state() {
        // 真实硬件 GetStatus 回复布局：[电量百分比, 档位, 充电状态]。
        assert_eq!(battery_status(&[100, 0x30, 0]), Some((100, false)));
        assert_eq!(battery_status(&[85, 0x30, 1]), Some((85, true)));
        assert_eq!(battery_status(&[85, 0x30, 3]), Some((85, true)));
        assert_eq!(battery_status(&[100, 0x30, 4]), Some((100, true)));
        assert_eq!(battery_status(&[15, 0x30, 5]), None);
        assert_eq!(battery_status(&[101, 0x30, 0]), None);
        assert_eq!(battery_status(&[]), None);
    }

    #[test]
    fn single_failed_scan_keeps_last_reading() {
        let mut cache = Cache::default();
        let receiver = "receiver".to_string();
        cache.update(vec![device("receiver#2")], std::slice::from_ref(&receiver), &[]);
        let devices = cache.update(
            vec![],
            std::slice::from_ref(&receiver),
            &["receiver#2".into()],
        );
        assert_eq!(devices[0].battery, Some(80));
        assert_eq!(devices[0].name, "MX Anywhere 3S");
    }

    #[test]
    fn successful_read_clears_failure_count() {
        let mut cache = Cache::default();
        let receiver = "receiver".to_string();
        cache.update(vec![device("receiver#2")], std::slice::from_ref(&receiver), &[]);
        cache.update(vec![], std::slice::from_ref(&receiver), &["receiver#2".into()]);
        cache.update(vec![device("receiver#2")], std::slice::from_ref(&receiver), &[]);
        let devices = cache.update(
            vec![],
            std::slice::from_ref(&receiver),
            &["receiver#2".into()],
        );
        assert_eq!(devices[0].battery, Some(80));
    }

    #[test]
    fn repeated_failure_reports_unknown_but_keeps_identity() {
        let mut cache = Cache::default();
        let receiver = "receiver".to_string();
        cache.update(vec![device("receiver#2")], std::slice::from_ref(&receiver), &[]);
        cache.update(vec![], std::slice::from_ref(&receiver), &["receiver#2".into()]);
        let devices = cache.update(
            vec![],
            std::slice::from_ref(&receiver),
            &["receiver#2".into()],
        );
        assert_eq!(devices[0].battery, None);
        assert_eq!(devices[0].name, "MX Anywhere 3S");
        assert!(cache.devices[0].battery.is_none());
    }

    #[test]
    fn temporary_name_failure_keeps_the_previous_identity() {
        let mut cache = Cache::default();
        let receiver = "receiver".to_string();
        cache.update(
            vec![device("receiver#2")],
            std::slice::from_ref(&receiver),
            &[],
        );
        let fallback = Device {
            id: "receiver#2".into(),
            name: "罗技设备 2".into(),
            kind: crate::bt::DeviceKind::Other,
            ..device("unused#2")
        };
        let devices = cache.update(vec![fallback], std::slice::from_ref(&receiver), &[]);
        assert_eq!(devices[0].name, "MX Anywhere 3S");
        assert_eq!(devices[0].kind, crate::bt::DeviceKind::Mouse);
        assert_eq!(devices[0].battery, Some(80));
    }

    #[test]
    fn unplugged_receiver_removes_its_devices() {
        let mut cache = Cache::default();
        cache.update(vec![device("receiver#2")], &["receiver".into()], &[]);
        assert!(cache.update(vec![], &[], &[]).is_empty());
    }

    #[test]
    fn duplicate_transport_prefers_known_battery() {
        let mut cache = Cache::default();
        let unknown = Device {
            id: "bt-like#2".into(),
            battery: None,
            ..device("other#2")
        };
        let devices = cache.update(
            vec![unknown, device("receiver#2")],
            &["bt-like".into(), "receiver".into()],
            &[],
        );
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].battery, Some(80));
    }
}
