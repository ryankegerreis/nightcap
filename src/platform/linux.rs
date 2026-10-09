//! Linux backlight, audio service, and system power backends.

use super::Backends;

pub fn backends() -> Backends {
    // TODO(platform agent): replace with Linux controls.
    super::fallback::backends()
}
