//! macOS controls. Hibernate deliberately means sleep: macOS has no public,
//! user-level hibernate request. Shutdown may request Automation permission.

use std::{
    ffi::{CStr, c_char, c_void},
    process::Command,
    ptr::{self, NonNull},
};

use anyhow::{Context, Result, ensure};

use super::{Backends, Brightness, Power, Volume};
use crate::settings::EndAction;

type GetBrightness = unsafe extern "C" fn(u32, *mut f32) -> i32;
type SetBrightness = unsafe extern "C" fn(u32, f32) -> i32;

unsafe extern "C" {
    fn dlopen(path: *const c_char, mode: i32) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn dlerror() -> *const c_char;
    fn dlclose(handle: *mut c_void) -> i32;
}

struct DynamicLibrary(NonNull<c_void>);

// SAFETY: Darwin's dynamic-loader handles have no thread affinity. This guard
// has unique ownership, and all symbol calls finish before it is dropped.
unsafe impl Send for DynamicLibrary {}

fn loader_error() -> String {
    // SAFETY: dlerror returns thread-local, NUL-terminated text valid until the
    // next dynamic-loader call; copy it immediately on this calling thread.
    let error = unsafe { dlerror() };
    if error.is_null() {
        "unknown dynamic-loader error".into()
    } else {
        unsafe { CStr::from_ptr(error) }
            .to_string_lossy()
            .into_owned()
    }
}

impl DynamicLibrary {
    fn symbol(&self, name: &CStr) -> Result<*mut c_void> {
        // SAFETY: this guard owns a live dlopen handle; name is NUL-terminated.
        let symbol = unsafe { dlsym(self.0.as_ptr(), name.as_ptr()) };
        ensure!(
            !symbol.is_null(),
            "loading {}: {}",
            name.to_string_lossy(),
            loader_error()
        );
        Ok(symbol)
    }
}

impl Drop for DynamicLibrary {
    fn drop(&mut self) {
        // SAFETY: close our unique handle after the associated pointers are unused.
        let _ = unsafe { dlclose(self.0.as_ptr()) };
    }
}

struct DisplayBrightness {
    // Keep the dlopen handle alive until the function pointers are no longer used.
    _library: DynamicLibrary,
    get: GetBrightness,
    set: SetBrightness,
}

impl DisplayBrightness {
    fn open() -> Result<Self> {
        // SAFETY: the absolute OS framework path and symbol signatures are fixed.
        unsafe {
            // RTLD_LAZY | RTLD_LOCAL: resolve only the symbols needed here.
            let handle = dlopen(
                c"/System/Library/PrivateFrameworks/DisplayServices.framework/DisplayServices"
                    .as_ptr(),
                1 | 4,
            );
            let library = DynamicLibrary(
                NonNull::new(handle)
                    .with_context(|| format!("loading DisplayServices: {}", loader_error()))?,
            );
            let get = std::mem::transmute::<*mut c_void, GetBrightness>(
                library.symbol(c"DisplayServicesGetBrightness")?,
            );
            let set = std::mem::transmute::<*mut c_void, SetBrightness>(
                library.symbol(c"DisplayServicesSetBrightness")?,
            );
            Ok(Self {
                _library: library,
                get,
                set,
            })
        }
    }
}

impl Brightness for DisplayBrightness {
    fn get(&mut self) -> Result<f32> {
        let mut level = 0.0;
        // SAFETY: framework is alive and level is a valid output pointer.
        status(
            unsafe { (self.get)(CGMainDisplayID(), &mut level) },
            "reading display brightness",
        )?;
        normalized(level)
    }

    fn set(&mut self, level: f32) -> Result<()> {
        let level = normalized(level)?;
        // SAFETY: framework is alive and display ID comes from CoreGraphics.
        status(
            unsafe { (self.set)(CGMainDisplayID(), level) },
            "setting display brightness",
        )
    }
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGMainDisplayID() -> u32;
    #[cfg(target_arch = "x86_64")]
    fn CGDisplayIOServicePort(display: u32) -> u32;
}

