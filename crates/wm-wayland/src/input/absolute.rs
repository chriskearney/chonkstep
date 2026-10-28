//! Where a touchscreen's or a drawing tablet's position lands.
//!
//! libinput reports an absolute device's position as a fraction of its
//! own surface: the panel a touchscreen is built into, or a tablet's
//! drawing area. A touchscreen's surface is one output, so its position
//! is placed on that output's rectangle in global space, turned through
//! the output's transform. Scaled to the union of every monitor instead,
//! as it once was, a tap on a laptop's own panel landed on the external
//! display beside it whenever one was connected. A tablet spans the union
//! unless it is bound to one output, as in Hyprland, so a drawing tablet
//! on a two-monitor desk reaches both; a pen built into a screen is bound
//! by `input:tablet:output` or udev.
//!
//! The output is the one the configuration names
//! (`input:touchdevice:output`, `input:tablet:output`), else the one udev
//! ties the device to (`WL_OUTPUT`, which libinput reports as the
//! device's output name). A name no connected output carries falls
//! through to the next rule. With neither, the device class decides
//! ([`Unbound`]), as Hyprland's defaults do: a touchscreen covers the
//! first built-in panel, else the primary, and a tablet spans the whole
//! layout. The area is resolved per event, so a hotplug or a rotation
//! applies from the next event on and no rectangle outlives its output.
//!
//! The virtual pointer places its positions through the same [`Area`].
//! Its coordinates are already in the output's logical orientation, so
//! its area is never turned.

use smithay::utils::{Logical, Point as LogicalPoint, Size as LogicalSize, Transform};
use wm_theme_api::{Point, Rect, Size};

use crate::state::{Compositor, OutputEntry};

/// The part of global space an absolute device's whole range covers,
/// and the turn from the device's axes onto it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Area {
    /// In global compositor space, the space `hit_at` and the monitor
    /// list are measured in.
    pub rect: Rect,
    /// A digitiser is fixed to its panel's scanout axes, so this is the
    /// output's transform: the renderer shows framebuffer pixel `P` the
    /// scene at `transform.transform_point_in(P, framebuffer)`, and a
    /// touch at `P` must land on what is drawn there.
    pub transform: Transform,
}

impl Area {
    /// The whole of one output.
    pub(crate) fn of_output(entry: &OutputEntry) -> Self {
        Self { rect: Rect::new(entry.position, entry.size), transform: entry.transform }
    }

    /// The union of every output, measured from the origin and never
    /// turned: what a tablet bound to no output spans. With one output
    /// at the origin, that output.
    pub(crate) fn desktop(size: Size) -> Self {
        Self { rect: Rect::new(Point::new(0, 0), size), transform: Transform::Normal }
    }

    /// The device's own extent: `rect`'s size before the turn, which for
    /// an output is its framebuffer's.
    pub(crate) fn device_size(&self) -> LogicalSize<i32, Logical> {
        self.transform.transform_size((self.rect.size.w as i32, self.rect.size.h as i32).into())
    }

    /// Where `at`, measured in [`Self::device_size`] from the device's own
    /// top-left corner, lands in global space.
    pub(crate) fn place(&self, at: LogicalPoint<f64, Logical>) -> LogicalPoint<f64, Logical> {
        let turned = self.transform.transform_point_in(at, &self.device_size().to_f64());
        (f64::from(self.rect.pos.x) + turned.x, f64::from(self.rect.pos.y) + turned.y).into()
    }
}

/// What an absolute device covers when neither the configuration nor
/// udev names a connected output for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unbound {
    /// A touchscreen: the first built-in panel, the one it is most likely
    /// built into, else the primary. Hyprland's `touchdevice:output`
    /// default, `[[Auto]]`.
    Panel,
    /// A tablet: the whole layout, as Hyprland's empty `tablet:output`
    /// spans every monitor, so a drawing tablet beside two monitors
    /// reaches both.
    Desktop,
}

