use serde::{Deserialize, Serialize};
use std::collections::HashMap;
#[cfg(any(windows, target_os = "macos"))]
use std::ffi::c_void;
#[cfg(target_os = "macos")]
use std::ffi::CStr;
#[cfg(windows)]
use std::mem::size_of;
#[cfg(windows)]
use std::ptr::null;
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::Duration;
#[cfg(target_os = "macos")]
use std::process::Command;

#[cfg(all(not(windows), not(target_os = "macos")))]
use battery::{Manager, State as BatteryState};
#[cfg(all(not(windows), not(target_os = "macos")))]
use display_info::DisplayInfo;
#[cfg(not(windows))]
use rusb::UsbContext;
#[cfg(all(not(windows), not(target_os = "macos")))]
use sysinfo::Networks;
#[cfg(all(target_os = "linux", feature = "linux-udev"))]
use libc::{poll, pollfd, POLLIN};
#[cfg(all(target_os = "linux", feature = "linux-udev"))]
use udev::MonitorBuilder;
#[cfg(all(target_os = "linux", feature = "linux-udev"))]
use std::os::unix::io::AsRawFd;
#[cfg(target_os = "macos")]
use objc2_core_foundation::{
    CFRunLoop, kCFRunLoopDefaultMode,
};
#[cfg(target_os = "macos")]
use objc2_io_kit::{
    io_iterator_t, IONotificationPort, IONotificationPortRef, IOIteratorNext, IOObjectRelease,
    IOServiceAddMatchingNotification, IOServiceMatching, kIOMatchedNotification,
    kIOTerminatedNotification,
};

