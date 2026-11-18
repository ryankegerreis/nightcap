//! Native Windows controls. COM interfaces live only inside individual calls,
//! so constructing a backend on the UI thread and moving it to the engine is safe.

use std::{collections::BTreeMap, ptr};

use anyhow::{Context, Result, ensure};
use windows::{
    Win32::{
        Devices::Display::{
            DestroyPhysicalMonitors, GetMonitorBrightness, GetNumberOfPhysicalMonitorsFromHMONITOR,
            GetPhysicalMonitorsFromHMONITOR, PHYSICAL_MONITOR, SetMonitorBrightness,
        },
        Foundation::{
            CloseHandle, ERROR_NOT_ALL_ASSIGNED, ERROR_NOT_FOUND, ERROR_SUCCESS, GetLastError,
            HANDLE, LPARAM, LUID, RECT, RPC_E_CHANGED_MODE, RPC_E_TOO_LATE, SetLastError,
        },
        Graphics::Gdi::{EnumDisplayMonitors, HDC, HMONITOR},
        Media::Audio::{
            AUDCLNT_E_DEVICE_INVALIDATED, DEVICE_STATE_ACTIVE, Endpoints::IAudioEndpointVolume,
            IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, eConsole, eRender,
        },
        Security::{
            AdjustTokenPrivileges, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW,
            SE_PRIVILEGE_ENABLED, SE_SHUTDOWN_NAME, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES,
            TOKEN_QUERY,
        },
        System::{
            Com::{
                CLSCTX_ALL, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance,
                CoInitializeEx, CoInitializeSecurity, CoSetProxyBlanket, CoTaskMemFree,
                CoUninitialize, EOAC_NONE, RPC_C_AUTHN_LEVEL_CALL, RPC_C_AUTHN_LEVEL_DEFAULT,
                RPC_C_IMP_LEVEL_IMPERSONATE,
            },
            Power::{GetPwrCapabilities, SYSTEM_POWER_CAPABILITIES, SetSuspendState},
            Rpc::{RPC_C_AUTHN_WINNT, RPC_C_AUTHZ_NONE},
            Shutdown::{
                EWX_POWEROFF, EWX_SHUTDOWN, ExitWindowsEx, SHTDN_REASON_FLAG_PLANNED,
                SHTDN_REASON_MAJOR_APPLICATION,
            },
            SystemServices::HIBERFILE_TYPE_FULL,
            Threading::{GetCurrentProcess, OpenProcessToken},
            Variant::{VARIANT, VT_I4, VT_UI1, VariantClear, VariantToString, VariantToUInt32},
            Wmi::{
                IWbemClassObject, IWbemLocator, IWbemServices, WBEM_FLAG_CONNECT_USE_MAX_WAIT,
                WBEM_FLAG_FORWARD_ONLY, WBEM_FLAG_RETURN_IMMEDIATELY, WBEM_GENERIC_FLAG_TYPE,
                WbemLocator,
            },
        },
    },
    core::{BOOL, BSTR, HRESULT, PCWSTR, PWSTR, w},
};

use super::{
    Backends, Brightness, Disconnected, Power, Volume,
    display_group::{CapturedDisplay, DisplayDriver, DisplayGroup, DisplayWrite, NotAttempted},
    normalized,
};
use crate::settings::EndAction;

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

fn com_call<T>(call: impl FnOnce() -> Result<T>) -> Result<T> {
    let _apartment = ComApartment::enter()?;
    call().map_err(|error| {
        let message = format!("{error:#}");
        // Keep the kind the engine acts on; drop any apartment-bound error source.
        if error.is::<Disconnected>() {
            Disconnected(message).into()
        } else {
            anyhow::anyhow!(message)
        }
    })
}

#[derive(Default)]
struct WindowsVolume {
    endpoint_id: Option<Vec<u16>>,
}

struct AudioEndpointId(PWSTR);

impl Drop for AudioEndpointId {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(Some(self.0.0.cast())) };
    }
}