// The deprecated display service port exists on Intel only, not Apple Silicon.
#[cfg(target_arch = "x86_64")]
mod intel {
    use super::*;
    use core_foundation::{base::TCFType, string::CFString};

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IODisplayGetFloatParameter(
            service: u32,
            options: u32,
            name: *const c_void,
            value: *mut f32,
        ) -> i32;
        fn IODisplaySetFloatParameter(
            service: u32,
            options: u32,
            name: *const c_void,
            value: f32,
        ) -> i32;
    }

    pub struct IoBrightness;

    impl Brightness for IoBrightness {
        fn get(&mut self) -> Result<f32> {
            let key = CFString::new("brightness");
            let mut level = 0.0;
            // SAFETY: the borrowed service port and CFString remain valid during the call.
            status(
                unsafe {
                    IODisplayGetFloatParameter(
                        CGDisplayIOServicePort(CGMainDisplayID()),
                        0,
                        key.as_concrete_TypeRef().cast(),
                        &mut level,
                    )
                },
                "reading IODisplay brightness",
            )?;
            normalized(level)
        }

        fn set(&mut self, level: f32) -> Result<()> {
            let level = normalized(level)?;
            let key = CFString::new("brightness");
            // SAFETY: same service port/key lifetime as get; level is finite.
            status(
                unsafe {
                    IODisplaySetFloatParameter(
                        CGDisplayIOServicePort(CGMainDisplayID()),
                        0,
                        key.as_concrete_TypeRef().cast(),
                        level,
                    )
                },
                "setting IODisplay brightness",
            )
        }
    }
}

#[repr(C)]
struct AudioAddress {
    selector: u32,
    scope: u32,
    element: u32,
}

const OUTPUT_VOLUME: AudioAddress = AudioAddress {
    // VirtualMainVolume and the old VirtualMasterVolume are both 'vmvc'.
    selector: u32::from_be_bytes(*b"vmvc"),
    scope: u32::from_be_bytes(*b"outp"),
    element: 0,
};

#[link(name = "CoreAudio", kind = "framework")]
unsafe extern "C" {
    fn AudioObjectGetPropertyData(
        object: u32,
        address: *const AudioAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        size: *mut u32,
        data: *mut c_void,
    ) -> i32;
    fn AudioObjectSetPropertyData(
        object: u32,
        address: *const AudioAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        size: u32,
        data: *const c_void,
    ) -> i32;
    fn AudioObjectHasProperty(object: u32, address: *const AudioAddress) -> u8;
    fn AudioObjectIsPropertySettable(
        object: u32,
        address: *const AudioAddress,
        settable: *mut u8,
    ) -> i32;
}

// Older HAL implementations expose virtual volume through AudioHardwareService.
#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AudioHardwareServiceHasProperty(object: u32, address: *const AudioAddress) -> u8;
    fn AudioHardwareServiceIsPropertySettable(
        object: u32,
        address: *const AudioAddress,
        settable: *mut u8,
    ) -> i32;
    fn AudioHardwareServiceGetPropertyData(
        object: u32,
        address: *const AudioAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        size: *mut u32,
        data: *mut c_void,
    ) -> i32;
    fn AudioHardwareServiceSetPropertyData(
        object: u32,
        address: *const AudioAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        size: u32,
        data: *const c_void,
    ) -> i32;
}

fn output_device() -> Result<u32> {
    let address = AudioAddress {
        selector: u32::from_be_bytes(*b"dOut"),
        scope: u32::from_be_bytes(*b"glob"),
        element: 0,
    };
    let mut device = 0u32;
    let mut size = size_of::<u32>() as u32;
    // SAFETY: audio system object is 1; output storage matches the property's type.
    status(
        unsafe {
            AudioObjectGetPropertyData(
                1,
                &address,
                0,
                ptr::null(),
                &mut size,
                (&mut device as *mut u32).cast(),
            )
        },
        "finding default audio output",
    )?;
    ensure!(
        size == size_of::<u32>() as u32 && device != 0,
        "no default audio output device"
    );
    Ok(device)
}

