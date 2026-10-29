//! Fitting the palette roles to the terminal's own colours.
//!
//! A terminal that takes 24-bit colour (`COLORTERM=truecolor`) and reports its background gets the
//! app's own teal palette ([`Palette::fit`] via `truecolor`): a dark or a light set, chosen by the
//! background, each colour lightened or darkened until it reads there. Every other terminal draws
//! with its own 16 colours, and what changes is which of the 16 each role uses. Each role has its colours in order of preference and takes the first that reads on
//! the terminal's background: [`READABLE`] contrast first, then [`FLOOR`]; failing both, the
//! terminal's default text colour. So a theme where today's choices read keeps them, and a light
//! theme whose yellow or bright black fades into the background gets a colour that doesn't.
//!
//! Contrast is the WCAG 2 ratio (1 to 21). When the terminal doesn't say what a palette colour is,
//! a light background assumes macOS Terminal's Basic palette, and a dark one trusts today's
//! choices.

use ratatui::style::{Color, Style};

/// Contrast every role aims for: WCAG AA for text.
const READABLE: f64 = 4.5;
/// The least contrast any text is drawn with: WCAG AA for large text and UI parts. Bright black
/// on black sits at about 3.7 in common dark themes, so dim text clears it there.
const FLOOR: f64 = 3.0;
/// How far the selection fill must stand out from the background.
const FILL_VISIBLE: f64 = 1.5;

/// An sRGB colour as the terminal reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Rgb {
    r: u8,
    g: u8,
    b: u8,
}

impl Rgb {
    pub(crate) const BLACK: Self = Self::new(0, 0, 0);
    pub(crate) const WHITE: Self = Self::new(255, 255, 255);