#[cfg(windows)]
use wmi::WMIConnection;
#[cfg(windows)]
use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
#[cfg(windows)]
use windows_sys::Win32::{
    Devices::{
        Display::GUID_DEVINTERFACE_MONITOR,
        Usb::{
            GUID_DEVINTERFACE_USB_DEVICE, GUID_DEVINTERFACE_USB_HOST_CONTROLLER,
            GUID_DEVINTERFACE_USB_HUB,
        },
    },
    Foundation::{CloseHandle, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM, WAIT_OBJECT_0},
    NetworkManagement::Ndis::GUID_DEVINTERFACE_NET,
    System::{
        Ioctl::GUID_DEVINTERFACE_DISK,
        LibraryLoader::GetModuleHandleW,
        Power::{RegisterPowerSettingNotification, UnregisterPowerSettingNotification, POWERBROADCAST_SETTING},
        SystemServices::{GUID_ACDC_POWER_SOURCE, GUID_CONSOLE_DISPLAY_STATE},
        Threading::{
            CreateEventW, ResetEvent, SetEvent, WaitForSingleObject, INFINITE,
        },
    },
    UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetWindowLongPtrW,
        MsgWaitForMultipleObjectsEx, PeekMessageW, PostQuitMessage, RegisterClassW,
        RegisterDeviceNotificationW, SetWindowLongPtrW, TranslateMessage, UnregisterDeviceNotification,
        CREATESTRUCTW, DBT_DEVICEARRIVAL, DBT_DEVICEREMOVECOMPLETE, DBT_DEVTYP_DEVICEINTERFACE,
        DEVICE_NOTIFY_WINDOW_HANDLE, GWLP_USERDATA, HWND_MESSAGE, MSG, PM_REMOVE, QS_ALLINPUT,
        PBT_POWERSETTINGCHANGE, WM_DESTROY, WM_DEVICECHANGE, WM_NCCREATE, WM_POWERBROADCAST,
        WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, MWMO_INPUTAVAILABLE,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PortKind {
    Charging,
    UsbStorage,
    ExternalDisplay,
    Ethernet,
    ThunderboltDock,
}

impl PortKind {
    pub fn all() -> [Self; 5] {
        [
            Self::Charging,
            Self::UsbStorage,
            Self::ExternalDisplay,
            Self::Ethernet,
            Self::ThunderboltDock,
        ]
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Charging => "Charging / power",
            Self::UsbStorage => "USB storage / pendrive",
            Self::ExternalDisplay => "HDMI / external display",
            Self::Ethernet => "LAN / ethernet",
            Self::ThunderboltDock => "Thunderbolt / dock",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PortRule {
    pub kind: PortKind,
    pub bundle: String,
    pub on_connect: bool,
    pub on_disconnect: bool,
}

impl PortRule {
    pub fn default_for(kind: PortKind) -> Self {
        Self {
            kind,
            bundle: "default".to_string(),
            on_connect: false,
            on_disconnect: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PortCapability {
    pub kind: PortKind,
    pub label: String,
    pub supported: bool,
    pub connected: bool,
}

pub struct PortMonitorHandle {
    stop_tx: mpsc::Sender<()>,
    #[cfg(windows)]
    stop_event: usize,
    join: Option<JoinHandle<()>>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct PortSnapshot {
    charging: Option<bool>,
    usb_storage: Option<bool>,
    external_display: Option<bool>,
    ethernet: Option<bool>,
    thunderbolt_dock: Option<bool>,
}

impl PortMonitorHandle {
    pub fn spawn(on_event: impl Fn(PortKind, bool) + Send + Sync + 'static) -> Self {
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let on_event = Arc::new(on_event);

        #[cfg(windows)]
        {
            let stop_event = unsafe { CreateEventW(null(), 1, 0, null()) };
            if stop_event.is_null() {
                let join = spawn_monitor_thread(stop_rx, on_event);
                return Self {
                    stop_tx,
                    stop_event: 0,
                    join: Some(join),
                };
            }

            let join = spawn_windows_monitor_thread(stop_event as usize, on_event);
            return Self {
                stop_tx,
                stop_event: stop_event as usize,
                join: Some(join),
            };
        }

        #[cfg(not(windows))]
        {
            let join = spawn_monitor_thread(stop_rx, on_event);
            Self {
                stop_tx,
                join: Some(join),
            }
        }
    }
}

impl Drop for PortMonitorHandle {
    fn drop(&mut self) {
        let _ = self.stop_tx.send(());
        #[cfg(windows)]
        {
            if self.stop_event != 0 {
                unsafe {
                    let _ = SetEvent(self.stop_event as HANDLE);
                }
            }
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
        #[cfg(windows)]
        {
            if self.stop_event != 0 {
                unsafe {
                    let _ = CloseHandle(self.stop_event as HANDLE);
                }
            }
        }
    }
}

pub fn probe_port_capabilities() -> Vec<PortCapability> {
    let snapshot = poll_port_snapshot();
    PortKind::all()
        .into_iter()
        .map(|kind| {
            let state = snapshot_for_kind(snapshot, kind);
            PortCapability {
                kind,
                label: kind.label().to_string(),
                supported: state.is_some(),
                connected: state.unwrap_or(false),
            }
        })
        .collect()
}

pub fn platform_label() -> &'static str {
    #[cfg(windows)]
    {
        "Windows"
    }

    #[cfg(target_os = "macos")]
    {
        "macOS"
    }

    #[cfg(target_os = "linux")]
    {
        "Linux"
    }

    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        "Unknown"
    }
}

pub fn port_monitor_mode_label() -> &'static str {
    #[cfg(windows)]
    {
        "Snapshot polling"
    }

    #[cfg(all(target_os = "linux", feature = "linux-udev"))]
    {
        "Event-driven (udev) with snapshot fallback"
    }

    #[cfg(all(target_os = "linux", not(feature = "linux-udev")))]
    {
        "Snapshot polling"
    }

    #[cfg(target_os = "macos")]
    {
        "Event-driven (IOKit) with snapshot fallback"
    }

    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        "Snapshot polling"
    }
}

fn emit_changes(
    previous: PortSnapshot,
    current: PortSnapshot,
    on_event: Arc<dyn Fn(PortKind, bool) + Send + Sync>,
) {
    for kind in PortKind::all() {
        let prev = snapshot_for_kind(previous, kind);
        let next = snapshot_for_kind(current, kind);

        if let (Some(prev), Some(next)) = (prev, next) {
            if prev != next {
                on_event(kind, next);
            }
        }
    }
}

fn snapshot_for_kind(snapshot: PortSnapshot, kind: PortKind) -> Option<bool> {
    match kind {
        PortKind::Charging => snapshot.charging,
        PortKind::UsbStorage => snapshot.usb_storage,
        PortKind::ExternalDisplay => snapshot.external_display,
        PortKind::Ethernet => snapshot.ethernet,
        PortKind::ThunderboltDock => snapshot.thunderbolt_dock,
    }
}

fn poll_port_snapshot() -> PortSnapshot {
    PortSnapshot {
        charging: poll_charging(),
        usb_storage: poll_usb_storage(),
        external_display: poll_external_display(),
        ethernet: poll_ethernet(),
        thunderbolt_dock: poll_thunderbolt_dock(),
    }
}

fn spawn_monitor_thread(
    stop_rx: mpsc::Receiver<()>,
    on_event: Arc<dyn Fn(PortKind, bool) + Send + Sync>,
) -> JoinHandle<()> {
    #[cfg(all(target_os = "linux", feature = "linux-udev"))]
    {
        return thread::spawn(move || spawn_linux_monitor_loop(stop_rx, on_event));
    }

    #[cfg(target_os = "macos")]
    {
        return thread::spawn(move || spawn_macos_monitor_loop(stop_rx, on_event));
    }

    #[cfg(any(
        all(target_os = "linux", not(feature = "linux-udev")),
        all(not(target_os = "linux"), not(target_os = "macos"))
    ))]
    {
        return thread::spawn(move || spawn_polling_monitor_loop(stop_rx, on_event));
    }
}

fn spawn_polling_monitor_loop(
    stop_rx: mpsc::Receiver<()>,
    on_event: Arc<dyn Fn(PortKind, bool) + Send + Sync>,
) {
    let mut previous = poll_port_snapshot();

    loop {
        if stop_rx.try_recv().is_ok() {
            break;
        }

        thread::sleep(Duration::from_secs(1));

        let current = poll_port_snapshot();
        emit_changes(previous, current, on_event.clone());
        previous = current;
    }
}

#[cfg(windows)]
fn spawn_windows_monitor_thread(
    stop_event: usize,
    on_event: Arc<dyn Fn(PortKind, bool) + Send + Sync>,
) -> JoinHandle<()> {
    thread::spawn(move || spawn_windows_monitor_loop(stop_event, on_event))
}

#[cfg(windows)]
fn spawn_windows_monitor_loop(
    stop_event: usize,
    on_event: Arc<dyn Fn(PortKind, bool) + Send + Sync>,
) {
    unsafe {
        let hinstance = GetModuleHandleW(null());
        let class_name = wide_string("TheMoaningGuyPortMonitor");

        let wndclass = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(windows_monitor_wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance as HINSTANCE,
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: null(),
            lpszClassName: class_name.as_ptr(),
        };

        let _ = RegisterClassW(&wndclass);

        let context = Box::new(WindowsMonitorContext {
            dirty_event: CreateEventW(null(), 1, 0, null()),
        });
        if context.dirty_event.is_null() {
            let mut previous = poll_port_snapshot();
            spawn_polling_monitor_loop_windows_fallback(stop_event, on_event, &mut previous);
            return;
        }

        let context_ptr = Box::into_raw(context);
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class_name.as_ptr(),
            class_name.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            std::ptr::null_mut(),
            hinstance as HINSTANCE,
            context_ptr as *const c_void,
        );

        if hwnd.is_null() {
            let context = Box::from_raw(context_ptr);
            if !context.dirty_event.is_null() {
                let _ = CloseHandle(context.dirty_event);
            }
            let mut previous = poll_port_snapshot();
            spawn_polling_monitor_loop_windows_fallback(stop_event, on_event, &mut previous);
            return;
        }

        let mut registrations = windows_register_notifications(hwnd);
        let mut previous = poll_port_snapshot();
        let stop_handle = stop_event as HANDLE;
        let wait_handles = [stop_handle, (*context_ptr).dirty_event];

        loop {
            let result = MsgWaitForMultipleObjectsEx(
                wait_handles.len() as u32,
                wait_handles.as_ptr(),
                INFINITE,
                QS_ALLINPUT,
                MWMO_INPUTAVAILABLE,
            );

            if result == WAIT_OBJECT_0 {
                break;
            }

            if result == WAIT_OBJECT_0 + 1 {
                let current = poll_port_snapshot();
                emit_changes(previous, current, on_event.clone());
                previous = current;
                let _ = ResetEvent((*context_ptr).dirty_event);
                continue;
            }

            if result == WAIT_OBJECT_0 + 2 {
                let mut msg: MSG = std::mem::zeroed();
                while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }

                if WaitForSingleObject((*context_ptr).dirty_event, 0) == WAIT_OBJECT_0 {
                    let current = poll_port_snapshot();
                    emit_changes(previous, current, on_event.clone());
                    previous = current;
                    let _ = ResetEvent((*context_ptr).dirty_event);
                }
                continue;
            }

            if result == u32::MAX {
                break;
            }
        }

        for registration in registrations.device_notifications.drain(..) {
            let _ = UnregisterDeviceNotification(registration);
        }
        for power in registrations.power_notifications.drain(..) {
            let _ = UnregisterPowerSettingNotification(power);
        }
        let _ = DestroyWindow(hwnd);
        let context = Box::from_raw(context_ptr);
        if !context.dirty_event.is_null() {
            let _ = CloseHandle(context.dirty_event);
        }
    }
}

