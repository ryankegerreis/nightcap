//! Native dimming for composited X11 desktops.

use anyhow::{Context, Result, ensure};
use x11rb::{
    connection::Connection,
    protocol::{
        randr::{ConnectionExt as _, MonitorInfo},
        render::{ConnectionExt as _, PictType},
        shape::{ConnectionExt as _, SK, SO},
        xproto::{
            Atom, AtomEnum, ChangeWindowAttributesAux, ClipOrdering, ColormapAlloc,
            ConfigureWindowAux, ConnectionExt as _, CreateWindowAux, PropMode, Rectangle,
            StackMode, Visualid, Window, WindowClass,
        },
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};

pub(super) struct Layer {
    connection: RustConnection,
    root: Window,
    compositor: Atom,
    window: Window,
    alpha_shift: u16,
    alpha_mask: u16,
    visible: bool,
    pixel: u32,
    bounds: Option<Rectangle>,
}

impl Layer {
    pub(super) fn new(anchor: Window) -> Result<Self> {
        let (connection, _) = x11rb::connect(None).context("opening X11 display")?;
        let root = connection.get_geometry(anchor)?.reply()?.root;
        let screen = connection
            .setup()
            .roots
            .iter()
            .position(|screen| screen.root == root)
            .context("finding the Nightcap X11 screen")?;
        let shape = connection.shape_query_version()?.reply()?;
        ensure!(
            (shape.major_version, shape.minor_version) >= (1, 1),
            "X11 input shaping is unavailable"
        );
        let randr = connection.randr_query_version(1, 5)?.reply()?;
        ensure!(
            (randr.major_version, randr.minor_version) >= (1, 5),
            "X11 monitor geometry is unavailable"
        );
        let compositor = connection
            .intern_atom(false, format!("_NET_WM_CM_S{screen}").as_bytes())?
            .reply()?
            .atom;
        let (visual, alpha_shift, alpha_mask) = alpha_visual(&connection, screen)?;
        let colormap = connection.generate_id()?;
        connection
            .create_colormap(ColormapAlloc::NONE, colormap, root, visual)?
            .check()?;
        let window = connection.generate_id()?;
        connection
            .create_window(
                32,
                window,
                root,
                0,
                0,
                1,
                1,
                0,
                WindowClass::INPUT_OUTPUT,
                visual,
                &CreateWindowAux::new()
                    .colormap(colormap)
                    .border_pixel(0)
                    .background_pixel(0)
                    .override_redirect(1),
            )?
            .check()?;
        connection
            .change_property8(
                PropMode::REPLACE,
                window,
                AtomEnum::WM_NAME,
                AtomEnum::STRING,
                b"Nightcap Dimming Overlay",
            )?
            .check()?;
        // Apply and verify the input shape before any request can map the window.
        connection
            .shape_rectangles(
                SO::SET,
                SK::INPUT,
                ClipOrdering::UNSORTED,
                window,
                0,
                0,
                &[],
            )?
            .check()?;
        ensure!(
            connection
                .shape_get_rectangles(window, SK::INPUT)?
                .reply()?
                .rectangles
                .is_empty(),
            "X11 did not accept an empty overlay input region"
        );
        Ok(Self {
            connection,
            root,
            compositor,
            window,
            alpha_shift,
            alpha_mask,
            visible: false,
            pixel: 0,
            bounds: None,
        })
    }

    pub(super) fn apply(&mut self, anchor: Window, alpha: f32) -> Result<bool> {
        let composited = self
            .connection
            .get_selection_owner(self.compositor)?
            .reply()?
            .owner
            != x11rb::NONE;
        if !composited || alpha == 0.0 {
            self.hide()?;
            return Ok(composited);
        }
        let bounds = self.monitor_bounds(anchor)?;
        self.apply_to_monitor(bounds, alpha)
    }

    fn hide(&mut self) -> Result<()> {
        if self.visible {
            self.connection.unmap_window(self.window)?.check()?;
            self.visible = false;
        }
        Ok(())
    }

    fn apply_to_monitor(&mut self, bounds: Option<Rectangle>, alpha: f32) -> Result<bool> {
        let Some(bounds) = bounds else {
            self.hide()?;
            return Ok(false);
        };
        if self.bounds.is_none_or(|previous| {
            (previous.x, previous.y, previous.width, previous.height)
                != (bounds.x, bounds.y, bounds.width, bounds.height)
        }) {
            self.connection
                .configure_window(
                    self.window,
                    &ConfigureWindowAux::new()
                        .x(i32::from(bounds.x))
                        .y(i32::from(bounds.y))
                        .width(u32::from(bounds.width))
                        .height(u32::from(bounds.height)),
                )?
                .check()?;
            self.bounds = Some(bounds);
        }
        let pixel = ((alpha * f32::from(self.alpha_mask)).round() as u32) << self.alpha_shift;
        if pixel != self.pixel {
            self.connection
                .change_window_attributes(
                    self.window,
                    &ChangeWindowAttributesAux::new().background_pixel(pixel),
                )?
                .check()?;
            self.connection
                .clear_area(false, self.window, 0, 0, 0, 0)?
                .check()?;
            self.pixel = pixel;
        }
        if !self.visible {
            self.connection.map_window(self.window)?.check()?;
            self.visible = true;
        }
        self.connection
            .configure_window(
                self.window,
                &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
            )?
            .check()?;
        Ok(true)
    }

    fn monitor_bounds(&self, anchor: Window) -> Result<Option<Rectangle>> {
        let geometry = self.connection.get_geometry(anchor)?.reply()?;
        let origin = self
            .connection
            .translate_coordinates(anchor, self.root, 0, 0)?
            .reply()?;
        let center = (
            i32::from(origin.dst_x) + i32::from(geometry.width) / 2,
            i32::from(origin.dst_y) + i32::from(geometry.height) / 2,
        );
        let monitors = self
            .connection
            .randr_get_monitors(self.root, true)?
            .reply()?
            .monitors;
        Ok(monitor_at(&monitors, center))
    }
}

impl Drop for Layer {
    fn drop(&mut self) {
        if let Ok(cookie) = self.connection.destroy_window(self.window) {
            let _ = cookie.check();
        }
    }
}

fn alpha_visual(connection: &RustConnection, screen: usize) -> Result<(Visualid, u16, u16)> {
    let reply = connection.render_query_pict_formats()?.reply()?;
    let picture = reply
        .screens
        .get(screen)
        .context("missing X Render screen")?;
    picture
        .depths
        .iter()
        .filter(|depth| depth.depth == 32)
        .flat_map(|depth| &depth.visuals)
        .find_map(|visual| {
            let format = reply.formats.iter().find(|format| {
                format.id == visual.format
                    && format.type_ == PictType::DIRECT
                    && format.direct.alpha_mask != 0
            })?;
            Some((
                visual.visual,
                format.direct.alpha_shift,
                format.direct.alpha_mask,
            ))
        })
        .context("no X11 visual supports alpha")
}

fn monitor_at(monitors: &[MonitorInfo], (x, y): (i32, i32)) -> Option<Rectangle> {
    monitors
        .iter()
        .filter(|monitor| monitor.width > 0 && monitor.height > 0)
        .min_by_key(|monitor| {
            let left = i32::from(monitor.x);
            let top = i32::from(monitor.y);
            let dx = i64::from(x - x.clamp(left, left + i32::from(monitor.width) - 1));
            let dy = i64::from(y - y.clamp(top, top + i32::from(monitor.height) - 1));
            (dx * dx + dy * dy, !monitor.primary)
        })
        .map(|monitor| Rectangle {
            x: monitor.x,
            y: monitor.y,
            width: monitor.width,
            height: monitor.height,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use x11rb::protocol::{
        Event,
        xproto::{
            BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, EventMask, ImageFormat, ImageOrder, MapState,
        },
        xtest::ConnectionExt as _,
    };

    #[test]
    fn overlay_uses_the_monitor_containing_the_anchor_or_the_nearest_one() {
        let monitors = [
            MonitorInfo {
                x: -100,
                width: 100,
                height: 100,
                ..Default::default()
            },
            MonitorInfo {
                width: 100,
                height: 100,
                primary: true,
                ..Default::default()
            },
        ];
        assert_eq!(monitor_at(&monitors, (-10, 20)).unwrap().x, -100);
        assert_eq!(monitor_at(&monitors, (200, 20)).unwrap().x, 0);
        assert!(monitor_at(&[], (0, 0)).is_none());
    }

    #[test]
    #[ignore = "requires an isolated X11 display and synthesizes clicks"]
    fn native_overlay_checks_input_alpha_and_cleanup() -> Result<()> {
        let composited = std::env::var("NIGHTCAP_X11_TEST_COMPOSITED")
            .context("set NIGHTCAP_X11_TEST_COMPOSITED to 0 or 1 on an isolated X11 display")?
            == "1";
        let (connection, screen) = x11rb::connect(None)?;
        let screen = &connection.setup().roots[screen];
        let target = |x| -> Result<Window> {
            let window = connection.generate_id()?;
            connection
                .create_window(
                    screen.root_depth,
                    window,
                    screen.root,
                    x,
                    50,
                    60,
                    60,
                    0,
                    WindowClass::INPUT_OUTPUT,
                    screen.root_visual,
                    &CreateWindowAux::new()
                        .override_redirect(1)
                        .event_mask(EventMask::BUTTON_PRESS),
                )?
                .check()?;
            connection.map_window(window)?.check()?;
            Ok(window)
        };
        let first = target(50)?;
        let second = target(150)?;
        let mut layer = Layer::new(first)?;
        let window = layer.window;
        assert_eq!(layer.apply(first, 0.5)?, composited);
        let attributes = connection.get_window_attributes(window)?.reply()?;
        assert!(attributes.override_redirect);
        assert_eq!(connection.query_tree(window)?.reply()?.parent, screen.root);
        assert_eq!(
            attributes.map_state,
            if composited {
                MapState::VIEWABLE
            } else {
                MapState::UNMAPPED
            }
        );
        assert!(
            connection
                .shape_get_rectangles(window, SK::INPUT)?
                .reply()?
                .rectangles
                .is_empty()
        );

        if composited {
            let geometry = connection.get_geometry(window)?.reply()?;
            let children = connection.query_tree(screen.root)?.reply()?.children;
            let stack = |id| children.iter().position(|child| *child == id).unwrap();
            for (x, target) in [(70, first), (170, second)] {
                assert!(
                    (i32::from(geometry.x)..i32::from(geometry.x) + i32::from(geometry.width))
                        .contains(&x)
                );
                assert!(
                    (i32::from(geometry.y)..i32::from(geometry.y) + i32::from(geometry.height))
                        .contains(&70)
                );
                assert!(stack(window) > stack(target));
            }
            let image = connection
                .get_image(ImageFormat::Z_PIXMAP, window, 0, 0, 1, 1, u32::MAX)?
                .reply()?;
            let bytes = image.data[..4].try_into()?;
            let pixel = if connection.setup().image_byte_order == ImageOrder::LSB_FIRST {
                u32::from_le_bytes(bytes)
            } else {
                u32::from_be_bytes(bytes)
            };
            assert_eq!(pixel, layer.pixel);
            assert_ne!(pixel, 0);
        }
        for (x, window) in [(70, first), (170, second)] {
            connection
                .warp_pointer(x11rb::NONE, screen.root, 0, 0, 0, 0, x, 70)?
                .check()?;
            for event in [BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT] {
                connection
                    .xtest_fake_input(event, 1, 0, screen.root, 0, 0, 0)?
                    .check()?;
            }
            assert!(
                matches!(connection.poll_for_event()?, Some(Event::ButtonPress(click)) if click.event == window)
            );
        }
        if composited {
            // Simulate RandR temporarily returning no active monitors.
            assert!(!layer.apply_to_monitor(None, 0.5)?);
            assert_eq!(
                connection.get_window_attributes(window)?.reply()?.map_state,
                MapState::UNMAPPED
            );
            assert!(layer.apply(first, 0.5)?);
            assert_eq!(
                connection.get_window_attributes(window)?.reply()?.map_state,
                MapState::VIEWABLE
            );
        }
        layer.apply(first, 0.0)?;
        assert_eq!(
            connection.get_window_attributes(window)?.reply()?.map_state,
            MapState::UNMAPPED
        );
        drop(layer);
        assert!(connection.get_window_attributes(window)?.reply().is_err());
        Ok(())
    }
}