/// The output an absolute device covers, as an index into the layout's
/// `names`: the `configured` one, else the `associated` one, else what
/// `unbound` says. `None` means the whole layout, which is also the
/// answer with no outputs at all.
pub(crate) fn mapped_output<'a>(
    names: impl Iterator<Item = &'a str> + Clone,
    configured: Option<&str>,
    associated: Option<&str>,
    unbound: Unbound,
) -> Option<usize> {
    let named = |wanted: Option<&str>| wanted.and_then(|wanted| names.clone().position(|name| name == wanted));
    named(configured).or_else(|| named(associated)).or_else(|| match unbound {
        Unbound::Panel => {
            names.clone().position(crate::lid_bus::is_internal_connector).or_else(|| names.clone().next().map(|_| 0))
        }
        Unbound::Desktop => None,
    })
}

/// The area an absolute device covers on the desk as it is now: one
/// output, or the union of them all, which is empty with no outputs.
pub(crate) fn area(comp: &Compositor, configured: Option<&str>, associated: Option<&str>, unbound: Unbound) -> Area {
    let backend = comp.wm.backend();
    // `monitors` lists the outputs in `Compositor::outputs` order, so an
    // index into one selects the same output in the other.
    let names = backend.monitors.iter().map(|monitor| monitor.name.as_str());
    mapped_output(names, configured, associated, unbound)
        .and_then(|index| comp.outputs.get(index))
        .map_or_else(|| Area::desktop(backend.output_size), Area::of_output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(x: i32, y: i32, w: u32, h: u32, transform: Transform) -> Area {
        Area { rect: Rect::new(Point::new(x, y), Size::new(w, h)), transform }
    }

    /// Where a touch at the fractions `(u, v)` of the digitiser lands, the
    /// way the input handlers ask libinput for it.
    fn touch(area: &Area, u: f64, v: f64) -> (f64, f64) {
        let device = area.device_size().to_f64();
        let at = area.place((u * device.w, v * device.h).into());
        (at.x, at.y)
    }

    /// A 1920x1200 laptop panel at the origin with a 2560x1440 monitor
    /// beside it: the union is 4480x1440, and its centre is on the monitor.
    #[test]
    fn a_tap_at_the_centre_lands_at_the_centre_of_its_own_output_not_of_the_union() {
        let panel = output(0, 0, 1920, 1200, Transform::Normal);
        let monitor = output(1920, 0, 2560, 1440, Transform::Normal);
        assert_eq!(touch(&panel, 0.5, 0.5), (960.0, 600.0));
        assert_eq!(touch(&monitor, 0.5, 0.5), (3200.0, 720.0));
        assert_eq!(touch(&Area::desktop(Size::new(4480, 1440)), 0.5, 0.5), (2240.0, 720.0));
        assert_eq!(touch(&monitor, 0.0, 0.0), (1920.0, 0.0), "the origin is the output's own corner");
    }

    /// The renderer shows framebuffer pixel `P` the scene at
    /// `transform.transform_point_in(P, framebuffer)`, which the GLES
    /// pixel tests in `rounded.rs` and `frame_effects.rs` read back. A
    /// 1920x1200 panel turned a quarter is a 1200x1920 output, here at
    /// (1920, 0), and the corner its scanout starts at shows the
    /// desktop's top-right for `_90` and its bottom-left for `_270`.
    #[test]
    fn a_quarter_turned_output_turns_the_touch_with_it() {
        let turned = output(1920, 0, 1200, 1920, Transform::_90);
        assert_eq!(turned.device_size(), (1920, 1200).into(), "the digitiser keeps the panel's own axes");
        assert_eq!(touch(&turned, 0.0, 0.0), (3120.0, 0.0));
        assert_eq!(touch(&turned, 1.0, 0.0), (3120.0, 1920.0));
        assert_eq!(touch(&turned, 0.0, 1.0), (1920.0, 0.0));
        assert_eq!(touch(&turned, 0.25, 0.75), (1920.0 + 300.0, 480.0));
        assert_eq!(touch(&turned, 0.5, 0.5), (2520.0, 960.0));

        let turned = output(1920, 0, 1200, 1920, Transform::_270);
        assert_eq!(touch(&turned, 0.0, 0.0), (1920.0, 1920.0));
        assert_eq!(touch(&turned, 1.0, 0.0), (1920.0, 0.0));
        assert_eq!(touch(&turned, 0.0, 1.0), (3120.0, 1920.0));
        assert_eq!(touch(&turned, 0.25, 0.75), (1920.0 + 900.0, 1440.0));
        assert_eq!(touch(&turned, 0.5, 0.5), (2520.0, 960.0));
    }

    /// Whatever the transform, the digitiser's whole range covers exactly
    /// its output: its four corners land on the output's four corners.
    #[test]
    fn every_transform_maps_the_digitiser_onto_exactly_its_output() {
        use Transform::*;
        for transform in [Normal, _90, _180, _270, Flipped, Flipped90, Flipped180, Flipped270] {
            let size = transform.transform_size(LogicalSize::<i32, Logical>::from((1920, 1200)));
            let rotated = output(-640, 120, size.w as u32, size.h as u32, transform);
            let mut corners: Vec<(f64, f64)> =
                [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)].iter().map(|&(u, v)| touch(&rotated, u, v)).collect();
            corners.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let (left, top) = (-640.0, 120.0);
            let (right, bottom) = (left + f64::from(size.w), top + f64::from(size.h));
            assert_eq!(corners, [(left, top), (left, bottom), (right, top), (right, bottom)], "{transform:?}");
            assert_eq!(touch(&rotated, 0.5, 0.5), ((left + right) / 2.0, (top + bottom) / 2.0), "{transform:?}");
        }
    }

    #[test]
    fn a_touchscreen_covers_its_named_output_else_the_built_in_panel_else_the_primary() {
        let docked = ["DP-1", "eDP-1"];
        let names = || docked.iter().copied();
        let panel = Unbound::Panel;
        assert_eq!(mapped_output(names(), None, None, panel), Some(1), "the built-in panel, though not primary");
        assert_eq!(mapped_output(names(), None, Some("DP-1"), panel), Some(0), "udev's association");
        assert_eq!(mapped_output(names(), Some("DP-1"), None, panel), Some(0), "the configuration");
        assert_eq!(mapped_output(names(), Some("eDP-1"), Some("DP-1"), panel), Some(1), "the configuration over udev");
        assert_eq!(
            mapped_output(names(), Some("HDMI-A-1"), Some("DP-2"), panel),
            Some(1),
            "names that are not connected fall through"
        );

        let desk = ["DP-1", "HDMI-A-1"];
        assert_eq!(mapped_output(desk.iter().copied(), None, None, panel), Some(0), "no panel: the primary");
        let two_panels = ["DP-1", "DSI-1", "eDP-1"];
        assert_eq!(mapped_output(two_panels.iter().copied(), None, None, panel), Some(1), "the first panel");
        assert_eq!(mapped_output(["LVDS-1"].iter().copied(), None, None, panel), Some(0));
        assert_eq!(mapped_output([].iter().copied(), Some("eDP-1"), None, panel), None, "no outputs at all");
    }

    /// Hyprland's empty `tablet:output` spans every monitor, so a tablet
    /// bound to nothing covers the whole layout, built-in panel or not.
    #[test]
    fn a_tablet_covers_its_named_output_else_the_whole_layout() {
        let docked = ["DP-1", "eDP-1"];
        let names = || docked.iter().copied();
        let tablet = Unbound::Desktop;
        assert_eq!(mapped_output(names(), None, None, tablet), None, "the whole layout, not the panel");
        assert_eq!(mapped_output(names(), None, Some("eDP-1"), tablet), Some(1), "udev's association");
        assert_eq!(mapped_output(names(), Some("DP-1"), Some("eDP-1"), tablet), Some(0), "the configuration over udev");
        assert_eq!(
            mapped_output(names(), Some("HDMI-A-1"), Some("DP-2"), tablet),
            None,
            "names that are not connected fall through to the whole layout"
        );

        // A 1920x1200 panel with a 2560x1440 monitor beside it: the tablet's
        // far corner reaches the monitor's far edge. With one output at the
        // origin, the layout is that output.
        let layout = Area::desktop(Size::new(4480, 1440));
        assert_eq!(touch(&layout, 1.0, 1.0), (4480.0, 1440.0));
        assert_eq!(touch(&layout, 0.5, 0.5), (2240.0, 720.0));
        let alone = Size::new(1920, 1200);
        assert_eq!(Area::desktop(alone), output(0, 0, 1920, 1200, Transform::Normal));
    }
}