#[cfg(windows)]
unsafe extern "system" fn windows_monitor_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_NCCREATE => {
            let createstruct = &*(lparam as *const CREATESTRUCTW);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, createstruct.lpCreateParams as isize);
            1
        }
        WM_DEVICECHANGE => {
            let change = wparam as u32;
            if matches!(
                change,
                DBT_DEVICEARRIVAL | DBT_DEVICEREMOVECOMPLETE | windows_sys::Win32::UI::WindowsAndMessaging::DBT_DEVNODES_CHANGED
            ) {
                windows_mark_dirty(hwnd);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_POWERBROADCAST => {
            if wparam as u32 == PBT_POWERSETTINGCHANGE {
                let setting = &*(lparam as *const POWERBROADCAST_SETTING);
                if guid_equals(&setting.PowerSetting, &GUID_ACDC_POWER_SOURCE)
                    || guid_equals(&setting.PowerSetting, &GUID_CONSOLE_DISPLAY_STATE)
                {
                    windows_mark_dirty(hwnd);
                }
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

#[cfg(windows)]
unsafe fn windows_mark_dirty(hwnd: HWND) {
    let context_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WindowsMonitorContext;
    if !context_ptr.is_null() && !(*context_ptr).dirty_event.is_null() {
        let _ = SetEvent((*context_ptr).dirty_event);
    }
}

#[cfg(windows)]
struct WindowsMonitorContext {
    dirty_event: HANDLE,
}

#[cfg(windows)]
struct WindowsNotificationRegistrations {
    device_notifications: Vec<windows_sys::Win32::UI::WindowsAndMessaging::HDEVNOTIFY>,
    power_notifications: Vec<windows_sys::Win32::System::Power::HPOWERNOTIFY>,
}

#[cfg(windows)]
unsafe fn windows_register_notifications(hwnd: HWND) -> WindowsNotificationRegistrations {
    let mut device_notifications = Vec::new();

    for guid in [
        GUID_DEVINTERFACE_USB_DEVICE,
        GUID_DEVINTERFACE_DISK,
        GUID_DEVINTERFACE_MONITOR,
        GUID_DEVINTERFACE_NET,
        GUID_DEVINTERFACE_USB_HOST_CONTROLLER,
        GUID_DEVINTERFACE_USB_HUB,
    ] {
        let filter = windows_sys::Win32::UI::WindowsAndMessaging::DEV_BROADCAST_DEVICEINTERFACE_W {
            dbcc_size: size_of::<windows_sys::Win32::UI::WindowsAndMessaging::DEV_BROADCAST_DEVICEINTERFACE_W>()
                as u32,
            dbcc_devicetype: DBT_DEVTYP_DEVICEINTERFACE,
            dbcc_reserved: 0,
            dbcc_classguid: guid,
            dbcc_name: [0],
        };

        let handle = RegisterDeviceNotificationW(
            hwnd as HANDLE,
            &filter as *const _ as *const c_void,
            DEVICE_NOTIFY_WINDOW_HANDLE,
        );
        if !handle.is_null() {
            device_notifications.push(handle);
        }
    }

    let mut power_notifications = Vec::new();
    for guid in [GUID_ACDC_POWER_SOURCE, GUID_CONSOLE_DISPLAY_STATE] {
        let power_notification = RegisterPowerSettingNotification(
            hwnd as HANDLE,
            &guid,
            DEVICE_NOTIFY_WINDOW_HANDLE,
        );
        if power_notification != 0 {
            power_notifications.push(power_notification);
        }
    }

    WindowsNotificationRegistrations {
        device_notifications,
        power_notifications,
    }
}

#[cfg(windows)]
fn spawn_polling_monitor_loop_windows_fallback(
    stop_event: usize,
    on_event: Arc<dyn Fn(PortKind, bool) + Send + Sync>,
    previous: &mut PortSnapshot,
) {
    loop {
        let wait = unsafe { WaitForSingleObject(stop_event as HANDLE, 1000) };
        if wait == WAIT_OBJECT_0 {
            break;
        }

        let current = poll_port_snapshot();
        emit_changes(*previous, current, on_event.clone());
        *previous = current;
    }
}

#[cfg(windows)]
fn guid_equals(a: &windows_sys::core::GUID, b: &windows_sys::core::GUID) -> bool {
    a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}

#[cfg(windows)]
fn wide_string(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

#[cfg(all(target_os = "linux", feature = "linux-udev"))]
fn spawn_linux_monitor_loop(
    stop_rx: mpsc::Receiver<()>,
    on_event: Arc<dyn Fn(PortKind, bool) + Send + Sync>,
) {
    let monitor = match MonitorBuilder::new().and_then(|builder| builder.listen()) {
        Ok(socket) => socket,
        Err(_) => {
            spawn_polling_monitor_loop(stop_rx, on_event);
            return;
        }
    };

    let mut previous = poll_port_snapshot();
    let fd = monitor.as_raw_fd();

    loop {
        if stop_rx.try_recv().is_ok() {
            break;
        }

        let mut saw_event = false;
        let mut pollfd = pollfd {
            fd,
            events: POLLIN,
            revents: 0,
        };

        let poll_result = unsafe { poll(&mut pollfd, 1, 250) };
        if poll_result < 0 {
            thread::sleep(Duration::from_millis(250));
            continue;
        }

        if poll_result == 0 {
            continue;
        }

        let mut events = monitor.iter();
        while let Some(event) = events.next() {
            let subsystem = event
                .subsystem()
                .and_then(|value| value.to_str())
                .unwrap_or("");

            if matches!(subsystem, "power_supply" | "block" | "drm" | "net" | "usb") {
                saw_event = true;
            }
        }

        if saw_event {
            let current = poll_port_snapshot();
            emit_changes(previous, current, on_event.clone());
            previous = current;
        }
    }
}

fn poll_charging() -> Option<bool> {
    #[cfg(windows)]
    {
        poll_charging_windows()
    }

    #[cfg(target_os = "macos")]
    {
        poll_charging_macos()
    }

    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        let manager = Manager::new().ok()?;
        let batteries = manager.batteries().ok()?;

        let mut saw_battery = false;
        let mut charging = false;

        for maybe_battery in batteries {
            let battery = maybe_battery.ok()?;
            saw_battery = true;

            match battery.state() {
                BatteryState::Charging | BatteryState::Full => {
                    charging = true;
                    break;
                }
                _ => {}
            }
        }

        if saw_battery {
            Some(charging)
        } else {
            None
        }
    }
}

fn poll_usb_storage() -> Option<bool> {
    #[cfg(windows)]
    {
        poll_usb_storage_windows()
    }

    #[cfg(target_os = "macos")]
    {
        poll_usb_storage_macos()
    }

    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        let devices = rusb::devices().ok()?;
        for device in devices.iter() {
            let descriptor = device.device_descriptor().ok()?;
            if descriptor.class_code() == 0x08 {
                return Some(true);
            }
        }
        Some(false)
    }
}

fn poll_external_display() -> Option<bool> {
    #[cfg(windows)]
    {
        poll_external_display_windows()
    }

    #[cfg(target_os = "macos")]
    {
        poll_external_display_macos()
    }

    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        let displays = DisplayInfo::all().ok()?;
        Some(displays.into_iter().any(|display| !display.is_builtin))
    }
}

fn poll_ethernet() -> Option<bool> {
    #[cfg(windows)]
    {
        poll_ethernet_windows()
    }

    #[cfg(target_os = "macos")]
    {
        poll_ethernet_macos()
    }

    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        let networks = Networks::new_with_refreshed_list();
        let connected = networks.iter().any(|(name, data)| {
            let name = name.to_lowercase();
            let wired_hint = ["eth", "ethernet", "lan", "en"]
                .iter()
                .any(|needle| name.contains(needle));
            let wireless_hint = ["wifi", "wi-fi", "wlan", "airport", "wireless"]
                .iter()
                .any(|needle| name.contains(needle));

            wired_hint && !wireless_hint && !data.ip_networks().is_empty()
        });

        Some(connected)
    }
}

fn poll_thunderbolt_dock() -> Option<bool> {
    #[cfg(windows)]
    {
        poll_thunderbolt_dock_windows()
    }

    #[cfg(target_os = "macos")]
    {
        poll_thunderbolt_dock_macos()
    }

    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        let devices = rusb::devices().ok()?;
        for device in devices.iter() {
            if is_probably_thunderbolt_or_dock(&device) {
                return Some(true);
            }
        }
        Some(false)
    }
}