    pub(crate) const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Relative luminance, 0 (black) to 1 (white).
    fn luminance(self) -> f64 {
        let linear = |c: u8| {
            let c = f64::from(c) / 255.0;
            if c <= 0.039_28 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(self.r) + 0.7152 * linear(self.g) + 0.0722 * linear(self.b)
    }

    /// The contrast ratio between two colours, 1 to 21.
    fn contrast(self, other: Self) -> f64 {
        let (a, b) = (self.luminance(), other.luminance());
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    /// Whether dark text reads better on it than light text.
    fn is_light(self) -> bool {
        self.contrast(Self::BLACK) > self.contrast(Self::WHITE)
    }

    /// `self` moved `t` (0 to 1) of the way to `other`.
    fn mix(self, other: Self, t: f64) -> Self {
        let channel = |a: u8, b: u8| {
            let v = f64::from(a) + (f64::from(b) - f64::from(a)) * t.clamp(0.0, 1.0);
            // In 0..=255 by construction.
            #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let v = v.round() as u8;
            v
        };
        Self::new(
            channel(self.r, other.r),
            channel(self.g, other.g),
            channel(self.b, other.b),
        )
    }

    /// `self`, moved towards black or white (away from `on`) until it reaches `min` contrast on
    /// `on`: the same hue, readable.
    fn reaching(self, on: Self, min: f64) -> Self {
        let away = if on.is_light() {
            Self::BLACK
        } else {
            Self::WHITE
        };
        (0..=20)
            .map(|step| self.mix(away, f64::from(step) / 20.0))
            .find(|c| c.contrast(on) >= min)
            .unwrap_or(away)
    }

    const fn color(self) -> Color {
        Color::Rgb(self.r, self.g, self.b)
    }

    /// A colour's sRGB value if it is one, else the reported colour of a palette index.
    fn of(color: Color, ansi: &[Option<Self>; 16]) -> Option<Self> {
        match color {
            Color::Rgb(r, g, b) => Some(Self::new(r, g, b)),
            other => index(other).and_then(|i| ansi[i]),
        }
    }
}

impl Reported {
    /// The colours as they travel to the host.
    pub(crate) fn to_wire(&self) -> qm_types::wire::Colors {
        let rgb = |c: Rgb| [c.r, c.g, c.b];
        qm_types::wire::Colors {
            fg: self.fg.map(rgb),
            bg: self.bg.map(rgb),
            ansi: self.ansi.iter().map(|c| c.map(rgb)).collect(),
            truecolor: self.truecolor,
            graphics: self.graphics,
            cell_pixels: self.cell_pixels.map(|(w, h)| [w, h]),
        }
    }

    /// The colours a client reported.
    pub(crate) fn from_wire(colors: &qm_types::wire::Colors) -> Self {
        let rgb = |[r, g, b]: [u8; 3]| Rgb::new(r, g, b);
        let mut ansi = [None; 16];
        for (slot, color) in ansi.iter_mut().zip(&colors.ansi) {
            *slot = color.map(rgb);
        }
        Self {
            fg: colors.fg.map(rgb),
            bg: colors.bg.map(rgb),
            ansi,
            truecolor: colors.truecolor,
            graphics: colors.graphics,
            cell_pixels: colors.cell_pixels.map(|[w, h]| (w, h)),
        }
    }
}

/// What the terminal said about its colours: `None` where it didn't answer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Reported {
    pub(crate) fg: Option<Rgb>,
    pub(crate) bg: Option<Rgb>,
    pub(crate) ansi: [Option<Rgb>; 16],
    /// Whether the terminal takes 24-bit colour.
    pub(crate) truecolor: bool,
    /// Whether the terminal shows kitty-protocol images.
    pub(crate) graphics: bool,
    /// One cell's size in pixels, (width, height).
    pub(crate) cell_pixels: Option<(u16, u16)>,
}

/// The 16 colours by palette index.
const ANSI: [Color; 16] = [
    Color::Black,
    Color::Red,
    Color::Green,
    Color::Yellow,
    Color::Blue,
    Color::Magenta,
    Color::Cyan,
    Color::Gray,
    Color::DarkGray,
    Color::LightRed,
    Color::LightGreen,
    Color::LightYellow,
    Color::LightBlue,
    Color::LightMagenta,
    Color::LightCyan,
    Color::White,
];

/// macOS Terminal's Basic profile, assumed for any colour a light terminal doesn't report.
const TYPICAL_LIGHT: [Rgb; 16] = [
    Rgb::new(0, 0, 0),
    Rgb::new(153, 0, 0),
    Rgb::new(0, 166, 0),
    Rgb::new(153, 153, 0),
    Rgb::new(0, 0, 178),
    Rgb::new(178, 0, 178),
    Rgb::new(0, 166, 178),
    Rgb::new(191, 191, 191),
    Rgb::new(102, 102, 102),
    Rgb::new(229, 0, 0),
    Rgb::new(0, 217, 0),
    Rgb::new(229, 229, 0),
    Rgb::new(0, 0, 255),
    Rgb::new(229, 0, 229),
    Rgb::new(0, 229, 229),
    Rgb::new(229, 229, 229),
];

/// The palette index of one of the 16 colours.
fn index(color: Color) -> Option<usize> {
    ANSI.iter().position(|c| *c == color)
}

/// The same hue at the other brightness: red and bright red, and so on.
fn twin(color: Color) -> Option<Color> {
    index(color).map(|i| ANSI[i ^ 8])
}

/// The colour each role draws with.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Palette {
    /// Secondary text, idle borders and hints.
    pub(crate) dim: Color,
    /// Focus, approvals, the active input: the brand teal.
    pub(crate) accent: Color,
    /// Decisions, notices, waiting (the accent in 16 colours; the words tell them apart).
    pub(crate) warn: Color,
    /// The background of a warning chip (`○ waits`); none in 16 colours.
    pub(crate) warn_tint: Option<Color>,
    /// Section headings and box titles (the default text colour in 16 colours).
    pub(crate) title: Color,
    /// Idle borders and rules (dim in 16 colours).
    pub(crate) border: Color,
    /// Inline code and paths (the accent in 16 colours).
    pub(crate) code: Color,
    /// Running, ready, passed.
    pub(crate) ok: Color,
    /// Diff additions: green text, or readable text on a green fill when green text fades.
    pub(crate) addition: Style,
    /// Errors.
    pub(crate) error: Color,
    /// A supervisor's marker in a session.
    pub(crate) blue: Color,
    /// A supervisor's name.
    pub(crate) violet: Color,
    /// The background of rest buttons, the current tab and the selected row.
    pub(crate) fill: Color,
    /// Text on the fill.
    pub(crate) on_fill: Color,
    /// Text on the accent: a hovered button.
    pub(crate) on_accent: Color,
    /// Text on [`Self::on_fill`]: a button on the selected row.
    pub(crate) on_inverse_fill: Color,
    /// The terminal's palette, where known, to check a glyph on the fill.
    ansi: [Option<Rgb>; 16],
}