fn endpoint(id: Option<&[u16]>) -> Result<(IMMDevice, IAudioEndpointVolume)> {
    // SAFETY: caller holds a ComApartment, and all interfaces are dropped before it.
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .context("creating audio device enumerator")?;
        let device = match id {
            Some(id) => {
                let device = enumerator
                    .GetDevice(PCWSTR(id.as_ptr()))
                    .map_err(endpoint_error)
                    .context("reopening captured render endpoint")?;
                // Unplugged and disabled outputs stay enumerable but cannot take a level.
                if device
                    .GetState()
                    .context("reading captured endpoint state")?
                    != DEVICE_STATE_ACTIVE
                {
                    return Err(disconnected_endpoint());
                }
                device
            }
            None => enumerator
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .context("finding default render endpoint")?,
        };
        let volume = device
            .Activate(CLSCTX_ALL, None)
            .map_err(endpoint_error)
            .context("activating audio endpoint volume")?;
        Ok((device, volume))
    }
}

fn disconnected_endpoint() -> anyhow::Error {
    Disconnected("the captured audio output is disconnected".into()).into()
}

fn endpoint_error(error: windows::core::Error) -> anyhow::Error {
    if [
        HRESULT::from_win32(ERROR_NOT_FOUND.0),
        AUDCLNT_E_DEVICE_INVALIDATED,
    ]
    .contains(&error.code())
    {
        disconnected_endpoint()
    } else {
        error.into()
    }
}

impl WindowsVolume {
    fn get_with(&mut self, read: impl FnOnce() -> Result<(Vec<u16>, f32)>) -> Result<f32> {
        let (id, level) = read()?;
        let level = normalized(level)?;
        self.endpoint_id = Some(id);
        Ok(level)
    }

    fn set_with(
        &mut self,
        level: f32,
        write: impl FnOnce(Option<&[u16]>, f32) -> Result<()>,
    ) -> Result<()> {
        let level = normalized(level)?;
        let id = self
            .endpoint_id
            .as_deref()
            .context("volume has no captured endpoint")?;
        write(Some(id), level)
    }
}

impl Volume for WindowsVolume {
    fn get(&mut self) -> Result<f32> {
        self.get_with(|| {
            com_call(|| {
                let (device, endpoint) = endpoint(None)?;
                let id = AudioEndpointId(
                    unsafe { device.GetId() }.context("reading audio endpoint ID")?,
                );
                let id = unsafe { id.0.as_wide() }
                    .iter()
                    .copied()
                    .chain([0])
                    .collect();
                let level = unsafe { endpoint.GetMasterVolumeLevelScalar() }
                    .context("reading endpoint volume")?;
                Ok((id, level))
            })
        })
    }