#[cfg(target_os = "macos")]
fn spawn_macos_monitor_loop(
    stop_rx: mpsc::Receiver<()>,
    on_event: Arc<dyn Fn(PortKind, bool) + Send + Sync>,
) {
    let (event_tx, event_rx) = mpsc::channel::<()>();
    let event_tx = Box::new(event_tx);
    let event_tx_ptr = Box::into_raw(event_tx) as *mut c_void;

    let notify_port = unsafe { IONotificationPort::create(0) };
    let run_loop_source = unsafe { IONotificationPort::run_loop_source(notify_port) };

    let Some(run_loop) = CFRunLoop::current() else {
        unsafe {
            drop(Box::from_raw(event_tx_ptr as *mut mpsc::Sender<()>));
            IONotificationPort::destroy(notify_port);
        }
        return;
    };

    if let Some(source) = run_loop_source.as_ref() {
        run_loop.add_source(Some(source.as_ref()), kCFRunLoopDefaultMode);
    }

    let mut watchers = Vec::new();
    for class_name in [
        c"IOUSBHostDevice",
        c"IODisplayConnect",
        c"IONetworkInterface",
        c"IOThunderboltPort",
        c"IOThunderboltController",
    ] {
        if let Some(iterator) =
            unsafe { register_macos_matching_notification(notify_port, class_name, event_tx_ptr) }
        {
            watchers.push(iterator);
        }
        if let Some(iterator) = unsafe {
            register_macos_terminated_notification(notify_port, class_name, event_tx_ptr)
        } {
            watchers.push(iterator);
        }
    }

    let mut previous = poll_port_snapshot();

    loop {
        if stop_rx.try_recv().is_ok() {
            break;
        }

        while event_rx.try_recv().is_ok() {}

        CFRunLoop::run_in_mode(kCFRunLoopDefaultMode, 0.5, true);

        if event_rx.try_recv().is_ok() {
            let current = poll_port_snapshot();
            emit_changes(previous, current, on_event.clone());
            previous = current;
            while event_rx.try_recv().is_ok() {}
        }
    }

    unsafe {
        for watcher in watchers {
            IOObjectRelease(watcher);
        }
        drop(Box::from_raw(event_tx_ptr as *mut mpsc::Sender<()>));
        IONotificationPort::destroy(notify_port);
    }
}

