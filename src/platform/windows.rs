//! Native Windows controls. COM interfaces live only inside individual calls,
//! so constructing a backend on the UI thread and moving it to the engine is safe.

use std::{
    collections::{BTreeMap, BTreeSet},
    ptr,
};

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
            Endpoints::IAudioEndpointVolume, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
            eConsole, eRender,
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
    core::{BOOL, BSTR, PCWSTR, PWSTR, w},
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

fn com_call<T>(call: impl FnOnce() -> Result<T>) -> Result<T> {
    let _apartment = ComApartment::enter()?;
    call().map_err(|error| anyhow::anyhow!("{error:#}"))
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
            Some(id) => enumerator
                .GetDevice(PCWSTR(id.as_ptr()))
                .context("reopening captured render endpoint")?,
            None => enumerator
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .context("finding default render endpoint")?,
        };
        let volume = device
            .Activate(CLSCTX_ALL, None)
            .context("activating audio endpoint volume")?;
        Ok((device, volume))
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

fn set_wmi_monitors<T>(
    baselines: &BTreeMap<String, f32>,
    reference: f32,
    level: f32,
    monitors: impl IntoIterator<Item = Result<(String, T)>>,
    mut write: impl FnMut(T, f32) -> Result<()>,
) -> Result<()> {
    let mut missing: BTreeSet<_> = baselines.keys().cloned().collect();
    let writes = attempt_all(monitors.into_iter().map(|monitor| {
        let (name, monitor) = monitor?;
        if let Some(baseline) = baselines.get(&name) {
            missing.remove(&name);
            write(
                monitor,
                super::scaled_brightness(*baseline, reference, level),
            )
            .with_context(|| format!("setting captured WMI panel {name}"))?;
        }
        Ok(())
    }));
    let missing = if missing.is_empty() {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "captured WMI panels are unavailable: {}",
            missing.into_iter().collect::<Vec<_>>().join(", ")
        ))
    };
    attempt_all([writes, missing])
}

fn attempt_all(attempts: impl IntoIterator<Item = Result<()>>) -> Result<()> {
    let errors: Vec<_> = attempts
        .into_iter()
        .filter_map(|result| result.err().map(|error| format!("{error:#}")))
        .collect();
    ensure!(errors.is_empty(), "{}", errors.join("; "));
    Ok(())
}

fn wmi_set(baselines: &BTreeMap<String, f32>, reference: f32, level: f32) -> Result<()> {
    com_call(|| {
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
        set_wmi_monitors(
            baselines,
            reference,
            level,
            monitors
                .into_iter()
                .map(|monitor| wmi_instance_name(&monitor).map(|name| (name, monitor))),
            |monitor, level| {
                let percent = (level * 100.0).round() as u8;
                let path = property(&monitor, w!("__PATH"))?;
                let mut path_text = [0u16; 4096];
                // SAFETY: buffer sizes and interface lifetimes are valid within this apartment.
                unsafe {
                    VariantToString(&path.0, &mut path_text)
                        .context("converting WMI monitor path")?;
                }
                let end = path_text
                    .iter()
                    .position(|&c| c == 0)
                    .context("unterminated WMI path")?;
                let path =
                    BSTR::from(String::from_utf16(&path_text[..end]).context("decoding WMI path")?);
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
                    let code = VariantToUInt32(&returned.0)
                        .context("reading WmiSetBrightness return value")?;
                    ensure!(code == 0, "WmiSetBrightness returned {code}");
                }
                Ok(())
            },
        )
    })
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

struct DdcBaselines {
    monitors: PhysicalMonitors,
    // Index into the retained handle array, baseline, native min/max.
    levels: Vec<(usize, f32, u32, u32)>,
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
    if matches(read()?) {
        return Ok(());
    }
    std::thread::sleep(std::time::Duration::from_millis(100));
    ensure!(
        matches(read()?),
        "DDC/CI brightness did not reach the requested level {value}"
    );
    Ok(())
}

impl DdcBaselines {
    fn capture() -> Result<Self> {
        let monitors = physical_monitors()?;
        let levels: Vec<_> = monitors
            .0
            .iter()
            .enumerate()
            .filter_map(|(index, monitor)| {
                ddc_range(monitor).ok().map(|(min, current, max)| {
                    (index, (current - min) as f32 / (max - min) as f32, min, max)
                })
            })
            .collect();
        ensure!(!levels.is_empty(), "no monitors support DDC/CI brightness");
        Ok(Self { monitors, levels })
    }

