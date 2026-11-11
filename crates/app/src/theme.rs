//! Kata on the desktop: Material 3's tonal surfaces seeded from suō, Zen Maru type, and the
//! hanko seal for buttons — a carved block on a 2px base, never a pill.

use gpui_kit::*;

#[derive(Clone, Copy)]
pub struct Palette {
    pub background: Hsla,
    pub surface: Hsla,
    pub container: Hsla,
    pub container_high: Hsla,
    pub container_highest: Hsla,
    pub on_surface: Hsla,
    pub on_surface_variant: Hsla,
    pub outline: Hsla,
    pub outline_variant: Hsla,
    pub primary: Hsla,
    pub primary_deep: Hsla,
    pub on_primary: Hsla,
    pub primary_container: Hsla,
    pub moss: Hsla,
    pub error: Hsla,
    pub ink: Hsla,
}

fn c(hex: u32) -> Hsla {
    rgb(hex).into()
}

pub fn light() -> Palette {
    Palette {
        background: c(0xFFFBF7),
        surface: c(0xFCF4EF),
        container: c(0xF7EEE8),
        container_high: c(0xF1E8E2),
        container_highest: c(0xEBE2DC),
        on_surface: c(0x221A17),
        on_surface_variant: c(0x54443F),
        outline: c(0x86736E),
        outline_variant: c(0xD9C2BC),
        primary: c(0xA84D63),
        primary_deep: c(0x7E3448),
        on_primary: c(0xFFFDF8),
        primary_container: c(0xFFD9DF),
        moss: c(0x5C7150),
        error: c(0xBA1A1A),
        ink: c(0x4E2B37),
    }
}

pub fn dark() -> Palette {
    Palette {
        background: c(0x19120F),
        surface: c(0x221A17),
        container: c(0x261E1B),
        container_high: c(0x312825),
        container_highest: c(0x3C3330),
        on_surface: c(0xEFE0DA),
        on_surface_variant: c(0xD8C2BD),
        outline: c(0xA08C88),
        outline_variant: c(0x534340),
        primary: c(0xE2A0A8),
        primary_deep: c(0xD06A82),
        on_primary: c(0x3A2128),
        primary_container: c(0x7E3448),
        moss: c(0xBFD0AE),
        error: c(0xFFB4AB),
        ink: c(0xF2E1CA),
    }
}

pub fn palette(window: &Window) -> Palette {
    match window.appearance() {
        WindowAppearance::Dark | WindowAppearance::VibrantDark => dark(),
        _ => light(),
    }
}

pub const SANS: &str = "Zen Maru Gothic";
pub const SERIF: &str = "Shippori Mincho";

pub fn load_fonts(cx: &mut App) {
    let fonts: Vec<std::borrow::Cow<'static, [u8]>> = vec![
        std::borrow::Cow::Borrowed(include_bytes!("../assets/fonts/zen_maru_regular.ttf")),
        std::borrow::Cow::Borrowed(include_bytes!("../assets/fonts/zen_maru_medium.ttf")),
        std::borrow::Cow::Borrowed(include_bytes!("../assets/fonts/zen_maru_bold.ttf")),
        std::borrow::Cow::Borrowed(include_bytes!("../assets/fonts/shippori_mincho_semibold.ttf")),
    ];
    let _ = cx.text_system().add_fonts(fonts);
}

#[derive(Clone, Copy, PartialEq)]
pub enum Tone {
    /// Suō pigment with sheet lettering: the one thing to press.
    Inked,
    /// The same block uninked: fold paper on a hair base.
    Quiet,
}

/// A hanko seal. Pressing sinks it: the base collapses under the block.
pub fn seal(id: impl Into<ElementId>, label: impl Into<SharedString>, tone: Tone, enabled: bool, p: &Palette) -> Stateful<Div> {
    let (face, text, base, hover) = match (tone, enabled) {
        (_, false) => (p.container_high, p.outline, p.container_high, p.container_high),
        (Tone::Inked, true) => (p.primary, p.on_primary, p.primary_deep, p.primary_deep),
        (Tone::Quiet, true) => (p.container_high, p.on_surface_variant, p.outline_variant, p.container_highest),
    };
    let ring = if tone == Tone::Inked && enabled { Some(p.on_primary.opacity(0.32)) } else { None };
    let label: SharedString = label.into();
    let mut block = div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .h(px(38.))
        .px(px(4.))
        .rounded(px(3.))
        .bg(face)
        /* The base: a 2px block of darker pigment under the face. */
        .border_b(px(if enabled { 3. } else { 0. }))
        .border_color(base)
        .text_color(text)
        .font_family(SANS)
        .font_weight(FontWeight::MEDIUM)
        .text_size(px(14.));
    if enabled {
        block = block.cursor_pointer().hover(move |s| s.bg(hover)).active(|s| s.border_b(px(0.)).mt(px(3.)));
    }
    let inner = div().flex().items_center().justify_center().h_full().px(px(16.)).rounded(px(1.)).child(label);
    block.child(match ring {
        /* The carved ring, 4px in from the edge. */
        Some(ring) => inner.my(px(4.)).h(px(26.)).border_1().border_color(ring),
        None => inner,
    })
}

/// Drawn as M3's linear indicator: a track and a suō bar.
pub fn bar(fraction: f32, p: &Palette) -> Div {
    let fraction = fraction.clamp(0., 1.);
    div().h(px(6.)).w_full().rounded(px(3.)).bg(p.container_highest).child(
        div().h_full().rounded(px(3.)).bg(p.primary).w(relative(fraction)),
    )
}

pub fn label(text: impl Into<SharedString>, p: &Palette) -> Div {
    div().text_size(px(13.)).text_color(p.on_surface_variant).child(text.into())
}