    fn set(&mut self, level: f32) -> Result<()> {
        self.set_with(level, |id, level| {
            com_call(|| {
                let (_device, endpoint) = endpoint(id)?;
                unsafe { endpoint.SetMasterVolumeLevelScalar(level, ptr::null()) }
                    .map_err(endpoint_error)
                    .context("setting endpoint volume")
            })
        })
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

fn wmi_levels() -> Result<BTreeMap<String, f32>> {
    com_call(|| {
        let services = wmi_services()?;
        let monitors = wmi_query(
            &services,
            "SELECT InstanceName, CurrentBrightness FROM WmiMonitorBrightness WHERE Active = TRUE",
        )?;
        let mut levels = BTreeMap::new();
        for monitor in monitors {
            let value = property(&monitor, w!("CurrentBrightness"))?;
            // SAFETY: VARIANT came from WMI and remains owned until conversion returns.
            let percent =
                unsafe { VariantToUInt32(&value.0) }.context("converting WMI brightness")?;
            ensure!(percent <= 100, "WMI returned brightness above 100%");
            levels.insert(wmi_instance_name(&monitor)?, percent as f32 / 100.0);
        }
        ensure!(!levels.is_empty(), "no active WMI brightness panels");
        Ok(levels)
    })
}

fn wmi_instance_name(monitor: &IWbemClassObject) -> Result<String> {
    let value = property(monitor, w!("InstanceName"))?;
    let mut text = [0u16; 4096];
    // SAFETY: value is an owned WMI VARIANT and the output buffer is valid.
    unsafe { VariantToString(&value.0, &mut text) }.context("reading WMI instance name")?;
    let end = text
        .iter()
        .position(|&c| c == 0)
        .context("unterminated WMI instance name")?;
    String::from_utf16(&text[..end]).context("decoding WMI instance name")
}

struct WmiSession {
    services: IWbemServices,
    signature: IWbemClassObject,
    panels: BTreeMap<String, IWbemClassObject>,
}

impl WmiSession {
    fn open() -> Result<Self> {
        let services = wmi_services()?;
        let monitors = wmi_query(
            &services,
            "SELECT * FROM WmiMonitorBrightnessMethods WHERE Active = TRUE",
        )?;
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
        // A panel whose identity cannot be read is reported as unavailable when written.
        let panels = monitors
            .into_iter()
            .filter_map(|monitor| Some((wmi_instance_name(&monitor).ok()?, monitor)))
            .collect();
        Ok(Self {
            services,
            signature: signature.context("missing WmiSetBrightness input signature")?,
            panels,
        })
    }

    fn set(&self, name: &str, percent: u32) -> Result<()> {
        let monitor = self
            .panels
            .get(name)
            .with_context(|| format!("captured WMI panel {name} is unavailable"))?;
        let percent = u8::try_from(percent).context("WMI brightness above 100%")?;
        let path = property(monitor, w!("__PATH"))?;
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
        let input = unsafe { self.signature.SpawnInstance(0) }
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
            self.services
                .ExecMethod(
                    &path,
                    &BSTR::from("WmiSetBrightness"),
                    WBEM_GENERIC_FLAG_TYPE(0),
                    None,
                    &input,
                    Some(&mut output),
                    None,
                )
                .with_context(|| format!("calling WmiSetBrightness on {name}"))?;
            let returned = property(
                &output.context("WmiSetBrightness returned no result")?,
                w!("ReturnValue"),
            )?;
            let code =
                VariantToUInt32(&returned.0).context("reading WmiSetBrightness return value")?;
            ensure!(code == 0, "WmiSetBrightness on {name} returned {code}");
        }
        Ok(())
    }
}

/// Writes every panel through one WMI session, opened and dropped in one apartment.
fn write_wmi(panels: &[(&str, u32)]) -> Vec<Result<()>> {
    if panels.is_empty() {
        return Vec::new();
    }
    let results = com_call(|| {
        let session = WmiSession::open()?;
        // Errors become text here because their sources are bound to this apartment.
        Ok(panels
            .iter()
            .map(|&(name, percent)| {
                session
                    .set(name, percent)
                    .map_err(|error| format!("{error:#}"))
            })
            .collect::<Vec<_>>())
    });
    match results {
        Ok(results) => results
            .into_iter()
            .map(|result| result.map_err(|error| anyhow::anyhow!(error)))
            .collect(),
        // The apartment or session could not open, so no panel was written.
        Err(error) => {
            let message = format!("{error:#}");
            panels
                .iter()
                .map(|_| Err(NotAttempted(message.clone()).into()))
                .collect()
        }
    }
}

struct PhysicalMonitors(Vec<PHYSICAL_MONITOR>);

// SAFETY: dxva2 monitor handles are not COM/AppKit objects and have no thread
// affinity. Ownership stays exclusive; Drop destroys them once on the worker.
unsafe impl Send for PhysicalMonitors {}

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

struct DdcDisplays {
    monitors: PhysicalMonitors,
    displays: Vec<CapturedDisplay<DisplayKey>>,
}

fn write_ddc(
    value: u32,
    write: impl FnOnce(u32) -> Result<()>,
    mut read: impl FnMut() -> Result<(u32, u32, u32)>,
) -> Result<()> {
    write(value)?;
    let matches = |(min, current, max): (u32, u32, u32)| {
        current.abs_diff(value) <= ((max - min) / 100).max(1)
    };
    // Monitors are often still busy just after a write, so a failed read also retries.
    if read().is_ok_and(matches) {
        return Ok(());
    }
    std::thread::sleep(std::time::Duration::from_millis(100));
    ensure!(
        matches(read()?),
        "DDC/CI brightness did not reach the requested level {value}"
    );
    Ok(())
}

impl DdcDisplays {
    fn capture() -> Result<Self> {
        let monitors = physical_monitors()?;
        let displays: Vec<_> = monitors
            .0
            .iter()
            .enumerate()
            .filter_map(|(index, monitor)| {
                ddc_range(monitor)
                    .ok()
                    .map(|(min, current, max)| CapturedDisplay {
                        key: DisplayKey::Ddc(index),
                        level: (current - min) as f32 / (max - min) as f32,
                        min,
                        max,
                    })
            })
            .collect();
        ensure!(
            !displays.is_empty(),
            "no monitors support DDC/CI brightness"
        );
        Ok(Self { monitors, displays })
    }
}

#[derive(Clone, Debug, PartialEq)]
enum DisplayKey {
    Wmi(String),
    // Index into the retained physical monitor handles.
    Ddc(usize),
}

#[derive(Default)]
struct WindowsDisplays {
    monitors: Option<PhysicalMonitors>,
}

impl WindowsDisplays {
    /// Either provider may be unavailable while the other's displays still fade.
    fn merge(
        &mut self,
        wmi: Result<BTreeMap<String, f32>>,
        ddc: Result<DdcDisplays>,
    ) -> Vec<CapturedDisplay<DisplayKey>> {
        let mut displays: Vec<_> = wmi
            .unwrap_or_default()
            .into_iter()
            .map(|(name, level)| CapturedDisplay {
                key: DisplayKey::Wmi(name),
                level,
                min: 0,
                max: 100,
            })
            .collect();
        self.monitors = ddc.ok().map(|ddc| {
            displays.extend(ddc.displays);
            ddc.monitors
        });
        displays
    }

