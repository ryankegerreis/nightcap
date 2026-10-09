//! Native Windows controls. COM interfaces live only inside individual calls,
//! so constructing a backend on the UI thread and moving it to the engine is safe.

use std::ptr;

use anyhow::{Context, Result, ensure};
use windows::{
    Win32::{
        Devices::Display::{
            DestroyPhysicalMonitors, GetMonitorBrightness, GetNumberOfPhysicalMonitorsFromHMONITOR,
            GetPhysicalMonitorsFromHMONITOR, PHYSICAL_MONITOR, SetMonitorBrightness,
        },
        Foundation::{
            CloseHandle, ERROR_NOT_ALL_ASSIGNED, ERROR_SUCCESS, GetLastError, HANDLE, LPARAM, LUID,
            RECT, RPC_E_CHANGED_MODE, RPC_E_TOO_LATE, SetLastError,
        },
        Graphics::Gdi::{EnumDisplayMonitors, HDC, HMONITOR},
        Media::Audio::{
            Endpoints::IAudioEndpointVolume, IMMDeviceEnumerator, MMDeviceEnumerator, eConsole,
            eRender,
        },
        Security::{
            AdjustTokenPrivileges, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW,
            SE_PRIVILEGE_ENABLED, SE_SHUTDOWN_NAME, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES,
            TOKEN_QUERY,
        },
        System::{
            Com::{
                CLSCTX_ALL, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance,
                CoInitializeEx, CoInitializeSecurity, CoSetProxyBlanket, CoUninitialize, EOAC_NONE,
                RPC_C_AUTHN_LEVEL_CALL, RPC_C_AUTHN_LEVEL_DEFAULT, RPC_C_IMP_LEVEL_IMPERSONATE,
            },
            Power::SetSuspendState,
            Rpc::{RPC_C_AUTHN_WINNT, RPC_C_AUTHZ_NONE},
            Shutdown::{
                EWX_POWEROFF, EWX_SHUTDOWN, ExitWindowsEx, SHTDN_REASON_FLAG_PLANNED,
                SHTDN_REASON_MAJOR_APPLICATION,
            },
            Threading::{GetCurrentProcess, OpenProcessToken},
            Variant::{VARIANT, VT_I4, VT_UI1, VariantClear, VariantToString, VariantToUInt32},
            Wmi::{
                IWbemClassObject, IWbemLocator, IWbemServices, WBEM_FLAG_CONNECT_USE_MAX_WAIT,
                WBEM_FLAG_FORWARD_ONLY, WBEM_FLAG_RETURN_IMMEDIATELY, WBEM_GENERIC_FLAG_TYPE,
                WbemLocator,
            },
        },
    },
    core::{BOOL, BSTR, PCWSTR, w},
};

use super::{Backends, Brightness, Power, Volume};
use crate::settings::EndAction;

fn normalized(level: f32) -> Result<f32> {
    ensure!(level.is_finite(), "control level must be finite");
    Ok(level.clamp(0.0, 1.0))
}

struct ComApartment(bool);

impl ComApartment {
    fn enter() -> Result<Self> {
        // SAFETY: initialize this calling thread; balance successful calls in Drop.
        let status = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if status == RPC_E_CHANGED_MODE {
            // The caller already owns an STA; do not uninitialize their apartment.
            return Ok(Self(false));
        }
        status
            .ok()
            .context("initializing COM on the calling thread")?;
        Ok(Self(true))
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: this guard owns one successful CoInitializeEx on this thread.
            unsafe { CoUninitialize() };
        }
    }
}

struct WindowsVolume;

fn endpoint() -> Result<IAudioEndpointVolume> {
    // SAFETY: caller holds a ComApartment, and all interfaces are dropped before it.
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .context("creating audio device enumerator")?;
        let device = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .context("finding default render endpoint")?;
        device
            .Activate(CLSCTX_ALL, None)
            .context("activating audio endpoint volume")
    }
}