impl Palette {
    /// The palette for a terminal that says nothing about its colours: tuned for a dark
    /// background, as before detection existed.
    pub(crate) const DEFAULT: Self = Self {
        dim: Color::DarkGray,
        accent: Color::Yellow,
        warn: Color::Yellow,
        warn_tint: None,
        title: Color::Reset,
        border: Color::DarkGray,
        code: Color::Yellow,
        ok: Color::Green,
        addition: Style::new().fg(Color::Green),
        error: Color::Red,
        blue: Color::LightBlue,
        violet: Color::Magenta,
        fill: Color::DarkGray,
        on_fill: Color::White,
        on_accent: Color::Black,
        on_inverse_fill: Color::Black,
        ansi: [None; 16],
    };

    /// The palette that reads on the terminal's reported colours.
    pub(crate) fn fit(reported: &Reported) -> Self {
        let Some(bg) = reported.bg.or_else(|| {
            // Only the text colour: the background is the other end.
            reported.fg.map(|fg| {
                if fg.is_light() {
                    Rgb::BLACK
                } else {
                    Rgb::WHITE
                }
            })
        }) else {
            return Self::DEFAULT;
        };
        let light = bg.is_light();
        let ansi: [Option<Rgb>; 16] =
            std::array::from_fn(|i| reported.ansi[i].or(light.then_some(TYPICAL_LIGHT[i])));
        let fit = Fit { bg, ansi };
        let accent = fit.pick(&[
            Color::Yellow,
            Color::LightYellow,
            Color::Blue,
            Color::LightBlue,
        ]);
        if reported.truecolor
            && let Some(bg) = reported.bg
        {
            return Self::truecolor(bg, reported.fg, ansi);
        }
        let (fill, on_fill) = fit.fill();
        let dim = fit.dim();
        let on_accent = if accent == Color::Reset {
            Color::Black
        } else {
            fit.text_on(
                accent,
                &[Color::Black, Color::White, Color::DarkGray, Color::Gray],
            )
        };
        Self {
            dim,
            accent,
            warn: accent,
            warn_tint: None,
            title: Color::Reset,
            border: dim,
            code: accent,
            ok: fit.pick(&[Color::Green, Color::LightGreen]),
            addition: fit.addition(),
            error: fit.pick(&[Color::Red, Color::LightRed]),
            blue: fit.pick(&[Color::LightBlue, Color::Blue, Color::LightCyan, Color::Cyan]),
            violet: fit.pick(&[Color::Magenta, Color::LightMagenta]),
            fill,
            on_fill,
            on_accent,
            on_inverse_fill: fit.text_on(
                on_fill,
                &[Color::Black, Color::White, Color::DarkGray, Color::Gray],
            ),
            ansi,
        }
    }