#[cfg(target_os = "macos")]
unsafe extern "C-unwind" fn macos_matching_callback(
    ref_con: *mut c_void,
    iterator: io_iterator_t,
) {
    let sender = &*(ref_con as *const mpsc::Sender<()>);

    loop {
        let service = IOIteratorNext(iterator);
        if service == 0 {
            break;
        }

        let _ = sender.send(());
        IOObjectRelease(service);
    }
}

#[cfg(target_os = "macos")]
unsafe fn register_macos_matching_notification(
    notify_port: IONotificationPortRef,
    class_name: &CStr,
    ref_con: *mut c_void,
) -> Option<io_iterator_t> {
    let matching = IOServiceMatching(class_name.as_ptr())?;
    let mut iterator: io_iterator_t = 0;
    let result = IOServiceAddMatchingNotification(
        notify_port,
        std::mem::transmute(kIOMatchedNotification),
        Some(unsafe { std::mem::transmute(matching) }),
        Some(macos_matching_callback),
        ref_con,
        &mut iterator,
    );

    if result != 0 {
        return None;
    }

    arm_macos_iterator(iterator, ref_con);
    Some(iterator)
}

#[cfg(target_os = "macos")]
unsafe fn register_macos_terminated_notification(
    notify_port: IONotificationPortRef,
    class_name: &CStr,
    ref_con: *mut c_void,
) -> Option<io_iterator_t> {
    let matching = IOServiceMatching(class_name.as_ptr())?;
    let mut iterator: io_iterator_t = 0;
    let result = IOServiceAddMatchingNotification(
        notify_port,
        std::mem::transmute(kIOTerminatedNotification),
        Some(unsafe { std::mem::transmute(matching) }),
        Some(macos_matching_callback),
        ref_con,
        &mut iterator,
    );

    if result != 0 {
        return None;
    }

    arm_macos_iterator(iterator, ref_con);
    Some(iterator)
}

