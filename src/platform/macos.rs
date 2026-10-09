//! macOS brightness, CoreAudio volume, and power backends.

use super::Backends;

pub fn backends() -> Backends {
    // TODO(platform agent): replace with native macOS controls.
    super::fallback::backends()
}