impl Volume for WindowsVolume {
    fn get(&mut self) -> Result<f32> {
        let _apartment = ComApartment::enter()?;
        let endpoint = endpoint()?;
        // SAFETY: interface belongs to this thread's active COM apartment.
        normalized(
            unsafe { endpoint.GetMasterVolumeLevelScalar() }.context("reading endpoint volume")?,
        )
    }

    fn set(&mut self, level: f32) -> Result<()> {
        let level = normalized(level)?;
        let _apartment = ComApartment::enter()?;
        let endpoint = endpoint()?;
        // SAFETY: valid scalar and interface; null event GUID is supported.
        unsafe { endpoint.SetMasterVolumeLevelScalar(level, ptr::null()) }
            .context("setting endpoint volume")
    }
}

// Generated VARIANT does not own/clear allocations; this wrapper does.
#[derive(Default)]
struct OwnedVariant(VARIANT);

impl Drop for OwnedVariant {
    fn drop(&mut self) {
        // SAFETY: initialized VARIANT, never cleared elsewhere.
        let _ = unsafe { VariantClear(&mut self.0) };
    }
}

fn property(object: &IWbemClassObject, name: PCWSTR) -> Result<OwnedVariant> {
    let mut value = OwnedVariant::default();
    // SAFETY: output is a valid, empty VARIANT; name is a NUL-terminated constant.
    unsafe { object.Get(name, 0, &mut value.0, None, None) }.context("reading WMI property")?;
    Ok(value)
}

fn wmi_services() -> Result<IWbemServices> {
    // SAFETY: caller owns a COM apartment. Existing process security is preserved.
    unsafe {
        if let Err(error) = CoInitializeSecurity(
            None,
            -1,
            None,
            None,
            RPC_C_AUTHN_LEVEL_DEFAULT,
            RPC_C_IMP_LEVEL_IMPERSONATE,
            None,
            EOAC_NONE,
            None,
        ) && error.code() != RPC_E_TOO_LATE
        {
            return Err(error).context("initializing WMI COM security");
        }
        let locator: IWbemLocator = CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER)
            .context("creating WMI locator")?;
        let empty = BSTR::new();
        let services = locator
            .ConnectServer(
                &BSTR::from("ROOT\\WMI"),
                &empty,
                &empty,
                &empty,
                WBEM_FLAG_CONNECT_USE_MAX_WAIT.0,
                &empty,
                None,
            )
            .context("connecting to ROOT\\WMI")?;
        CoSetProxyBlanket(
            &services,
            RPC_C_AUTHN_WINNT,
            RPC_C_AUTHZ_NONE,
            None,
            RPC_C_AUTHN_LEVEL_CALL,
            RPC_C_IMP_LEVEL_IMPERSONATE,
            None,
            EOAC_NONE,
        )
        .context("setting WMI impersonation")?;
        Ok(services)
    }
}

fn wmi_query(services: &IWbemServices, query: &str) -> Result<Vec<IWbemClassObject>> {
    // SAFETY: all objects stay in this call's apartment; storage is initialized.
    unsafe {
        let enumeration = services
            .ExecQuery(
                &BSTR::from("WQL"),
                &BSTR::from(query),
                WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY,
                None,
            )
            .context("querying monitor brightness WMI")?;
        let mut objects = Vec::new();
        loop {
            let mut item = [None];
            let mut returned = 0;
            let status = enumeration.Next(3000, &mut item, &mut returned);
            status.ok().context("enumerating WMI brightness results")?;
            if returned == 0 {
                // WBEM_S_TIMEDOUT is success-valued but must not mean an empty device list.
                ensure!(status.0 != 0x40004, "WMI brightness query timed out");
                break;
            }
            objects.push(
                item[0]
                    .take()
                    .context("WMI returned an empty monitor object")?,
            );
        }
        Ok(objects)
    }
}