#[cfg(target_os = "macos")]
unsafe fn arm_macos_iterator(iterator: io_iterator_t, ref_con: *mut c_void) {
    let sender = &*(ref_con as *const mpsc::Sender<()>);

    loop {
        let service = IOIteratorNext(iterator);
        if service == 0 {
            break;
        }

        let _ = sender.send(());
        IOObjectRelease(service);
    }
}

#[cfg(target_os = "macos")]
fn poll_charging_macos() -> Option<bool> {
    let output = run_macos_command("/usr/bin/pmset", &["-g", "batt"])?;
    let text = output.to_lowercase();

    if text.contains("ac power") {
        Some(true)
    } else if text.contains("battery power") {
        Some(false)
    } else {
        None
    }
}

#[cfg(target_os = "macos")]
fn poll_usb_storage_macos() -> Option<bool> {
    let output = run_macos_command("/usr/sbin/system_profiler", &["SPUSBDataType"])?;
    let text = output.to_lowercase();
    Some(
        text.contains("mass storage")
            || text.contains("usb storage")
            || text.contains("usb attached scsi")
            || text.contains("pendrive"),
    )
}

#[cfg(target_os = "macos")]
fn poll_external_display_macos() -> Option<bool> {
    let output = run_macos_command("/usr/sbin/system_profiler", &["SPDisplaysDataType", "-json"])?;
    let value: serde_json::Value = serde_json::from_str(&output).ok()?;
    Some(macos_has_external_display(&value))
}