    fn write_monitor(&self, index: usize, value: u32) -> Result<()> {
        let monitor = self
            .monitors
            .as_ref()
            .and_then(|monitors| monitors.0.get(index))
            .context("captured DDC/CI monitor is unavailable")?;
        write_ddc(
            value,
            |value| {
                ensure!(
                    // SAFETY: the handle is owned by the retained PhysicalMonitors.
                    unsafe { SetMonitorBrightness(monitor.hPhysicalMonitor, value) } != 0,
                    "setting DDC/CI brightness: {}",
                    windows::core::Error::from_thread()
                );
                Ok(())
            },
            || ddc_range(monitor),
        )
    }
}

impl DisplayDriver for WindowsDisplays {
    type Key = DisplayKey;

    fn capture(&mut self) -> Result<Vec<CapturedDisplay<DisplayKey>>> {
        Ok(self.merge(wmi_levels(), DdcDisplays::capture()))
    }

    fn write(&mut self, writes: &[DisplayWrite<DisplayKey>]) -> Vec<Result<()>> {
        let panels: Vec<_> = writes
            .iter()
            .filter_map(|write| match &write.key {
                DisplayKey::Wmi(name) => Some((name.as_str(), write.value)),
                DisplayKey::Ddc(_) => None,
            })
            .collect();
        let mut panel_results = write_wmi(&panels).into_iter();
        writes
            .iter()
            .map(|write| match &write.key {
                DisplayKey::Wmi(_) => panel_results
                    .next()
                    .unwrap_or_else(|| Err(anyhow::anyhow!("the WMI write was not attempted"))),
                DisplayKey::Ddc(index) => self.write_monitor(*index, write.value),
            })
            .collect()
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
        let mut return_length = 0;
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
                Some(&mut return_length),
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

fn hibernate_supported() -> bool {
    let mut capabilities = SYSTEM_POWER_CAPABILITIES::default();
    // SAFETY: capabilities is a valid, writable output structure; this only reads OS state.
    if !unsafe { GetPwrCapabilities(&mut capabilities) } {
        log::debug!(
            "Hibernate capability check failed: {}",
            windows::core::Error::from_thread()
        );
        return false;
    }
    if !hibernation_configured(&capabilities) {
        return false;
    }
    // The guard immediately restores the token's previous privileges. No power action runs.
    match ShutdownPrivilege::enable() {
        Ok(_privilege) => true,
        Err(error) => {
            log::debug!("Hibernate privilege is unavailable: {error:#}");
            false
        }
    }
}

fn hibernation_configured(capabilities: &SYSTEM_POWER_CAPABILITIES) -> bool {
    capabilities.SystemS4
        && capabilities.HiberFilePresent
        && u32::from(capabilities.HiberFileType) == HIBERFILE_TYPE_FULL
}

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

/// Whether any display offers native brightness, whatever its current level.
/// Each fade captures afresh, so a display at its minimum now (and left out of
/// that capture) can still fade natively after the user raises it.
fn brightness_supported(wmi: Result<BTreeMap<String, f32>>, ddc: Result<DdcDisplays>) -> bool {
    wmi.is_ok_and(|panels| !panels.is_empty()) || ddc.is_ok_and(|ddc| !ddc.displays.is_empty())
}

pub fn backends() -> Backends {
    let brightness = brightness_supported(wmi_levels(), DdcDisplays::capture())
        .then(|| Box::new(DisplayGroup::new(WindowsDisplays::default())) as Box<dyn Brightness>);
    let mut volume = WindowsVolume::default();
    let volume = volume
        .get()
        .ok()
        .map(|_| Box::new(volume) as Box<dyn Volume>);
    Backends {
        brightness,
        volume,
        power: Box::new(WindowsPower),
        hibernate_supported: hibernate_supported(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ddc_ignoring_successful_writes_reports_failure_for_overlay_fallback() {
        let mut written = None;
        let result = write_ddc(
            5,
            |value| {
                written = Some(value);
                Ok(())
            },
            || Ok((0, 10, 100)),
        );
        assert_eq!(written, Some(5));
        assert!(result.is_err());
    }

    #[test]
    fn ddc_accepts_applied_or_single_step_rounded_writes() {
        for actual in [4, 5, 6] {
            write_ddc(5, |_| Ok(()), || Ok((0, actual, 100))).unwrap();
        }
    }

    #[test]
    fn ddc_retries_a_read_that_fails_right_after_the_write() {
        let mut reads = 0;
        write_ddc(
            5,
            |_| Ok(()),
            || {
                reads += 1;
                ensure!(reads > 1, "monitor busy");
                Ok((0, 5, 100))
            },
        )
        .unwrap();
        assert_eq!(reads, 2);
    }

    use windows::Win32::Security::{GetTokenInformation, TokenPrivileges};
    use windows::Win32::System::SystemServices::{HIBERFILE_TYPE_NONE, HIBERFILE_TYPE_REDUCED};

    #[derive(Debug)]
    struct ApartmentBoundFailure(std::sync::Arc<std::sync::atomic::AtomicBool>);

    impl std::fmt::Display for ApartmentBoundFailure {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("apartment-bound failure")
        }
    }

    impl std::error::Error for ApartmentBoundFailure {}

    impl Drop for ApartmentBoundFailure {
        fn drop(&mut self) {
            use windows::Win32::System::Com::{APTTYPE, APTTYPEQUALIFIER, CoGetApartmentType};
            let mut apartment = APTTYPE::default();
            let mut qualifier = APTTYPEQUALIFIER::default();
            let active = unsafe { CoGetApartmentType(&mut apartment, &mut qualifier) }.is_ok();
            self.0.store(active, std::sync::atomic::Ordering::SeqCst);
        }
    }

    #[test]
    fn com_errors_are_released_before_the_call_apartment_is_closed() {
        std::thread::spawn(|| {
            let released = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let error =
                com_call::<()>(|| Err(ApartmentBoundFailure(released.clone()).into())).unwrap_err();
            assert!(released.load(std::sync::atomic::Ordering::SeqCst));
            assert!(error.to_string().contains("apartment-bound failure"));
            drop(error);
        })
        .join()
        .unwrap();
    }

    #[test]
    fn wmi_error_can_be_reported_after_a_fresh_mta_is_closed() {
        std::thread::spawn(|| {
            let error = com_call(|| {
                let services = wmi_services()?;
                wmi_query(&services, "SELECT * FROM NightcapMissingBrightnessClass")?;
                Ok(())
            })
            .unwrap_err();
            assert!(!error.to_string().is_empty());
            drop(error);
        })
        .join()
        .unwrap();
    }

    fn shutdown_privilege_attributes() -> Option<u32> {
        let mut token = HANDLE::default();
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.unwrap();
        let mut luid = LUID::default();
        let mut length = 0;
        let result = (|| -> Result<Option<u32>> {
            unsafe { LookupPrivilegeValueW(None, SE_SHUTDOWN_NAME, &mut luid) }?;
            let _ = unsafe { GetTokenInformation(token, TokenPrivileges, None, 0, &mut length) };
            let mut buffer = vec![0u32; (length as usize).div_ceil(size_of::<u32>())];
            unsafe {
                GetTokenInformation(
                    token,
                    TokenPrivileges,
                    Some(buffer.as_mut_ptr().cast()),
                    length,
                    &mut length,
                )?;
                let privileges = &*buffer.as_ptr().cast::<TOKEN_PRIVILEGES>();
                let entries = std::slice::from_raw_parts(
                    privileges.Privileges.as_ptr(),
                    privileges.PrivilegeCount as usize,
                );
                Ok(entries
                    .iter()
                    .find(|entry| {
                        entry.Luid.LowPart == luid.LowPart && entry.Luid.HighPart == luid.HighPart
                    })
                    .map(|entry| entry.Attributes.0))
            }
        })();
        unsafe { CloseHandle(token) }.unwrap();
        result.unwrap()
    }

    #[test]
    fn shutdown_privilege_guard_restores_the_process_token() {
        let before = shutdown_privilege_attributes();
        if before.is_none() {
            assert!(ShutdownPrivilege::enable().is_err());
            return;
        }
        let guard =
            ShutdownPrivilege::enable().expect("acquire shutdown privilege without a power action");
        assert_ne!(
            shutdown_privilege_attributes().unwrap() & SE_PRIVILEGE_ENABLED.0,
            0
        );
        drop(guard);
        assert_eq!(shutdown_privilege_attributes(), before);
    }

    fn endpoint_id(name: &str) -> Vec<u16> {
        name.encode_utf16().chain([0]).collect()
    }

    #[derive(Default)]
    struct AudioDevices {
        default: String,
        levels: BTreeMap<String, f32>,
        writes: Vec<(String, f32)>,
    }

    impl AudioDevices {
        fn read(&self) -> Result<(Vec<u16>, f32)> {
            let level = self
                .levels
                .get(&self.default)
                .context("default disconnected")?;
            Ok((endpoint_id(&self.default), *level))
        }

        fn write(&mut self, id: Option<&[u16]>, level: f32) -> Result<()> {
            let name = match id {
                Some(id) => String::from_utf16(&id[..id.len() - 1])?,
                None => self.default.clone(),
            };
            *self
                .levels
                .get_mut(&name)
                .context("endpoint disconnected")? = level;
            self.writes.push((name, level));
            Ok(())
        }
    }

    fn audio_devices() -> AudioDevices {
        AudioDevices {
            default: "speakers".into(),
            levels: BTreeMap::from([("speakers".into(), 0.8), ("headphones".into(), 0.4)]),
            ..Default::default()
        }
    }

    #[test]
    fn volume_rejects_writes_before_a_successful_capture() {
        let mut volume = WindowsVolume::default();
        let mut devices = audio_devices();
        assert!(
            volume
                .set_with(0.2, |id, level| devices.write(id, level))
                .is_err()
        );
        assert!(devices.writes.is_empty());
        assert!(
            volume
                .get_with(|| Ok((endpoint_id("speakers"), f32::NAN)))
                .is_err()
        );
        assert!(
            volume
                .set_with(0.2, |id, level| devices.write(id, level))
                .is_err()
        );
        assert!(devices.writes.is_empty());
    }

    #[test]
    fn volume_keeps_the_captured_device_when_the_default_changes() {
        let mut volume = WindowsVolume::default();
        let mut devices = audio_devices();
        let baseline = volume.get_with(|| devices.read()).unwrap();
        devices.default = "headphones".into();
        volume
            .set_with(0.2, |id, level| devices.write(id, level))
            .unwrap();
        volume
            .set_with(baseline, |id, level| devices.write(id, level))
            .unwrap();
        assert_eq!(
            devices.writes,
            [("speakers".into(), 0.2), ("speakers".into(), 0.8)]
        );
        assert_eq!(devices.levels["headphones"], 0.4);
        assert_eq!(volume.get_with(|| devices.read()).unwrap(), 0.4);
        volume
            .set_with(0.1, |id, level| devices.write(id, level))
            .unwrap();
        assert_eq!(devices.levels["headphones"], 0.1);
        assert_eq!(devices.levels["speakers"], 0.8);
    }

    #[test]
    fn volume_retries_a_disconnected_capture_without_writing_the_new_default() {
        let mut volume = WindowsVolume::default();
        let mut devices = audio_devices();
        let baseline = volume.get_with(|| devices.read()).unwrap();
        devices.default = "headphones".into();
        devices.levels.remove("speakers");
        assert!(
            volume
                .set_with(baseline, |id, level| devices.write(id, level))
                .is_err()
        );
        assert!(devices.writes.is_empty());
        assert!(volume.get_with(|| anyhow::bail!("capture failed")).is_err());
        assert!(
            volume
                .get_with(|| Ok((endpoint_id("headphones"), f32::NAN)))
                .is_err()
        );
        devices.levels.insert("speakers".into(), 0.1);
        volume
            .set_with(baseline, |id, level| devices.write(id, level))
            .unwrap();
        assert_eq!(devices.writes, [("speakers".into(), 0.8)]);
        assert_eq!(devices.levels["headphones"], 0.4);
    }

    #[test]
    fn disconnected_endpoints_keep_their_kind_through_com_calls() {
        for code in [
            HRESULT::from_win32(ERROR_NOT_FOUND.0),
            AUDCLNT_E_DEVICE_INVALIDATED,
        ] {
            assert!(endpoint_error(code.into()).is::<Disconnected>());
        }
        assert!(!endpoint_error(windows::Win32::Foundation::E_FAIL.into()).is::<Disconnected>());
        std::thread::spawn(|| {
            let error = com_call::<()>(|| Err(disconnected_endpoint().context("restoring volume")))
                .unwrap_err();
            assert!(error.is::<Disconnected>());
            assert!(error.to_string().contains("restoring volume"));
            let error = com_call::<()>(|| anyhow::bail!("unrelated")).unwrap_err();
            assert!(!error.is::<Disconnected>());
        })
        .join()
        .unwrap();
    }

    fn ddc_display(index: usize, level: f32, min: u32, max: u32) -> CapturedDisplay<DisplayKey> {
        CapturedDisplay {
            key: DisplayKey::Ddc(index),
            level,
            min,
            max,
        }
    }

    fn ddc_displays() -> DdcDisplays {
        DdcDisplays {
            monitors: PhysicalMonitors(Vec::new()),
            displays: vec![ddc_display(0, 0.8, 10, 110), ddc_display(1, 0.4, 20, 220)],
        }
    }

    fn keys(displays: Vec<CapturedDisplay<DisplayKey>>) -> Vec<DisplayKey> {
        displays.into_iter().map(|display| display.key).collect()
    }

    #[test]
    fn brightness_capture_survives_either_provider_becoming_unavailable() {
        let mut displays = WindowsDisplays::default();
        let wmi = BTreeMap::from([("panel".into(), 0.6)]);
        let panel = DisplayKey::Wmi("panel".into());
        assert_eq!(
            keys(displays.merge(Ok(wmi.clone()), Ok(ddc_displays()))),
            [panel.clone(), DisplayKey::Ddc(0), DisplayKey::Ddc(1)]
        );
        assert!(displays.monitors.is_some());
        assert_eq!(
            keys(displays.merge(Ok(wmi), Err(anyhow::anyhow!("DDC unavailable")))),
            [panel]
        );
        assert!(displays.monitors.is_none());
        assert_eq!(
            keys(displays.merge(Err(anyhow::anyhow!("WMI unavailable")), Ok(ddc_displays()))),
            [DisplayKey::Ddc(0), DisplayKey::Ddc(1)]
        );
        assert!(displays.monitors.is_some());
    }

    #[test]
    fn displays_starting_at_minimum_keep_native_brightness_available() {
        // Startup must not mistake "nothing to fade now" for "unsupported":
        // that would leave the overlay in charge after the user raises brightness.
        let mut displays = ddc_displays();
        for display in &mut displays.displays {
            display.level = 0.0;
        }
        let at_minimum = || Ok(BTreeMap::from([("panel".into(), 0.0)]));
        assert!(brightness_supported(at_minimum(), Ok(displays)));
        assert!(brightness_supported(
            at_minimum(),
            Err(anyhow::anyhow!("no DDC"))
        ));
        let no_ddc = || Err(anyhow::anyhow!("no DDC"));
        assert!(!brightness_supported(
            Err(anyhow::anyhow!("no WMI")),
            no_ddc()
        ));
        assert!(!brightness_supported(Ok(BTreeMap::new()), no_ddc()));
    }

    #[test]
    fn hibernate_requires_s4_and_a_full_hibernation_file() {
        let mut capabilities = SYSTEM_POWER_CAPABILITIES {
            SystemS4: true,
            HiberFilePresent: true,
            HiberFileType: HIBERFILE_TYPE_FULL as u8,
            ..Default::default()
        };
        assert!(hibernation_configured(&capabilities));
        capabilities.SystemS4 = false;
        assert!(!hibernation_configured(&capabilities));
        capabilities.SystemS4 = true;
        capabilities.HiberFilePresent = false;
        assert!(!hibernation_configured(&capabilities));
        capabilities.HiberFilePresent = true;
        for file_type in [HIBERFILE_TYPE_NONE, HIBERFILE_TYPE_REDUCED, 255] {
            capabilities.HiberFileType = file_type as u8;
            assert!(!hibernation_configured(&capabilities));
        }
    }
}
