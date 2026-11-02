//! The sizes a Browser window can show a page at: the window's own size
//! (Responsive), or a device's viewport, in CSS pixels as the device
//! reports them (`window.innerWidth` × `innerHeight` in its browser, at
//! full screen).

use crate::ui::assets::IconName;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Phone,
    Tablet,
    Computer,
}

impl Kind {
    pub fn icon(self) -> IconName {
        match self {
            Kind::Phone => IconName::Smartphone,
            Kind::Tablet => IconName::Tablet,
            Kind::Computer => IconName::Monitor,
        }
    }
}

/// A device to show pages at.
#[derive(Debug, PartialEq)]
pub struct Device {
    /// Saved in the project, so it stays the same when names change.
    pub id: &'static str,
    pub name: &'static str,
    pub kind: Kind,
    /// Portrait for phones and tablets.
    pub width: u32,
    pub height: u32,
    /// What the device's browser says it is, so sites send their mobile
    /// pages. `None`: this Mac's Safari.
    pub user_agent: Option<&'static str>,
}

const IPHONE: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 18_5 like Mac OS X) AppleWebKit/605.1.15 \
                      (KHTML, like Gecko) Version/18.5 Mobile/15E148 Safari/604.1";
const IPAD: &str = "Mozilla/5.0 (iPad; CPU OS 18_5 like Mac OS X) AppleWebKit/605.1.15 \
                    (KHTML, like Gecko) Version/18.5 Mobile/15E148 Safari/604.1";
const ANDROID_PIXEL: &str = "Mozilla/5.0 (Linux; Android 15; Pixel 9) AppleWebKit/537.36 \
                             (KHTML, like Gecko) Chrome/137.0.0.0 Mobile Safari/537.36";
const ANDROID_GALAXY: &str = "Mozilla/5.0 (Linux; Android 14; SM-S921B) AppleWebKit/537.36 \
                              (KHTML, like Gecko) Chrome/137.0.0.0 Mobile Safari/537.36";

const fn device(
    id: &'static str,
    name: &'static str,
    kind: Kind,
    width: u32,
    height: u32,
    user_agent: Option<&'static str>,
) -> Device {
    Device { id, name, kind, width, height, user_agent }
}

/// In menu order, grouped by kind.
pub const DEVICES: &[Device] = &[
    device("iphone-se", "iPhone SE", Kind::Phone, 375, 667, Some(IPHONE)),
    device("iphone-16", "iPhone 16", Kind::Phone, 393, 852, Some(IPHONE)),
    device("iphone-16-plus", "iPhone 16 Plus", Kind::Phone, 430, 932, Some(IPHONE)),
    device("iphone-16-pro", "iPhone 16 Pro", Kind::Phone, 402, 874, Some(IPHONE)),
    device("iphone-16-pro-max", "iPhone 16 Pro Max", Kind::Phone, 440, 956, Some(IPHONE)),
    device("pixel-9", "Pixel 9", Kind::Phone, 412, 923, Some(ANDROID_PIXEL)),
    device("galaxy-s24", "Galaxy S24", Kind::Phone, 360, 780, Some(ANDROID_GALAXY)),
    device("ipad-mini", "iPad mini", Kind::Tablet, 744, 1133, Some(IPAD)),
    device("ipad-air-11", "iPad Air 11″", Kind::Tablet, 820, 1180, Some(IPAD)),
    device("ipad-pro-13", "iPad Pro 13″", Kind::Tablet, 1032, 1376, Some(IPAD)),
    device("laptop", "Laptop", Kind::Computer, 1280, 800, None),
    device("desktop", "Desktop", Kind::Computer, 1440, 900, None),
    device("full-hd", "Full HD", Kind::Computer, 1920, 1080, None),
];

/// The device each quick button picks.
pub const QUICK: [(&str, Kind); 3] = [
    ("iphone-16-pro", Kind::Phone),
    ("ipad-air-11", Kind::Tablet),
    ("desktop", Kind::Computer),
];

pub fn find(id: &str) -> Option<&'static Device> {
    DEVICES.iter().find(|device| device.id == id)
}

/// The page's size in CSS px on `device` (turned on its side if
/// `landscape`), or `None` to follow the window: Responsive, or a device
/// this version doesn't know (saved by a newer one).
pub fn fixed_size(device: Option<&str>, landscape: bool) -> Option<(f32, f32)> {
    let device = find(device?)?;
    let (width, height) = (device.width as f32, device.height as f32);
    Some(if landscape { (height, width) } else { (width, height) })
}

/// Room left around a device's screen, in layout px.
pub const MARGIN: f32 = 16.;

/// Where the page sits in the window's page area, and at what scale.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Layout {
    /// The page's rectangle, relative to the area's top left.
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    /// px per CSS px: the page zoom (and the canvas zoom, while windows
    /// are laid out zoomed).
    pub zoom: f32,
    /// The page's size in CSS px, as the page sees it.
    pub css_width: f32,
    pub css_height: f32,
    /// How far the area is scrolled (a device bigger than the window
    /// scrolls instead of shrinking), and how far it can be.
    pub scroll: (f32, f32),
    pub max_scroll: (f32, f32),
}