#[cfg(target_os = "macos")]
fn poll_ethernet_macos() -> Option<bool> {
    let output = run_macos_command("/usr/sbin/networksetup", &["-listallhardwareports"])?;
    let mut active_devices = Vec::new();
    let mut current_port = String::new();

    for line in output.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Hardware Port: ") {
            current_port = rest.trim().to_lowercase();
        } else if let Some(rest) = line.strip_prefix("Device: ") {
            let current_device = rest.trim().to_string();

            if current_port.contains("ethernet")
                || current_port.contains("lan")
                || current_port.contains("thunderbolt")
            {
                active_devices.push(current_device.clone());
            }
        }
    }

    if active_devices.is_empty() {
        return Some(false);
    }

    for device in active_devices {
        if let Some(ifconfig) = run_macos_command("/sbin/ifconfig", &[&device]) {
            let text = ifconfig.to_lowercase();
            if text.contains("status: active") && text.contains("inet ") {
                return Some(true);
            }
        }
    }

    Some(false)
}

#[cfg(target_os = "macos")]
fn poll_thunderbolt_dock_macos() -> Option<bool> {
    let output = run_macos_command("/usr/sbin/system_profiler", &["SPThunderboltDataType"])?;
    let text = output.to_lowercase();

    Some(
        text.contains("device name:")
            || text.contains("vendor name:")
            || text.contains("dock")
            || text.contains("hub"),
    )
}

#[cfg(target_os = "macos")]
fn run_macos_command(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }

    String::from_utf8(output.stdout).ok()
}

#[cfg(target_os = "macos")]
fn macos_has_external_display(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(map) => {
            let builtin = map
                .get("spdisplays_builtin")
                .and_then(|value| value.as_str())
                .map(|value| value.eq_ignore_ascii_case("spdisplays_yes"))
                .unwrap_or(false);
            let name = map.get("_name").and_then(|value| value.as_str()).unwrap_or("");
            let looks_like_display = map.contains_key("spdisplays_resolution")
                || map.contains_key("spdisplays_display_type")
                || map.contains_key("spdisplays_vendor-id")
                || map.contains_key("spdisplays_ndrvs");

            if looks_like_display && !builtin && !name.is_empty() && name != "Graphics/Displays" {
                return true;
            }

            map.values().any(macos_has_external_display)
        }
        serde_json::Value::Array(items) => items.iter().any(macos_has_external_display),
        _ => false,
    }
}

#[cfg(windows)]
fn poll_charging_windows() -> Option<bool> {
    let mut status = SYSTEM_POWER_STATUS {
        ACLineStatus: 0,
        BatteryFlag: 0,
        BatteryLifePercent: 0,
        SystemStatusFlag: 0,
        BatteryLifeTime: 0,
        BatteryFullLifeTime: 0,
    };

    let ok = unsafe { GetSystemPowerStatus(&mut status) };
    if ok == 0 {
        return None;
    }

    match status.ACLineStatus {
        0 => Some(false),
        1 => Some(true),
        _ => Some((status.BatteryFlag & 8) != 0),
    }
}

#[cfg(windows)]
fn poll_usb_storage_windows() -> Option<bool> {
    let wmi = WMIConnection::new().ok()?;
    let drives: Vec<WindowsDiskDrive> = wmi
        .raw_query("SELECT InterfaceType, PNPDeviceID, Model FROM Win32_DiskDrive")
        .ok()?;

    Some(drives.iter().any(|drive| {
        let interface = drive.interface_type.as_deref().unwrap_or("").to_lowercase();
        let model = drive.model.as_deref().unwrap_or("").to_lowercase();
        let pnp = drive.pnp_device_id.as_deref().unwrap_or("").to_lowercase();

        interface.contains("usb") || pnp.contains("usbstor") || model.contains("usb")
    }))
}

#[cfg(windows)]
fn poll_external_display_windows() -> Option<bool> {
    let wmi = WMIConnection::with_namespace_path("ROOT\\WMI").ok()?;
    let monitors: Vec<WindowsMonitor> = wmi
        .raw_query("SELECT Active FROM WmiMonitorBasicDisplayParams")
        .ok()?;

    Some(monitors.iter().filter(|monitor| monitor.active.unwrap_or(false)).count() > 1)
}