fn wmi_levels() -> Result<Vec<f32>> {
    let _apartment = ComApartment::enter()?;
    let services = wmi_services()?;
    let monitors = wmi_query(
        &services,
        "SELECT CurrentBrightness FROM WmiMonitorBrightness WHERE Active = TRUE",
    )?;
    let mut levels = Vec::new();
    for monitor in monitors {
        let value = property(&monitor, w!("CurrentBrightness"))?;
        // SAFETY: VARIANT came from WMI and remains owned until conversion returns.
        let percent = unsafe { VariantToUInt32(&value.0) }.context("converting WMI brightness")?;
        ensure!(percent <= 100, "WMI returned brightness above 100%");
        levels.push(percent as f32 / 100.0);
    }
    ensure!(!levels.is_empty(), "no active WMI brightness panels");
    Ok(levels)
}

fn wmi_set(level: f32) -> Result<()> {
    let percent = (normalized(level)? * 100.0).round() as u8;
    let _apartment = ComApartment::enter()?;
    let services = wmi_services()?;
    let monitors = wmi_query(
        &services,
        "SELECT * FROM WmiMonitorBrightnessMethods WHERE Active = TRUE",
    )?;
    ensure!(!monitors.is_empty(), "no active WMI brightness methods");
    let mut class = None;
    let mut signature = None;
    let mut output_signature = None;
    // SAFETY: GetMethod requires a class definition, not a queried CIM instance.
    unsafe {
        services
            .GetObject(
                &BSTR::from("WmiMonitorBrightnessMethods"),
                WBEM_GENERIC_FLAG_TYPE(0),
                None,
                Some(&mut class),
                None,
            )
            .context("loading WMI brightness methods class")?;
        class
            .context("missing WMI brightness methods class")?
            .GetMethod(
                w!("WmiSetBrightness"),
                0,
                &mut signature,
                &mut output_signature,
            )
            .context("reading WmiSetBrightness signature")?;
    }
    let signature = signature.context("missing WmiSetBrightness input signature")?;
    for monitor in monitors {
        let path = property(&monitor, w!("__PATH"))?;
        let mut path_text = [0u16; 4096];
        // SAFETY: buffer sizes and interface lifetimes are valid within this apartment.
        unsafe {
            VariantToString(&path.0, &mut path_text).context("converting WMI monitor path")?;
        }
        let end = path_text
            .iter()
            .position(|&c| c == 0)
            .context("unterminated WMI path")?;
        let path = BSTR::from(String::from_utf16(&path_text[..end]).context("decoding WMI path")?);
        // SAFETY: signature is returned by WMI; the new instance is local to this call.
        let input = unsafe { signature.SpawnInstance(0) }
            .context("creating WmiSetBrightness parameters")?;
        let mut timeout = OwnedVariant::default();
        let mut brightness = OwnedVariant::default();
        // SAFETY: assign matching scalar tags and union members; neither allocates.
        unsafe {
            (*timeout.0.Anonymous.Anonymous).vt = VT_I4;
            (*timeout.0.Anonymous.Anonymous).Anonymous.lVal = 0;
            (*brightness.0.Anonymous.Anonymous).vt = VT_UI1;
            (*brightness.0.Anonymous.Anonymous).Anonymous.bVal = percent;
            input
                .Put(w!("Timeout"), 0, &timeout.0, 0)
                .context("setting WMI timeout parameter")?;
            input
                .Put(w!("Brightness"), 0, &brightness.0, 0)
                .context("setting WMI brightness parameter")?;
            let mut output = None;
            services
                .ExecMethod(
                    &path,
                    &BSTR::from("WmiSetBrightness"),
                    WBEM_GENERIC_FLAG_TYPE(0),
                    None,
                    &input,
                    Some(&mut output),
                    None,
                )
                .context("calling WmiSetBrightness")?;
            let returned = property(
                &output.context("WmiSetBrightness returned no result")?,
                w!("ReturnValue"),
            )?;
            let code =
                VariantToUInt32(&returned.0).context("reading WmiSetBrightness return value")?;
            ensure!(code == 0, "WmiSetBrightness returned {code}");
        }
    }
    Ok(())
}