    /// The app's own colours on a terminal that takes 24-bit colour and reported background `bg`
    /// (and text `fg`, where known). Each colour keeps its hue but is moved until it reads on
    /// `bg`.
    fn truecolor(bg: Rgb, fg: Option<Rgb>, ansi: [Option<Rgb>; 16]) -> Self {
        let set = if bg.is_light() { &LIGHT } else { &DARK };
        let text = |c: Rgb| c.reaching(bg, READABLE).color();
        let accent = set.accent.reaching(bg, FLOOR);
        let fg = fg
            .filter(|fg| fg.contrast(bg) >= READABLE)
            .unwrap_or(if bg.is_light() {
                Rgb::BLACK
            } else {
                Rgb::WHITE
            });
        // A tint of the accent over the background: the selected row and rest buttons. On a
        // background as bright as the accent no tint stands out, so it moves away instead.
        let fill = (1..=10)
            .map(|step| bg.mix(set.accent, f64::from(step) * 0.04))
            .find(|c| c.contrast(bg) >= 1.25)
            .unwrap_or_else(|| bg.mix(set.accent, 0.12).reaching(bg, 1.25));
        let warn = set.warn.reaching(bg, READABLE);
        let tint = bg.mix(warn, 0.16);
        let on_accent = [set.on_accent, Rgb::BLACK, Rgb::WHITE]
            .into_iter()
            .find(|c| c.contrast(accent) >= READABLE)
            .unwrap_or(set.on_accent);
        Self {
            dim: set.dim.reaching(bg, FLOOR).color(),
            accent: accent.color(),
            warn: warn.reaching(tint, READABLE).color(),
            warn_tint: Some(tint.color()),
            title: text(set.title),
            border: set.border.reaching(bg, 1.4).color(),
            code: text(set.code),
            ok: text(set.ok),
            addition: Style::new().fg(text(set.ok)),
            error: text(set.error),
            blue: text(set.blue),
            violet: text(set.violet),
            fill: fill.color(),
            on_fill: fg.reaching(fill, READABLE).color(),
            on_accent: on_accent.color(),
            on_inverse_fill: fill.reaching(fg, READABLE).color(),
            ansi,
        }
    }

    /// A coloured glyph drawn on the fill: its own colour if that reads there, else the same hue
    /// at the other brightness, else the fill's text colour.
    pub(crate) fn on_fill_glyph(&self, color: Color) -> Color {
        let Some(fill) = Rgb::of(self.fill, &self.ansi) else {
            return color;
        };
        if let Color::Rgb(..) = color {
            // In 24-bit colour, the same hue moved until it reads on the fill.
            return Rgb::of(color, &self.ansi).map_or(color, |c| c.reaching(fill, FLOOR).color());
        }
        let reads = |c: Color| {
            index(c)
                .and_then(|i| self.ansi[i])
                .is_none_or(|rgb| rgb.contrast(fill) >= FLOOR)
        };
        if reads(color) {
            color
        } else {
            twin(color).filter(|t| reads(*t)).unwrap_or(self.on_fill)
        }
    }
}

/// One of the app's own colour sets, for a dark or a light background.
struct Set {
    dim: Rgb,
    title: Rgb,
    border: Rgb,
    accent: Rgb,
    on_accent: Rgb,
    warn: Rgb,
    ok: Rgb,
    error: Rgb,
    blue: Rgb,
    violet: Rgb,
    code: Rgb,
}

/// Teal on a dark background: the brand's #27A493 and cool greys and blues around it.
const DARK: Set = Set {
    dim: Rgb::new(0x71, 0x87, 0x9a),
    title: Rgb::new(0x9f, 0xb2, 0xc0),
    border: Rgb::new(0x2b, 0x3c, 0x49),
    accent: Rgb::new(0x27, 0xa4, 0x93),
    on_accent: Rgb::new(0x04, 0x22, 0x1d),
    warn: Rgb::new(0xd7, 0xb4, 0x6a),
    ok: Rgb::new(0x8f, 0xcf, 0x7f),
    error: Rgb::new(0xf0, 0x71, 0x78),
    blue: Rgb::new(0x6c, 0xb6, 0xff),
    violet: Rgb::new(0xa9, 0x9c, 0xf7),
    code: Rgb::new(0x7f, 0xd8, 0xc9),
};

/// Teal on a light background: the same hues, darker.
const LIGHT: Set = Set {
    dim: Rgb::new(0x5b, 0x6e, 0x7c),
    title: Rgb::new(0x3f, 0x52, 0x60),
    border: Rgb::new(0xc5, 0xd2, 0xdb),
    accent: Rgb::new(0x18, 0x7a, 0x6d),
    on_accent: Rgb::WHITE,
    warn: Rgb::new(0x8f, 0x62, 0x10),
    ok: Rgb::new(0x2f, 0x7d, 0x3a),
    error: Rgb::new(0xc2, 0x3b, 0x3b),
    blue: Rgb::new(0x25, 0x63, 0xc9),
    violet: Rgb::new(0x6a, 0x4f, 0xc9),
    code: Rgb::new(0x13, 0x69, 0x5e),
};