/// Lays the page out in an area `width` × `height` px, scrolled by
/// `scroll`, at `zoom` px per CSS px.
///
/// Responsive, the page fills the area: as many CSS px as the area is px
/// over the zoom. A device's page is always its own size in CSS px (times
/// the zoom), so resizing the window never changes how big its text is:
/// centred while it fits, else the area scrolls to show the rest (with a
/// margin around it).
pub fn layout(fixed: Option<(f32, f32)>, width: f32, height: f32, zoom: f32, scroll: (f32, f32)) -> Layout {
    let zoom = zoom.max(0.01);
    let Some((css_width, css_height)) = fixed else {
        return Layout {
            x: 0.,
            y: 0.,
            width,
            height,
            zoom,
            css_width: (width / zoom).round(),
            css_height: (height / zoom).round(),
            scroll: (0., 0.),
            max_scroll: (0., 0.),
        };
    };
    let (page_width, page_height) = (css_width * zoom, css_height * zoom);
    // One axis: centred if the page and its margins fit, else scrolled.
    let place = |area: f32, page: f32, scroll: f32| {
        let content = page + 2. * MARGIN * zoom;
        if content <= area {
            ((area - page) / 2., 0., 0.)
        } else {
            let max = content - area;
            let scroll = scroll.clamp(0., max);
            (MARGIN * zoom - scroll, scroll, max)
        }
    };
    let (x, scroll_x, max_x) = place(width, page_width, scroll.0);
    let (y, scroll_y, max_y) = place(height, page_height, scroll.1);
    Layout {
        x,
        y,
        width: page_width,
        height: page_height,
        zoom,
        css_width,
        css_height,
        scroll: (scroll_x, scroll_y),
        max_scroll: (max_x, max_y),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn on(device: &str, landscape: bool) -> Option<(f32, f32)> {
        fixed_size(Some(device), landscape)
    }

    #[test]
    fn responsive_fills_the_window() {
        let layout = layout(None, 1000., 600., 1., (40., 40.));
        assert_eq!((layout.x, layout.y, layout.width, layout.height), (0., 0., 1000., 600.));
        assert_eq!((layout.css_width, layout.css_height), (1000., 600.));
        assert_eq!((layout.scroll, layout.max_scroll), ((0., 0.), (0., 0.)));
        // Zoomed in to 200%, the page is half as many CSS px.
        let zoomed = super::layout(None, 1000., 600., 2., (0., 0.));
        assert_eq!((zoomed.css_width, zoomed.css_height, zoomed.zoom), (500., 300., 2.));
    }

    #[test]
    fn a_device_that_fits_is_centred_at_its_own_size() {
        let layout = layout(on("iphone-16-pro", false), 800., 1000., 1., (0., 0.));
        assert_eq!((layout.css_width, layout.css_height), (402., 874.));
        assert_eq!((layout.width, layout.height), (402., 874.));
        assert_eq!((layout.x, layout.y), (199., 63.));
        assert_eq!(layout.max_scroll, (0., 0.));
    }

    #[test]
    fn a_device_too_big_scrolls_and_never_shrinks() {
        // 1440 × 900 (1472 × 932 with margins) in a 752 × 532 area.
        let at = |scroll| layout(on("desktop", false), 752., 532., 1., scroll);
        let top = at((0., 0.));
        assert_eq!((top.width, top.height, top.zoom), (1440., 900., 1.));
        assert_eq!((top.x, top.y), (MARGIN, MARGIN));
        assert_eq!(top.max_scroll, (720., 400.));
        let scrolled = at((100., 50.));
        assert_eq!((scrolled.x, scrolled.y), (MARGIN - 100., MARGIN - 50.));
        // Scrolling stops at the far edge (and at the start).
        assert_eq!(at((5000., -20.)).scroll, (720., 0.));
        // Resizing the window keeps the page's size: only what's in view
        // changes.
        let wider = layout(on("desktop", false), 1200., 532., 1., (0., 0.));
        assert_eq!((wider.width, wider.height), (1440., 900.));
        // One axis can fit while the other scrolls.
        let tall = layout(on("iphone-16", false), 800., 500., 1., (0., 10.));
        assert_eq!((tall.x, tall.y, tall.max_scroll.0), (203.5, MARGIN - 10., 0.));
    }

    #[test]
    fn page_zoom_scales_the_device() {
        let half = layout(on("desktop", false), 1000., 800., 0.5, (0., 0.));
        assert_eq!((half.width, half.height), (720., 450.));
        assert_eq!((half.css_width, half.css_height), (1440., 900.));
        assert_eq!(half.max_scroll, (0., 0.));
    }

    #[test]
    fn landscape_turns_phones_on_their_side() {
        assert_eq!(on("iphone-16", true), Some((852., 393.)));
        assert_eq!(on("iphone-16", false), Some((393., 852.)));
        // Unknown ids (from a newer version) follow the window.
        assert_eq!(on("iphone-99", false), None);
        assert_eq!(fixed_size(None, true), None);
    }

    #[test]
    fn every_device_has_a_unique_id_and_the_quick_ones_exist() {
        for (ix, device) in DEVICES.iter().enumerate() {
            assert!(DEVICES[ix + 1..].iter().all(|other| other.id != device.id), "{}", device.id);
        }
        for (id, kind) in QUICK {
            assert_eq!(find(id).map(|device| device.kind), Some(kind));
        }
    }
}