struct PhysicalMonitors(Vec<PHYSICAL_MONITOR>);

impl Drop for PhysicalMonitors {
    fn drop(&mut self) {
        if !self.0.is_empty() {
            // SAFETY: this array owns the handles returned by dxva2, exactly once.
            let _ = unsafe { DestroyPhysicalMonitors(&self.0) };
        }
    }
}

unsafe extern "system" fn collect_monitor(
    monitor: HMONITOR,
    _: HDC,
    _: *mut RECT,
    data: LPARAM,
) -> BOOL {
    // SAFETY: data points to the live Vec supplied to synchronous EnumDisplayMonitors.
    unsafe { &mut *(data.0 as *mut Vec<HMONITOR>) }.push(monitor);
    BOOL(1)
}

fn physical_monitors() -> Result<PhysicalMonitors> {
    let mut logical = Vec::<HMONITOR>::new();
    // SAFETY: callback data remains alive during this synchronous enumeration.
    unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(collect_monitor),
            LPARAM((&mut logical as *mut Vec<HMONITOR>) as isize),
        )
    }
    .ok()
    .context("enumerating display monitors")?;
    let mut physical = PhysicalMonitors(Vec::new());
    for monitor in logical {
        let mut count = 0;
        // SAFETY: monitor came from enumeration and count is valid output storage.
        if unsafe { GetNumberOfPhysicalMonitorsFromHMONITOR(monitor, &mut count) }.is_err() {
            continue;
        }
        ensure!(count <= 256, "unexpected physical monitor count {count}");
        if count == 0 {
            continue;
        }
        let mut handles = vec![PHYSICAL_MONITOR::default(); count as usize];
        // SAFETY: array length matches the count queried from dxva2.
        if unsafe { GetPhysicalMonitorsFromHMONITOR(monitor, &mut handles) }.is_ok() {
            physical.0.extend(handles);
        }
    }
    ensure!(!physical.0.is_empty(), "no physical monitor handles");
    Ok(physical)
}

fn ddc_range(monitor: &PHYSICAL_MONITOR) -> Result<(u32, u32, u32)> {
    let (mut min, mut current, mut max) = (0, 0, 0);
    // SAFETY: handle is owned by a live PhysicalMonitors, output pointers are valid.
    ensure!(
        unsafe { GetMonitorBrightness(monitor.hPhysicalMonitor, &mut min, &mut current, &mut max) }
            != 0,
        "reading DDC/CI brightness: {}",
        windows::core::Error::from_thread()
    );
    ensure!(
        max > min && (min..=max).contains(&current),
        "invalid DDC/CI brightness range"
    );
    Ok((min, current, max))
}

fn ddc_levels() -> Result<Vec<f32>> {
    let monitors = physical_monitors()?;
    let levels: Vec<_> = monitors
        .0
        .iter()
        .filter_map(|monitor| ddc_range(monitor).ok())
        .map(|(min, current, max)| (current - min) as f32 / (max - min) as f32)
        .collect();
    ensure!(!levels.is_empty(), "no monitors support DDC/CI brightness");
    Ok(levels)
}

fn ddc_set(level: f32) -> Result<()> {
    let level = normalized(level)?;
    let monitors = physical_monitors()?;
    let mut count = 0;
    for monitor in &monitors.0 {
        // Unsupported external monitors are skipped; supported ones are all updated.
        if let Ok((min, _, max)) = ddc_range(monitor) {
            let value = (f64::from(min) + f64::from(level) * f64::from(max - min)).round() as u32;
            // SAFETY: live handle and value within the queried monitor range.
            ensure!(
                unsafe { SetMonitorBrightness(monitor.hPhysicalMonitor, value) } != 0,
                "setting DDC/CI brightness: {}",
                windows::core::Error::from_thread()
            );
            count += 1;
        }
    }
    ensure!(count > 0, "no monitors support DDC/CI brightness");
    Ok(())
}