/// The terminal's background and palette, with gaps filled as the module says.
struct Fit {
    bg: Rgb,
    ansi: [Option<Rgb>; 16],
}

impl Fit {
    /// A colour's contrast on `on`; a colour the terminal didn't report counts as readable.
    fn contrast(&self, color: Color, on: Rgb) -> f64 {
        index(color)
            .and_then(|i| self.ansi[i])
            .map_or(f64::INFINITY, |rgb| rgb.contrast(on))
    }

    /// The first colour that reaches [`READABLE`] on the background, else the first that
    /// reaches [`FLOOR`], else the default text colour.
    fn pick(&self, preferred: &[Color]) -> Color {
        [READABLE, FLOOR]
            .into_iter()
            .find_map(|min| {
                preferred
                    .iter()
                    .copied()
                    .find(|c| self.contrast(*c, self.bg) >= min)
            })
            .unwrap_or(Color::Reset)
    }

    /// Keep the diff's semantic green without sacrificing text contrast. If the terminal's
    /// greens are too pale for text (common on white), use one as a fill instead. We cannot
    /// invent a darker RGB green without breaking the terminal's 16-colour contract.
    fn addition(&self) -> Style {
        let greens = [Color::Green, Color::LightGreen];
        if let Some(green) = greens
            .into_iter()
            .find(|c| self.contrast(*c, self.bg) >= READABLE)
        {
            return Style::new().fg(green);
        }
        let green =
            if self.contrast(Color::LightGreen, self.bg) > self.contrast(Color::Green, self.bg) {
                Color::LightGreen
            } else {
                Color::Green
            };
        Style::new().bg(green).fg(self.text_on(
            green,
            &[Color::Black, Color::White, Color::DarkGray, Color::Gray],
        ))
    }

    /// Bright black if it reads; else the faintest grey that does; else the default text colour.
    fn dim(&self) -> Color {
        if self.contrast(Color::DarkGray, self.bg) >= FLOOR {
            return Color::DarkGray;
        }
        [Color::Black, Color::Gray, Color::White]
            .into_iter()
            .map(|c| (c, self.contrast(c, self.bg)))
            .filter(|(_, contrast)| *contrast >= FLOOR)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map_or(Color::Reset, |(c, _)| c)
    }

    /// The fill and its text: the first grey that stands out from the background and carries
    /// readable text, bright black first.
    fn fill(&self) -> (Color, Color) {
        let texts = [Color::White, Color::Black, Color::Gray, Color::DarkGray];
        [Color::DarkGray, Color::Gray, Color::Black, Color::White]
            .into_iter()
            .find_map(|fill| {
                let rgb = index(fill).and_then(|i| self.ansi[i]);
                if rgb.is_some_and(|rgb| rgb.contrast(self.bg) < FILL_VISIBLE) {
                    return None;
                }
                let text = texts.iter().copied().find(|t| {
                    *t != fill && rgb.is_none_or(|rgb| self.contrast(*t, rgb) >= READABLE)
                })?;
                Some((fill, text))
            })
            .unwrap_or((Color::DarkGray, Color::White))
    }

    /// The first of `texts` that reads on `bg` (one of the 16), else the one that reads best.
    fn text_on(&self, bg: Color, texts: &[Color]) -> Color {
        let Some(rgb) = index(bg).and_then(|i| self.ansi[i]) else {
            return texts[0];
        };
        let candidates = texts.iter().copied().filter(|t| *t != bg);
        candidates
            .clone()
            .find(|t| self.contrast(*t, rgb) >= READABLE)
            .or_else(|| {
                candidates.max_by(|a, b| self.contrast(*a, rgb).total_cmp(&self.contrast(*b, rgb)))
            })
            .unwrap_or(texts[0])
    }
}
