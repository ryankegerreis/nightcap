//! Windows display brightness, audio endpoint volume, and power backends.

use super::Backends;

pub fn backends() -> Backends {
    // TODO(platform agent): replace with native Windows controls.
    super::fallback::backends()
}