struct WindowsBrightness {
    wmi: bool,
    ddc: bool,
}

impl Brightness for WindowsBrightness {
    fn get(&mut self) -> Result<f32> {
        let mut levels = Vec::new();
        if self.wmi {
            levels.extend(wmi_levels()?);
        }
        if self.ddc {
            levels.extend(ddc_levels()?);
        }
        ensure!(!levels.is_empty(), "no supported brightness monitors");
        Ok(levels.iter().sum::<f32>() / levels.len() as f32)
    }

    fn set(&mut self, level: f32) -> Result<()> {
        if self.wmi {
            wmi_set(level)?;
        }
        if self.ddc {
            ddc_set(level)?;
        }
        Ok(())
    }
}

struct ShutdownPrivilege {
    token: HANDLE,
    previous: TOKEN_PRIVILEGES,
}

impl ShutdownPrivilege {
    fn enable() -> Result<Self> {
        let mut token = HANDLE::default();
        // SAFETY: requesting a token for the current process; output pointer valid.
        unsafe {
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
                &mut token,
            )
        }
        .context("opening process token for shutdown privilege")?;
        let mut guard = Self {
            token,
            previous: TOKEN_PRIVILEGES::default(),
        };
        let mut luid = LUID::default();
        // SAFETY: valid token and single-entry privilege structure; capture previous state.
        unsafe {
            LookupPrivilegeValueW(None, SE_SHUTDOWN_NAME, &mut luid)
                .context("looking up SeShutdownPrivilege")?;
            let request = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES {
                    Luid: luid,
                    Attributes: SE_PRIVILEGE_ENABLED,
                }],
            };
            SetLastError(ERROR_SUCCESS);
            AdjustTokenPrivileges(
                token,
                false,
                Some(&request),
                size_of::<TOKEN_PRIVILEGES>() as u32,
                Some(&mut guard.previous),
                None,
            )
            .context("enabling SeShutdownPrivilege")?;
            ensure!(
                GetLastError() != ERROR_NOT_ALL_ASSIGNED,
                "SeShutdownPrivilege is not available to this user"
            );
        }
        Ok(guard)
    }
}

impl Drop for ShutdownPrivilege {
    fn drop(&mut self) {
        // SAFETY: guard owns the token and captured privilege state.
        unsafe {
            if self.previous.PrivilegeCount > 0 {
                let _ =
                    AdjustTokenPrivileges(self.token, false, Some(&self.previous), 0, None, None);
            }
            let _ = CloseHandle(self.token);
        }
    }
}

struct WindowsPower;

impl Power for WindowsPower {
    fn perform(&mut self, action: EndAction) -> Result<()> {
        let _privilege = ShutdownPrivilege::enable()?;
        // SAFETY: privilege enabled; no force flags, preserving normal OS vetoes.
        unsafe {
            match action {
                EndAction::Sleep | EndAction::Hibernate => {
                    ensure!(
                        SetSuspendState(action == EndAction::Hibernate, false, false),
                        "requesting {action:?}: {}",
                        windows::core::Error::from_thread()
                    );
                }
                EndAction::Shutdown => ExitWindowsEx(
                    EWX_SHUTDOWN | EWX_POWEROFF,
                    SHTDN_REASON_MAJOR_APPLICATION | SHTDN_REASON_FLAG_PLANNED,
                )
                .context("requesting Windows shutdown")?,
            }
        }
        Ok(())
    }
}

pub fn backends() -> Backends {
    let mut brightness = WindowsBrightness {
        wmi: wmi_levels().is_ok(),
        ddc: ddc_levels().is_ok(),
    };
    let brightness = brightness
        .get()
        .ok()
        .map(|_| Box::new(brightness) as Box<dyn Brightness>);
    let mut volume = WindowsVolume;
    let volume = volume
        .get()
        .ok()
        .map(|_| Box::new(volume) as Box<dyn Volume>);
    Backends {
        brightness,
        volume,
        power: Box::new(WindowsPower),
    }
}