    fn set(&self, reference: f32, level: f32) -> Result<()> {
        self.set_with(reference, level, |index, value| {
            let monitor = &self.monitors.0[index];
            write_ddc(
                value,
                |value| {
                    ensure!(
                        unsafe { SetMonitorBrightness(monitor.hPhysicalMonitor, value) } != 0,
                        "setting DDC/CI brightness: {}",
                        windows::core::Error::from_thread()
                    );
                    Ok(())
                },
                || ddc_range(monitor),
            )
        })
    }

    fn set_with(
        &self,
        reference: f32,
        level: f32,
        mut write: impl FnMut(usize, u32) -> Result<()>,
    ) -> Result<()> {
        attempt_all(self.levels.iter().map(|&(index, baseline, min, max)| {
            let scaled = super::scaled_brightness(baseline, reference, level);
            let value = (f64::from(min) + f64::from(scaled) * f64::from(max - min)).round() as u32;
            write(index, value)
        }))
    }
}

#[derive(Default)]
struct WindowsBrightness {
    wmi_baselines: BTreeMap<String, f32>,
    ddc_baselines: Option<DdcBaselines>,
    reference: f32,
}

impl WindowsBrightness {
    fn capture(
        &mut self,
        wmi: Result<BTreeMap<String, f32>>,
        ddc: Result<DdcBaselines>,
    ) -> Result<f32> {
        self.wmi_baselines = wmi.unwrap_or_default();
        self.ddc_baselines = ddc.ok();
        self.reference = 0.0;
        let levels: Vec<_> = self
            .wmi_baselines
            .values()
            .copied()
            .chain(
                self.ddc_baselines
                    .iter()
                    .flat_map(|d| d.levels.iter().map(|&(_, level, _, _)| level)),
            )
            .collect();
        ensure!(!levels.is_empty(), "no supported brightness monitors");
        self.reference = levels.into_iter().fold(0.0, f32::max);
        Ok(self.reference)
    }

    fn set_with(
        &mut self,
        level: f32,
        wmi: impl FnOnce(&BTreeMap<String, f32>, f32, f32) -> Result<()>,
        ddc: impl FnOnce(&DdcBaselines, f32, f32) -> Result<()>,
    ) -> Result<()> {
        let level = normalized(level)?;
        if self.reference > 0.0 {
            let wmi = (!self.wmi_baselines.is_empty())
                .then(|| wmi(&self.wmi_baselines, self.reference, level));
            let ddc = self
                .ddc_baselines
                .as_ref()
                .map(|baselines| ddc(baselines, self.reference, level));
            attempt_all(wmi.into_iter().chain(ddc))?;
        }
        Ok(())
    }
}

impl Brightness for WindowsBrightness {
    fn get(&mut self) -> Result<f32> {
        self.capture(wmi_levels(), DdcBaselines::capture())
    }