#[cfg(windows)]
fn poll_ethernet_windows() -> Option<bool> {
    let wmi = WMIConnection::new().ok()?;
    let adapters: Vec<WindowsNetworkAdapter> = wmi
        .raw_query(
            "SELECT NetEnabled, PhysicalAdapter, Name, NetConnectionID, AdapterType FROM Win32_NetworkAdapter",
        )
        .ok()?;

    Some(adapters.iter().any(|adapter| {
        if adapter.net_enabled != Some(true) || adapter.physical_adapter == Some(false) {
            return false;
        }

        let name = adapter.name.as_deref().unwrap_or("").to_lowercase();
        let connection = adapter.net_connection_id.as_deref().unwrap_or("").to_lowercase();
        let adapter_type = adapter.adapter_type.as_deref().unwrap_or("").to_lowercase();

        let wired_hint = ["ethernet", "lan", "thunderbolt", "dock"]
            .iter()
            .any(|needle| {
                name.contains(needle)
                    || connection.contains(needle)
                    || adapter_type.contains(needle)
            });
        let wireless_hint = ["wifi", "wi-fi", "wlan", "wireless"]
            .iter()
            .any(|needle| {
                name.contains(needle)
                    || connection.contains(needle)
                    || adapter_type.contains(needle)
            });

        wired_hint && !wireless_hint
    }))
}

#[cfg(windows)]
fn poll_thunderbolt_dock_windows() -> Option<bool> {
    let wmi = WMIConnection::new().ok()?;
    let devices: Vec<WindowsPnPDevice> = wmi
        .raw_query("SELECT Name, Caption, PNPDeviceID FROM Win32_PnPEntity")
        .ok()?;

    Some(devices.iter().any(|device| {
        let name = device.name.as_deref().unwrap_or("").to_lowercase();
        let caption = device.caption.as_deref().unwrap_or("").to_lowercase();
        let pnp = device.pnp_device_id.as_deref().unwrap_or("").to_lowercase();

        [
            "thunderbolt",
            "dock",
            "caldigit",
            "anker",
            "belkin",
            "dell",
            "lenovo",
            "satechi",
            "j5create",
            "ugreen",
            "displaylink",
        ]
        .iter()
        .any(|needle| name.contains(needle) || caption.contains(needle) || pnp.contains(needle))
    }))
}

#[cfg(windows)]
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct WindowsDiskDrive {
    interface_type: Option<String>,
    pnp_device_id: Option<String>,
    model: Option<String>,
}

#[cfg(windows)]
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct WindowsMonitor {
    active: Option<bool>,
}

#[cfg(windows)]
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct WindowsNetworkAdapter {
    net_enabled: Option<bool>,
    physical_adapter: Option<bool>,
    name: Option<String>,
    net_connection_id: Option<String>,
    adapter_type: Option<String>,
}

#[cfg(windows)]
#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct WindowsPnPDevice {
    name: Option<String>,
    caption: Option<String>,
    pnp_device_id: Option<String>,
}

pub fn update_rule_bundle(rules: &mut [PortRule], old_name: &str, new_name: &str) {
    for rule in rules.iter_mut() {
        if rule.bundle == old_name {
            rule.bundle = new_name.to_string();
        }
    }
}

pub fn repair_rules(rules: &mut Vec<PortRule>) {
    let mut by_kind = HashMap::<PortKind, PortRule>::new();

    for rule in rules.drain(..) {
        by_kind.entry(rule.kind).or_insert(rule);
    }

    *rules = PortKind::all()
        .into_iter()
        .map(|kind| {
            by_kind
                .remove(&kind)
                .unwrap_or_else(|| PortRule::default_for(kind))
        })
        .collect();
}

#[cfg(not(windows))]
fn is_probably_thunderbolt_or_dock<T: UsbContext>(device: &rusb::Device<T>) -> bool {
    let descriptor = match device.device_descriptor() {
        Ok(descriptor) => descriptor,
        Err(_) => return false,
    };

    if matches!(descriptor.class_code(), 0x09 | 0xef) {
        return true;
    }

    let handle = match device.open() {
        Ok(handle) => handle,
        Err(_) => return false,
    };

    let languages = match handle.read_languages(Duration::from_millis(80)) {
        Ok(languages) => languages,
        Err(_) => return false,
    };

    if languages.is_empty() {
        return false;
    }

    let lang = languages[0];
    let mut fields = Vec::new();

    if let Ok(product) =
        handle.read_product_string(lang, &descriptor, Duration::from_millis(80))
    {
        fields.push(product.to_lowercase());
    }
    if let Ok(manufacturer) =
        handle.read_manufacturer_string(lang, &descriptor, Duration::from_millis(80))
    {
        fields.push(manufacturer.to_lowercase());
    }

    [
        "thunderbolt",
        "dock",
        "caldigit",
        "anker",
        "belkin",
        "dell",
        "lenovo",
        "satechi",
        "j5create",
        "ugreen",
        "displaylink",
    ]
    .iter()
    .any(|needle| fields.iter().any(|field| field.contains(needle)))
}