#[derive(Debug)]
struct VolumeUnavailable(&'static str);

impl std::fmt::Display for VolumeUnavailable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for VolumeUnavailable {}

// bool indicates the older AudioHardwareService interface is needed.
fn volume_device() -> Result<(u32, bool)> {
    let device = output_device()?;
    let mut settable = 0u8;
    // SAFETY: property address is valid, and Boolean is an unsigned byte on macOS.
    let service = unsafe {
        if AudioObjectHasProperty(device, &OUTPUT_VOLUME) != 0 {
            status(
                AudioObjectIsPropertySettable(device, &OUTPUT_VOLUME, &mut settable),
                "checking audio volume access",
            )?;
            false
        } else {
            if AudioHardwareServiceHasProperty(device, &OUTPUT_VOLUME) == 0 {
                return Err(
                    VolumeUnavailable("output device has no software volume control").into(),
                );
            }
            status(
                AudioHardwareServiceIsPropertySettable(device, &OUTPUT_VOLUME, &mut settable),
                "checking audio service volume access",
            )?;
            true
        }
    };
    if settable == 0 {
        return Err(VolumeUnavailable("output device volume is not settable").into());
    }
    Ok((device, service))
}

struct CoreAudioVolume;

impl Volume for CoreAudioVolume {
    fn get(&mut self) -> Result<f32> {
        let (device, service) = volume_device()?;
        let mut level = 0.0f32;
        let mut size = size_of::<f32>() as u32;
        // SAFETY: output buffer/size match the Float32 virtual volume property.
        let result = unsafe {
            if service {
                AudioHardwareServiceGetPropertyData(
                    device,
                    &OUTPUT_VOLUME,
                    0,
                    ptr::null(),
                    &mut size,
                    (&mut level as *mut f32).cast(),
                )
            } else {
                AudioObjectGetPropertyData(
                    device,
                    &OUTPUT_VOLUME,
                    0,
                    ptr::null(),
                    &mut size,
                    (&mut level as *mut f32).cast(),
                )
            }
        };
        status(result, "reading CoreAudio volume")?;
        ensure!(
            size == size_of::<f32>() as u32,
            "unexpected CoreAudio volume size"
        );
        normalized(level)
    }

    fn set(&mut self, level: f32) -> Result<()> {
        let level = normalized(level)?;
        let (device, service) = volume_device()?;
        // SAFETY: input buffer and size match the Float32 property.
        let result = unsafe {
            if service {
                AudioHardwareServiceSetPropertyData(
                    device,
                    &OUTPUT_VOLUME,
                    0,
                    ptr::null(),
                    size_of::<f32>() as u32,
                    (&level as *const f32).cast(),
                )
            } else {
                AudioObjectSetPropertyData(
                    device,
                    &OUTPUT_VOLUME,
                    0,
                    ptr::null(),
                    size_of::<f32>() as u32,
                    (&level as *const f32).cast(),
                )
            }
        };
        status(result, "setting CoreAudio volume")
    }
}

struct AppleScriptVolume;

impl Volume for AppleScriptVolume {
    fn get(&mut self) -> Result<f32> {
        // Querying a device without software volume yields 'missing value', rejected below.
        parse_volume(&command(
            "osascript",
            &["-e", "output volume of (get volume settings)"],
        )?)
    }

    fn set(&mut self, level: f32) -> Result<()> {
        let percent = (normalized(level)? * 100.0).round() as u8;
        command(
            "osascript",
            &["-e", &format!("set volume output volume {percent}")],
        )?;
        Ok(())
    }
}

fn parse_volume(output: &str) -> Result<f32> {
    let percent: u8 = output
        .trim()
        .parse()
        .context("reading AppleScript volume percentage")?;
    ensure!(percent <= 100, "volume percentage is outside 0..100");
    Ok(f32::from(percent) / 100.0)
}

fn normalized(level: f32) -> Result<f32> {
    ensure!(level.is_finite(), "control level must be finite");
    Ok(level.clamp(0.0, 1.0))
}

fn status(code: i32, operation: &str) -> Result<()> {
    ensure!(
        code == 0,
        "{operation}: OS status {code} (0x{:08x})",
        code as u32
    );
    Ok(())
}

fn command(program: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("running {program}"))?;
    ensure!(
        output.status.success(),
        "{program} failed ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    String::from_utf8(output.stdout).with_context(|| format!("decoding {program} output"))
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPMFindPowerManagement(port: u32) -> u32;
    fn IOPMSleepSystem(connection: u32) -> i32;
    fn IOServiceClose(connection: u32) -> i32;
}

struct MacPower;

impl Power for MacPower {
    fn perform(&mut self, action: EndAction) -> Result<()> {
        if action == EndAction::Shutdown {
            command(
                "osascript",
                &["-e", "tell application \"System Events\" to shut down"],
            )
            .context("requesting shutdown (System Events may require Automation permission)")?;
            return Ok(());
        }
        if action == EndAction::Hibernate {
            log::info!("macOS has no user-level hibernate request; requesting sleep instead");
        }
        // SAFETY: IOPM creates a connection, which is closed exactly once after use.
        let native = unsafe {
            let connection = IOPMFindPowerManagement(0);
            if connection == 0 {
                Err(anyhow::anyhow!(
                    "IOPMFindPowerManagement returned no connection"
                ))
            } else {
                let result = status(IOPMSleepSystem(connection), "requesting IOKit sleep");
                let _ = IOServiceClose(connection);
                result
            }
        };
        if let Err(error) = native {
            log::debug!("IOKit sleep unavailable: {error:#}; trying pmset");
            command("pmset", &["sleepnow"])
                .with_context(|| format!("IOKit sleep failed: {error:#}; pmset fallback"))?;
        }
        Ok(())
    }
}

pub fn backends() -> Backends {
    let mut brightness: Option<Box<dyn Brightness>> = None;
    match DisplayBrightness::open() {
        Ok(mut backend) => match backend.get() {
            Ok(_) => brightness = Some(Box::new(backend)),
            Err(error) => log::debug!("DisplayServices brightness unavailable: {error:#}"),
        },
        Err(error) => log::debug!("DisplayServices unavailable: {error:#}"),
    }
    #[cfg(target_arch = "x86_64")]
    if brightness.is_none() {
        let mut backend = intel::IoBrightness;
        if backend.get().is_ok() {
            brightness = Some(Box::new(backend));
        }
    }
    let mut native = CoreAudioVolume;
    let volume: Option<Box<dyn Volume>> = match native.get() {
        Ok(_) => Some(Box::new(native)),
        Err(error) => {
            log::debug!("CoreAudio volume unavailable: {error:#}");
            // A known read-only device must not be falsely advertised via AppleScript.
            if error.is::<VolumeUnavailable>() {
                None
            } else {
                let mut fallback = AppleScriptVolume;
                fallback
                    .get()
                    .ok()
                    .map(|_| Box::new(fallback) as Box<dyn Volume>)
            }
        }
    };
    Backends {
        brightness,
        volume,
        power: Box::new(MacPower),
        hibernate_supported: false,
    }
}

/// Owns the workspace block observer and unregisters it before releasing its token.
/// The callback only queues a command; hardware restoration stays on the engine.
pub struct WakeObserver {
    center: objc2::rc::Retained<objc2_foundation::NSNotificationCenter>,
    token:
        objc2::rc::Retained<objc2::runtime::ProtocolObject<dyn objc2_foundation::NSObjectProtocol>>,
}

impl WakeObserver {
    pub fn new(engine: crate::engine::EngineHandle) -> Self {
        use block2::RcBlock;
        use objc2_app_kit::{NSWorkspace, NSWorkspaceDidWakeNotification};
        let center = NSWorkspace::sharedWorkspace().notificationCenter();
        let block = RcBlock::new(move |_| {
            let _ = engine.send(crate::engine::Command::SystemWoke);
        });
        // SAFETY: no object filter or operation queue, and the captured engine
        // handle is Send + Sync. NotificationCenter copies and retains the block.
        let token = unsafe {
            center.addObserverForName_object_queue_usingBlock(
                Some(NSWorkspaceDidWakeNotification),
                None,
                None,
                &block,
            )
        };
        Self { center, token }
    }
}

impl Drop for WakeObserver {
    fn drop(&mut self) {
        // SAFETY: this exact live token came from this center's block registration.
        unsafe {
            self.center.removeObserver(self.token.as_ref());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "reads real macOS hardware; sets only the existing volume; never requests power actions"]
    fn platform_probe() -> Result<()> {
        let mut native = CoreAudioVolume;
        println!("CoreAudio volume: {:?}", native.get());
        let mut script = AppleScriptVolume;
        println!("AppleScript volume: {:?}", script.get());
        let mut controls = backends();
        match controls.brightness.as_mut() {
            Some(brightness) => println!("brightness: {:.6}", brightness.get()?),
            None => println!("brightness: unavailable (overlay)"),
        }
        match controls.volume.as_mut() {
            Some(volume) => {
                let current = volume.get()?;
                println!("volume before: {current:.6}");
                volume
                    .set(current)
                    .context("setting volume to its existing value")?;
                let after = volume.get()?;
                println!("volume after: {after:.6}");
                ensure!(
                    (current - after).abs() < 0.011,
                    "volume changed during probe"
                );
            }
            None => println!("volume: unavailable"),
        }
        Ok(())
    }

    #[test]
    fn applescript_volume_requires_a_valid_percentage() {
        assert_eq!(parse_volume("42\n").unwrap(), 0.42);
        for output in ["missing value", "", "101", "-1", "NaN"] {
            assert!(parse_volume(output).is_err(), "{output}");
        }
    }
}