    fn set(&mut self, level: f32) -> Result<()> {
        self.set_with(level, wmi_set, DdcBaselines::set)
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

pub fn backends() -> Backends {
    let mut brightness = WindowsBrightness::default();
    let brightness = brightness
        .get()
        .ok()
        .map(|_| Box::new(brightness) as Box<dyn Brightness>);
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

    fn ddc_baselines() -> DdcBaselines {
        DdcBaselines {
            monitors: PhysicalMonitors(Vec::new()),
            levels: vec![(0, 0.8, 10, 110), (1, 0.4, 20, 220)],
        }
    }

    fn brightness() -> WindowsBrightness {
        WindowsBrightness::default()
    }

    #[test]
    fn brightness_capture_survives_either_provider_becoming_unavailable() {
        let mut brightness = brightness();
        let wmi = BTreeMap::from([("panel".into(), 0.6)]);
        assert_eq!(
            brightness
                .capture(Ok(wmi.clone()), Ok(ddc_baselines()))
                .unwrap(),
            0.8
        );
        assert_eq!(
            brightness
                .capture(Ok(wmi), Err(anyhow::anyhow!("DDC unavailable")))
                .unwrap(),
            0.6
        );
        assert!(brightness.ddc_baselines.is_none());
        assert_eq!(
            brightness
                .capture(Err(anyhow::anyhow!("WMI unavailable")), Ok(ddc_baselines()))
                .unwrap(),
            0.8
        );
        assert!(brightness.wmi_baselines.is_empty());
    }

    #[test]
    fn brightness_failed_capture_does_not_reuse_old_provider_baselines() {
        let mut brightness = brightness();
        brightness
            .capture(
                Ok(BTreeMap::from([("panel".into(), 0.6)])),
                Ok(ddc_baselines()),
            )
            .unwrap();
        assert!(
            brightness
                .capture(
                    Err(anyhow::anyhow!("WMI unavailable")),
                    Err(anyhow::anyhow!("DDC unavailable"))
                )
                .is_err()
        );
        assert!(brightness.wmi_baselines.is_empty());
        assert!(brightness.ddc_baselines.is_none());
        let mut writes = Vec::new();
        brightness
            .set_with(
                0.5,
                |_, _, _| {
                    writes.push("WMI");
                    Ok(())
                },
                |_, _, _| anyhow::bail!("stale DDC write"),
            )
            .unwrap();
        assert!(writes.is_empty());
    }

    #[test]
    fn brightness_restores_ddc_even_when_wmi_restore_fails() {
        let mut brightness = brightness();
        brightness
            .capture(
                Ok(BTreeMap::from([("panel".into(), 0.6)])),
                Ok(ddc_baselines()),
            )
            .unwrap();
        let mut restored = Vec::new();
        assert!(
            brightness
                .set_with(
                    0.8,
                    |_, _, _| anyhow::bail!("WMI disconnected"),
                    |ddc, reference, level| ddc.set_with(reference, level, |index, value| {
                        restored.push((index, value));
                        Ok(())
                    }),
                )
                .is_err()
        );
        assert_eq!(restored, [(0, 90), (1, 100)]);
    }

    #[test]
    fn brightness_skips_providers_without_current_baselines() {
        let mut brightness = brightness();
        brightness
            .capture(Err(anyhow::anyhow!("WMI unavailable")), Ok(ddc_baselines()))
            .unwrap();
        let mut restored = Vec::new();
        brightness
            .set_with(
                0.8,
                |_, _, _| anyhow::bail!("uncaptured WMI provider"),
                |ddc, reference, level| {
                    ddc.set_with(reference, level, |index, value| {
                        restored.push((index, value));
                        Ok(())
                    })
                },
            )
            .unwrap();
        assert_eq!(restored, [(0, 90), (1, 100)]);
    }

    #[test]
    fn ddc_restore_attempts_healthy_monitors_after_a_failure_and_retries() {
        let ddc = ddc_baselines();
        let mut restored = BTreeMap::from([(0, 0), (1, 0), (2, 77)]);
        assert!(
            ddc.set_with(0.8, 0.8, |index, value| {
                ensure!(index != 0, "monitor disconnected");
                restored.insert(index, value);
                Ok(())
            })
            .is_err()
        );
        assert_eq!(restored, BTreeMap::from([(0, 0), (1, 100), (2, 77)]));
        ddc.set_with(0.8, 0.8, |index, value| {
            restored.insert(index, value);
            Ok(())
        })
        .unwrap();
        assert_eq!(restored, BTreeMap::from([(0, 90), (1, 100), (2, 77)]));
    }

    #[test]
    fn wmi_restore_reports_missing_captured_panels_and_restores_present_panels() {
        let baselines = BTreeMap::from([("missing".into(), 0.8), ("present".into(), 0.4)]);
        let mut restored = BTreeMap::from([(1, 0.1), (2, 0.7)]);
        let result = set_wmi_monitors(
            &baselines,
            0.8,
            0.8,
            [Ok(("present".into(), 1)), Ok(("uncaptured".into(), 2))],
            |monitor, level| {
                restored.insert(monitor, level);
                Ok(())
            },
        );
        assert!(result.is_err());
        assert!((restored[&1] - 0.4).abs() < 0.000001);
        assert_eq!(restored[&2], 0.7);
    }

    #[test]
    fn wmi_restore_continues_after_a_monitor_write_or_identity_failure() {
        let baselines = BTreeMap::from([("broken".into(), 0.8), ("healthy".into(), 0.4)]);
        let mut restored = BTreeMap::from([(0, 0.1), (1, 0.1), (2, 0.7)]);
        let result = set_wmi_monitors(
            &baselines,
            0.8,
            0.8,
            [
                Ok(("broken".into(), 0)),
                Err(anyhow::anyhow!("unreadable monitor identity")),
                Ok(("healthy".into(), 1)),
                Ok(("uncaptured".into(), 2)),
            ],
            |monitor, level| {
                ensure!(monitor != 0, "monitor write failed");
                restored.insert(monitor, level);
                Ok(())
            },
        );
        assert!(result.is_err());
        assert_eq!(restored[&0], 0.1);
        assert!((restored[&1] - 0.4).abs() < 0.000001);
        assert_eq!(restored[&2], 0.7);
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
