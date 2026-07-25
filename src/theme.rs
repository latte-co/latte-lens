//! Foreground-only terminal theme system.
//!
//! The theme is a pure color palette: every semantic token resolves to a single
//! foreground [`Color`]. Region layering, focus, and selection are expressed by
//! the callers through modifiers (`DIM`/`BOLD`/`REVERSED`) and glyphs, never by
//! painting a panel background. This keeps the viewer legible under `NO_COLOR`
//! and on 16-color terminals, where a token can degrade to `Color::Reset` while
//! the attribute and glyph cues survive.
//!
//! All environment reads happen once, at construction time, through
//! [`detect`]. Rendering code only ever calls [`Theme::current`], which is a
//! pure lookup against an already-resolved, process-global [`Theme`].

use std::sync::OnceLock;

use ratatui::style::Color;

/// How faithfully the terminal can render color. Ordered most to least capable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ColorMode {
    /// 24-bit direct color (`Color::Rgb`).
    TrueColor,
    /// 256-color palette (`Color::Indexed`).
    Ansi256,
    /// 16 named ANSI colors.
    Ansi16,
    /// No color at all (`NO_COLOR`): every token becomes `Color::Reset` so only
    /// modifiers and glyphs distinguish regions, focus, and diff polarity.
    None,
}

/// Which built-in Catppuccin flavor to use.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Flavor {
    /// Dark. Catppuccin Mocha.
    Mocha,
    /// Light. Catppuccin Latte.
    Latte,
}

/// A single palette color carrying its representation at every fidelity level.
///
/// The three fields let one value degrade deterministically: true color uses
/// `hex`, 256-color terminals use `idx256`, 16-color terminals use the named
/// `ansi16`, and `NO_COLOR` drops the foreground entirely.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RawColor {
    pub hex: (u8, u8, u8),
    pub idx256: u8,
    pub ansi16: Color,
}

impl RawColor {
    /// Resolve to the best foreground `Color` the terminal can show.
    ///
    /// `ColorMode::None` returns `Color::Reset`: only the foreground is dropped,
    /// so any modifier or glyph the caller applied is preserved.
    pub fn resolve(&self, mode: ColorMode) -> Color {
        match mode {
            ColorMode::TrueColor => Color::Rgb(self.hex.0, self.hex.1, self.hex.2),
            ColorMode::Ansi256 => Color::Indexed(self.idx256),
            ColorMode::Ansi16 => self.ansi16,
            ColorMode::None => Color::Reset,
        }
    }
}

const fn rc(hex: (u8, u8, u8), idx256: u8, ansi16: Color) -> RawColor {
    RawColor {
        hex,
        idx256,
        ansi16,
    }
}

/// Catppuccin named colors for one flavor. Custom themes build a `Palette`
/// (optionally by extending a preset) and everything downstream stays uniform.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Palette {
    pub base: RawColor,
    pub text: RawColor,
    pub subtext0: RawColor,
    pub overlay1: RawColor,
    pub surface2: RawColor,
    pub mauve: RawColor,
    pub blue: RawColor,
    pub green: RawColor,
    pub red: RawColor,
    pub peach: RawColor,
    pub yellow: RawColor,
    pub teal: RawColor,
    pub sky: RawColor,
    pub lavender: RawColor,
}

impl Palette {
    /// Resolve a `$name` palette reference used by external theme files.
    pub fn get(&self, name: &str) -> Option<RawColor> {
        Some(match name {
            "base" => self.base,
            "text" => self.text,
            "subtext0" => self.subtext0,
            "overlay1" => self.overlay1,
            "surface2" => self.surface2,
            "mauve" => self.mauve,
            "blue" => self.blue,
            "green" => self.green,
            "red" => self.red,
            "peach" => self.peach,
            "yellow" => self.yellow,
            "teal" => self.teal,
            "sky" => self.sky,
            "lavender" => self.lavender,
            _ => return None,
        })
    }

    /// Overwrite a named palette entry (used when an external theme sets
    /// `palette.blue = "#..."`). Unknown names are reported so the loader can
    /// warn and ignore rather than silently drop the value.
    pub fn set(&mut self, name: &str, color: RawColor) -> bool {
        let slot = match name {
            "base" => &mut self.base,
            "text" => &mut self.text,
            "subtext0" => &mut self.subtext0,
            "overlay1" => &mut self.overlay1,
            "surface2" => &mut self.surface2,
            "mauve" => &mut self.mauve,
            "blue" => &mut self.blue,
            "green" => &mut self.green,
            "red" => &mut self.red,
            "peach" => &mut self.peach,
            "yellow" => &mut self.yellow,
            "teal" => &mut self.teal,
            "sky" => &mut self.sky,
            "lavender" => &mut self.lavender,
            _ => return false,
        };
        *slot = color;
        true
    }
}

/// Catppuccin Mocha (dark). Official hex; ANSI-256/16 are close approximations.
pub const MOCHA: Palette = Palette {
    base: rc((0x1e, 0x1e, 0x2e), 235, Color::Black),
    text: rc((0xcd, 0xd6, 0xf4), 189, Color::White),
    subtext0: rc((0xa6, 0xad, 0xc8), 146, Color::Gray),
    overlay1: rc((0x7f, 0x84, 0x9c), 103, Color::DarkGray),
    surface2: rc((0x58, 0x5b, 0x70), 60, Color::DarkGray),
    mauve: rc((0xcb, 0xa6, 0xf7), 183, Color::LightMagenta),
    blue: rc((0x89, 0xb4, 0xfa), 111, Color::LightBlue),
    green: rc((0xa6, 0xe3, 0xa1), 151, Color::LightGreen),
    red: rc((0xf3, 0x8b, 0xa8), 211, Color::LightRed),
    peach: rc((0xfa, 0xb3, 0x87), 216, Color::LightRed),
    yellow: rc((0xf9, 0xe2, 0xaf), 223, Color::LightYellow),
    teal: rc((0x94, 0xe2, 0xd5), 116, Color::LightCyan),
    sky: rc((0x89, 0xdc, 0xeb), 117, Color::LightCyan),
    lavender: rc((0xb4, 0xbe, 0xfe), 147, Color::LightBlue),
};

/// Catppuccin Latte (light). Official hex; ANSI-256/16 are close approximations.
pub const LATTE: Palette = Palette {
    base: rc((0xef, 0xf1, 0xf5), 255, Color::White),
    text: rc((0x4c, 0x4f, 0x69), 60, Color::Black),
    subtext0: rc((0x6c, 0x6f, 0x85), 66, Color::DarkGray),
    overlay1: rc((0x8c, 0x8f, 0xa1), 103, Color::DarkGray),
    surface2: rc((0xac, 0xb0, 0xbe), 145, Color::Gray),
    mauve: rc((0x88, 0x39, 0xef), 92, Color::Magenta),
    blue: rc((0x1e, 0x66, 0xf5), 33, Color::Blue),
    green: rc((0x40, 0xa0, 0x2b), 64, Color::Green),
    red: rc((0xd2, 0x0f, 0x39), 160, Color::Red),
    peach: rc((0xfe, 0x64, 0x0b), 202, Color::Red),
    yellow: rc((0xdf, 0x8e, 0x1d), 172, Color::Yellow),
    teal: rc((0x17, 0x92, 0x99), 30, Color::Cyan),
    sky: rc((0x04, 0xa5, 0xe5), 38, Color::Cyan),
    lavender: rc((0x72, 0x87, 0xfd), 63, Color::Blue),
};

/// Return the built-in palette registered under `name`, if any. Preset names are
/// the stable identifiers accepted by external `extends` and `appearance` keys.
pub fn preset(name: &str) -> Option<Palette> {
    match name {
        "catppuccin-mocha" => Some(MOCHA),
        "catppuccin-latte" => Some(LATTE),
        _ => None,
    }
}

/// Is `name` a known built-in preset?
pub fn is_preset(name: &str) -> bool {
    preset(name).is_some()
}

/// The default flavor for a preset name, used to pick a `ColorMode` companion.
pub fn preset_flavor(name: &str) -> Option<Flavor> {
    match name {
        "catppuccin-mocha" => Some(Flavor::Mocha),
        "catppuccin-latte" => Some(Flavor::Latte),
        _ => None,
    }
}

/// Every semantic token, still in `RawColor` form. A custom theme overrides
/// individual entries here; resolution to the final `Theme` runs once, through
/// the same [`RawColor::resolve`] degrade chain, so custom and built-in colors
/// share one code path.
#[derive(Clone, Copy, Debug)]
pub struct Semantics {
    // Region identity.
    pub tree_accent: RawColor,
    pub content_accent: RawColor,
    pub git_accent: RawColor,
    // Structure.
    pub text_primary: RawColor,
    pub text_muted: RawColor,
    pub text_subtle: RawColor,
    pub divider: RawColor,
    // File types.
    pub dir: RawColor,
    pub file: RawColor,
    pub file_config: RawColor,
    pub file_doc: RawColor,
    pub file_media: RawColor,
    pub file_binary: RawColor,
    pub file_exec: RawColor,
    pub symlink: RawColor,
    pub missing: RawColor,
    pub tree_change_hint: RawColor,
    // Git status / review.
    pub status_add: RawColor,
    pub status_del: RawColor,
    pub status_mod: RawColor,
    pub status_renamed: RawColor,
    pub reviewed: RawColor,
    pub changed_after_review: RawColor,
    pub unreviewed: RawColor,
    // Diff.
    pub diff_add: RawColor,
    pub diff_del: RawColor,
    pub diff_hunk: RawColor,
    pub diff_file_header: RawColor,
    pub diff_context: RawColor,
    pub diff_meta: RawColor,
    // Syntax.
    pub syn_comment: RawColor,
    pub syn_string: RawColor,
    pub syn_keyword: RawColor,
    pub syn_function: RawColor,
    pub syn_type: RawColor,
    pub syn_number: RawColor,
    pub syn_constant: RawColor,
    pub syn_attribute: RawColor,
    // Interactive.
    pub search_match: RawColor,
    pub nav_target: RawColor,
    pub success: RawColor,
}

/// All semantic token names, used for name-keyed iteration.
const TOKEN_NAMES: &[&str] = &[
    "tree_accent",
    "content_accent",
    "git_accent",
    "text_primary",
    "text_muted",
    "text_subtle",
    "divider",
    "dir",
    "file",
    "file_config",
    "file_doc",
    "file_media",
    "file_binary",
    "file_exec",
    "symlink",
    "missing",
    "tree_change_hint",
    "status_add",
    "status_del",
    "status_mod",
    "status_renamed",
    "reviewed",
    "changed_after_review",
    "unreviewed",
    "diff_add",
    "diff_del",
    "diff_hunk",
    "diff_file_header",
    "diff_context",
    "diff_meta",
    "syn_comment",
    "syn_string",
    "syn_keyword",
    "syn_function",
    "syn_type",
    "syn_number",
    "syn_constant",
    "syn_attribute",
    "search_match",
    "nav_target",
    "success",
];

impl Semantics {
    /// The finalized semantic mapping for a palette (see module docs for the
    /// full token → color table).
    pub fn from_palette(p: &Palette) -> Self {
        Self {
            tree_accent: p.blue,
            content_accent: p.lavender,
            git_accent: p.teal,
            text_primary: p.text,
            text_muted: p.subtext0,
            text_subtle: p.surface2,
            divider: p.surface2,
            dir: p.blue,
            file: p.text,
            file_config: p.yellow,
            file_doc: p.subtext0,
            file_media: p.mauve,
            file_binary: p.peach,
            file_exec: p.green,
            symlink: p.sky,
            missing: p.red,
            tree_change_hint: p.peach,
            status_add: p.green,
            status_del: p.red,
            status_mod: p.peach,
            status_renamed: p.yellow,
            reviewed: p.green,
            changed_after_review: p.peach,
            unreviewed: p.subtext0,
            diff_add: p.green,
            diff_del: p.red,
            diff_hunk: p.peach,
            diff_file_header: p.blue,
            diff_context: p.text,
            diff_meta: p.subtext0,
            syn_comment: p.overlay1,
            syn_string: p.green,
            syn_keyword: p.mauve,
            syn_function: p.blue,
            syn_type: p.yellow,
            syn_number: p.peach,
            syn_constant: p.peach,
            syn_attribute: p.yellow,
            search_match: p.lavender,
            nav_target: p.green,
            success: p.green,
        }
    }

    /// Override one token by name (external theme `semantic` block). Unknown
    /// names return `false` so the loader warns and ignores instead of failing.
    pub fn set(&mut self, name: &str, color: RawColor) -> bool {
        let slot = match name {
            "tree_accent" => &mut self.tree_accent,
            "content_accent" => &mut self.content_accent,
            "git_accent" => &mut self.git_accent,
            "text_primary" => &mut self.text_primary,
            "text_muted" => &mut self.text_muted,
            "text_subtle" => &mut self.text_subtle,
            "divider" => &mut self.divider,
            "dir" => &mut self.dir,
            "file" => &mut self.file,
            "file_config" => &mut self.file_config,
            "file_doc" => &mut self.file_doc,
            "file_media" => &mut self.file_media,
            "file_binary" => &mut self.file_binary,
            "file_exec" => &mut self.file_exec,
            "symlink" => &mut self.symlink,
            "missing" => &mut self.missing,
            "tree_change_hint" => &mut self.tree_change_hint,
            "status_add" => &mut self.status_add,
            "status_del" => &mut self.status_del,
            "status_mod" => &mut self.status_mod,
            "status_renamed" => &mut self.status_renamed,
            "reviewed" => &mut self.reviewed,
            "changed_after_review" => &mut self.changed_after_review,
            "unreviewed" => &mut self.unreviewed,
            "diff_add" => &mut self.diff_add,
            "diff_del" => &mut self.diff_del,
            "diff_hunk" => &mut self.diff_hunk,
            "diff_file_header" => &mut self.diff_file_header,
            "diff_context" => &mut self.diff_context,
            "diff_meta" => &mut self.diff_meta,
            "syn_comment" => &mut self.syn_comment,
            "syn_string" => &mut self.syn_string,
            "syn_keyword" => &mut self.syn_keyword,
            "syn_function" => &mut self.syn_function,
            "syn_type" => &mut self.syn_type,
            "syn_number" => &mut self.syn_number,
            "syn_constant" => &mut self.syn_constant,
            "syn_attribute" => &mut self.syn_attribute,
            "search_match" => &mut self.search_match,
            "nav_target" => &mut self.nav_target,
            "success" => &mut self.success,
            _ => return false,
        };
        *slot = color;
        true
    }

    /// Read a token by name (mirror of [`Semantics::set`]).
    pub fn get(&self, name: &str) -> Option<RawColor> {
        Some(match name {
            "tree_accent" => self.tree_accent,
            "content_accent" => self.content_accent,
            "git_accent" => self.git_accent,
            "text_primary" => self.text_primary,
            "text_muted" => self.text_muted,
            "text_subtle" => self.text_subtle,
            "divider" => self.divider,
            "dir" => self.dir,
            "file" => self.file,
            "file_config" => self.file_config,
            "file_doc" => self.file_doc,
            "file_media" => self.file_media,
            "file_binary" => self.file_binary,
            "file_exec" => self.file_exec,
            "symlink" => self.symlink,
            "missing" => self.missing,
            "tree_change_hint" => self.tree_change_hint,
            "status_add" => self.status_add,
            "status_del" => self.status_del,
            "status_mod" => self.status_mod,
            "status_renamed" => self.status_renamed,
            "reviewed" => self.reviewed,
            "changed_after_review" => self.changed_after_review,
            "unreviewed" => self.unreviewed,
            "diff_add" => self.diff_add,
            "diff_del" => self.diff_del,
            "diff_hunk" => self.diff_hunk,
            "diff_file_header" => self.diff_file_header,
            "diff_context" => self.diff_context,
            "diff_meta" => self.diff_meta,
            "syn_comment" => self.syn_comment,
            "syn_string" => self.syn_string,
            "syn_keyword" => self.syn_keyword,
            "syn_function" => self.syn_function,
            "syn_type" => self.syn_type,
            "syn_number" => self.syn_number,
            "syn_constant" => self.syn_constant,
            "syn_attribute" => self.syn_attribute,
            "search_match" => self.search_match,
            "nav_target" => self.nav_target,
            "success" => self.success,
            _ => return None,
        })
    }

    /// Copy across the customizations a base theme applied: for every token
    /// where `base` diverges from `base_defaults` (the clean palette mapping of
    /// the base), adopt the base's value. Used when an external theme `extends`
    /// another so inherited semantic overrides survive a palette re-derive.
    pub fn carry_customizations(&mut self, base: &Semantics, base_defaults: &Semantics) {
        for name in TOKEN_NAMES {
            if let (Some(base_value), Some(default_value)) =
                (base.get(name), base_defaults.get(name))
                && base_value != default_value
            {
                self.set(name, base_value);
            }
        }
    }

    /// Resolve every token to a final `Color` at the given fidelity.
    pub fn resolve(&self, mode: ColorMode) -> Theme {
        let c = |raw: &RawColor| raw.resolve(mode);
        Theme {
            mode,
            tree_accent: c(&self.tree_accent),
            content_accent: c(&self.content_accent),
            git_accent: c(&self.git_accent),
            text_primary: c(&self.text_primary),
            text_muted: c(&self.text_muted),
            text_subtle: c(&self.text_subtle),
            divider: c(&self.divider),
            dir: c(&self.dir),
            file: c(&self.file),
            file_config: c(&self.file_config),
            file_doc: c(&self.file_doc),
            file_media: c(&self.file_media),
            file_binary: c(&self.file_binary),
            file_exec: c(&self.file_exec),
            symlink: c(&self.symlink),
            missing: c(&self.missing),
            tree_change_hint: c(&self.tree_change_hint),
            status_add: c(&self.status_add),
            status_del: c(&self.status_del),
            status_mod: c(&self.status_mod),
            status_renamed: c(&self.status_renamed),
            reviewed: c(&self.reviewed),
            changed_after_review: c(&self.changed_after_review),
            unreviewed: c(&self.unreviewed),
            diff_add: c(&self.diff_add),
            diff_del: c(&self.diff_del),
            diff_hunk: c(&self.diff_hunk),
            diff_file_header: c(&self.diff_file_header),
            diff_context: c(&self.diff_context),
            diff_meta: c(&self.diff_meta),
            syn_comment: c(&self.syn_comment),
            syn_string: c(&self.syn_string),
            syn_keyword: c(&self.syn_keyword),
            syn_function: c(&self.syn_function),
            syn_type: c(&self.syn_type),
            syn_number: c(&self.syn_number),
            syn_constant: c(&self.syn_constant),
            syn_attribute: c(&self.syn_attribute),
            search_match: c(&self.search_match),
            nav_target: c(&self.nav_target),
            success: c(&self.success),
        }
    }
}

/// Fully resolved, foreground-only theme. Every field is a ready-to-use
/// `Color`. Rendering code reads these directly and never touches the palette.
#[derive(Clone, Copy, Debug)]
pub struct Theme {
    pub mode: ColorMode,
    pub tree_accent: Color,
    pub content_accent: Color,
    pub git_accent: Color,
    pub text_primary: Color,
    pub text_muted: Color,
    pub text_subtle: Color,
    pub divider: Color,
    pub dir: Color,
    pub file: Color,
    pub file_config: Color,
    pub file_doc: Color,
    pub file_media: Color,
    pub file_binary: Color,
    pub file_exec: Color,
    pub symlink: Color,
    pub missing: Color,
    pub tree_change_hint: Color,
    pub status_add: Color,
    pub status_del: Color,
    pub status_mod: Color,
    pub status_renamed: Color,
    pub reviewed: Color,
    pub changed_after_review: Color,
    pub unreviewed: Color,
    pub diff_add: Color,
    pub diff_del: Color,
    pub diff_hunk: Color,
    pub diff_file_header: Color,
    pub diff_context: Color,
    pub diff_meta: Color,
    pub syn_comment: Color,
    pub syn_string: Color,
    pub syn_keyword: Color,
    pub syn_function: Color,
    pub syn_type: Color,
    pub syn_number: Color,
    pub syn_constant: Color,
    pub syn_attribute: Color,
    pub search_match: Color,
    pub nav_target: Color,
    pub success: Color,
}

impl Theme {
    /// Build a built-in theme from a fidelity level and flavor.
    pub fn from_parts(mode: ColorMode, flavor: Flavor) -> Self {
        let palette = match flavor {
            Flavor::Mocha => MOCHA,
            Flavor::Latte => LATTE,
        };
        Semantics::from_palette(&palette).resolve(mode)
    }

    /// The process-global theme. Defaults to a deterministic TrueColor Mocha the
    /// first time it is read; production installs a detected theme before the
    /// first render via [`install`]. This method performs no I/O.
    pub fn current() -> &'static Theme {
        THEME.get_or_init(|| Theme::from_parts(ColorMode::TrueColor, Flavor::Mocha))
    }
}

static THEME: OnceLock<Theme> = OnceLock::new();

/// Install the process-global theme. Only the first install wins; call this
/// once during startup, before any rendering. Later calls are ignored so a
/// stray call cannot repaint mid-session.
pub fn install(theme: Theme) {
    let _ = THEME.set(theme);
}

/// Detect the terminal's color fidelity and default flavor from the environment.
///
/// This is the single environment-reading entry point and must be called only
/// during construction. Priorities:
/// - `ColorMode`: `NO_COLOR` (non-empty) → `None`; else `COLORTERM ∈
///   {truecolor,24bit}` → `TrueColor`; else `TERM` contains `256color` →
///   `Ansi256`; else `Ansi16`.
/// - `Flavor`: `LATTE_LENS_THEME` = `light`/`dark` wins; else a large trailing
///   `COLORFGBG` field (light background) → `Latte`; else `Mocha`.
pub fn detect() -> (ColorMode, Flavor) {
    (detect_color_mode(), detect_flavor())
}

fn detect_color_mode() -> ColorMode {
    if std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty()) {
        return ColorMode::None;
    }
    if let Some(colorterm) = std::env::var_os("COLORTERM") {
        let colorterm = colorterm.to_string_lossy();
        if colorterm.eq_ignore_ascii_case("truecolor") || colorterm.eq_ignore_ascii_case("24bit") {
            return ColorMode::TrueColor;
        }
    }
    if let Some(term) = std::env::var_os("TERM")
        && term.to_string_lossy().contains("256color")
    {
        return ColorMode::Ansi256;
    }
    ColorMode::Ansi16
}

fn detect_flavor() -> Flavor {
    if let Some(explicit) = std::env::var_os("LATTE_LENS_THEME") {
        let explicit = explicit.to_string_lossy();
        if explicit.eq_ignore_ascii_case("light") {
            return Flavor::Latte;
        }
        if explicit.eq_ignore_ascii_case("dark") {
            return Flavor::Mocha;
        }
    }
    if let Some(fgbg) = std::env::var_os("COLORFGBG") {
        let fgbg = fgbg.to_string_lossy();
        if let Some(background) = fgbg.rsplit(';').next()
            && let Ok(value) = background.trim().parse::<u32>()
        {
            // A high trailing color index (e.g. 15/white, 7/gray) indicates a
            // light terminal background.
            if value == 7 || value >= 10 {
                return Flavor::Latte;
            }
        }
    }
    Flavor::Mocha
}

/// Detect the fidelity level and build the matching built-in theme in one step.
/// The flavor argument overrides the detected default (used once the config
/// layer has resolved an explicit dark/light choice).
pub fn detected_theme(flavor: Flavor) -> Theme {
    Theme::from_parts(detect_color_mode(), flavor)
}

/// Build a `RawColor` from a 24-bit hex triple, approximating the 256-color and
/// 16-color representations so a custom color degrades through the exact same
/// [`RawColor::resolve`] chain as a built-in one.
pub fn raw_from_rgb(r: u8, g: u8, b: u8) -> RawColor {
    RawColor {
        hex: (r, g, b),
        idx256: rgb_to_ansi256(r, g, b),
        ansi16: rgb_to_ansi16(r, g, b),
    }
}

/// Parse a theme-file color value into a `RawColor`.
///
/// Accepted forms:
/// - `#rrggbb` / `#rgb` hex,
/// - `$name` palette reference (resolved against `palette`),
/// - a 0-255 integer as an ANSI-256 index,
/// - an ANSI color name (`red`, `bright_blue`, `light-green`, ...).
///
/// Returns `None` for anything unparseable so the loader warns and keeps the
/// built-in token rather than failing the whole theme.
pub fn parse_color_value(value: &str, palette: &Palette) -> Option<RawColor> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if let Some(reference) = value.strip_prefix('$') {
        return palette.get(reference.trim());
    }
    if let Some(hex) = value.strip_prefix('#') {
        return parse_hex(hex);
    }
    if let Ok(index) = value.parse::<u8>() {
        // A bare integer is an ANSI-256 index; approximate hex/16 from it so the
        // degrade chain still works when the terminal is only true color-capable.
        let (r, g, b) = ansi256_to_rgb(index);
        return Some(RawColor {
            hex: (r, g, b),
            idx256: index,
            ansi16: rgb_to_ansi16(r, g, b),
        });
    }
    parse_named_color(value)
}

fn parse_hex(hex: &str) -> Option<RawColor> {
    let hex = hex.trim();
    let (r, g, b) = match hex.len() {
        6 => (
            u8::from_str_radix(&hex[0..2], 16).ok()?,
            u8::from_str_radix(&hex[2..4], 16).ok()?,
            u8::from_str_radix(&hex[4..6], 16).ok()?,
        ),
        3 => {
            let expand = |c: &str| u8::from_str_radix(c, 16).map(|value| value * 17);
            (
                expand(&hex[0..1]).ok()?,
                expand(&hex[1..2]).ok()?,
                expand(&hex[2..3]).ok()?,
            )
        }
        _ => return None,
    };
    Some(raw_from_rgb(r, g, b))
}

fn parse_named_color(value: &str) -> Option<RawColor> {
    // Normalize `bright_blue`, `light-green`, `Light Green` to `lightgreen`.
    let key: String = value
        .chars()
        .filter(|c| !matches!(c, '_' | '-' | ' '))
        .flat_map(char::to_lowercase)
        .collect();
    let key = key.replace("bright", "light");
    let named = match key.as_str() {
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "gray" | "grey" | "white" if key == "white" => Color::White,
        "gray" | "grey" => Color::Gray,
        "darkgray" | "darkgrey" => Color::DarkGray,
        "lightred" => Color::LightRed,
        "lightgreen" => Color::LightGreen,
        "lightyellow" => Color::LightYellow,
        "lightblue" => Color::LightBlue,
        "lightmagenta" => Color::LightMagenta,
        "lightcyan" => Color::LightCyan,
        "white" => Color::White,
        _ => return None,
    };
    // A named color has no faithful hex; approximate one so true-color terminals
    // still render it, while `ansi16` stays exact.
    let (r, g, b) = named_color_rgb(named);
    Some(RawColor {
        hex: (r, g, b),
        idx256: rgb_to_ansi256(r, g, b),
        ansi16: named,
    })
}

fn named_color_rgb(color: Color) -> (u8, u8, u8) {
    match color {
        Color::Black => (0, 0, 0),
        Color::Red => (0xa8, 0x00, 0x00),
        Color::Green => (0x00, 0xa8, 0x00),
        Color::Yellow => (0xa8, 0xa8, 0x00),
        Color::Blue => (0x00, 0x00, 0xa8),
        Color::Magenta => (0xa8, 0x00, 0xa8),
        Color::Cyan => (0x00, 0xa8, 0xa8),
        Color::Gray => (0xa8, 0xa8, 0xa8),
        Color::DarkGray => (0x54, 0x54, 0x54),
        Color::LightRed => (0xff, 0x54, 0x54),
        Color::LightGreen => (0x54, 0xff, 0x54),
        Color::LightYellow => (0xff, 0xff, 0x54),
        Color::LightBlue => (0x54, 0x54, 0xff),
        Color::LightMagenta => (0xff, 0x54, 0xff),
        Color::LightCyan => (0x54, 0xff, 0xff),
        Color::White => (0xff, 0xff, 0xff),
        _ => (0x80, 0x80, 0x80),
    }
}

/// Map an RGB triple to the closest ANSI-256 index (216-color cube or grayscale
/// ramp), matching the standard xterm quantization.
fn rgb_to_ansi256(r: u8, g: u8, b: u8) -> u8 {
    if r == g && g == b {
        // Grayscale ramp (232..=255) plus cube endpoints.
        if r < 8 {
            return 16;
        }
        if r > 248 {
            return 231;
        }
        return 232 + ((u16::from(r) - 8) * 24 / 247) as u8;
    }
    let q = |v: u8| -> u16 {
        if v < 48 {
            0
        } else if v < 115 {
            1
        } else {
            (u16::from(v) - 35) / 40
        }
    };
    16 + (36 * q(r) + 6 * q(g) + q(b)) as u8
}

fn ansi256_to_rgb(index: u8) -> (u8, u8, u8) {
    match index {
        0..=15 => named_color_rgb(match index {
            0 => Color::Black,
            1 => Color::Red,
            2 => Color::Green,
            3 => Color::Yellow,
            4 => Color::Blue,
            5 => Color::Magenta,
            6 => Color::Cyan,
            7 => Color::Gray,
            8 => Color::DarkGray,
            9 => Color::LightRed,
            10 => Color::LightGreen,
            11 => Color::LightYellow,
            12 => Color::LightBlue,
            13 => Color::LightMagenta,
            14 => Color::LightCyan,
            _ => Color::White,
        }),
        16..=231 => {
            let index = u16::from(index) - 16;
            let levels = [0u8, 95, 135, 175, 215, 255];
            (
                levels[(index / 36) as usize],
                levels[((index / 6) % 6) as usize],
                levels[(index % 6) as usize],
            )
        }
        232..=255 => {
            let value = 8 + (u16::from(index) - 232) * 10;
            let value = value.min(255) as u8;
            (value, value, value)
        }
    }
}

/// Reduce an RGB triple to the nearest of the 16 basic ANSI colors.
fn rgb_to_ansi16(r: u8, g: u8, b: u8) -> Color {
    const CANDIDATES: &[Color] = &[
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
    let target = (i32::from(r), i32::from(g), i32::from(b));
    CANDIDATES
        .iter()
        .copied()
        .min_by_key(|&candidate| {
            let (cr, cg, cb) = named_color_rgb(candidate);
            let dr = target.0 - i32::from(cr);
            let dg = target.1 - i32::from(cg);
            let db = target.2 - i32::from(cb);
            dr * dr + dg * dg + db * db
        })
        .unwrap_or(Color::White)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_degrades_through_every_fidelity_level() {
        let color = rc((0x12, 0x34, 0x56), 99, Color::LightBlue);
        assert_eq!(color.resolve(ColorMode::TrueColor), Color::Rgb(0x12, 0x34, 0x56));
        assert_eq!(color.resolve(ColorMode::Ansi256), Color::Indexed(99));
        assert_eq!(color.resolve(ColorMode::Ansi16), Color::LightBlue);
        // NO_COLOR drops only the foreground.
        assert_eq!(color.resolve(ColorMode::None), Color::Reset);
    }

    #[test]
    fn no_color_makes_every_token_reset_without_panicking() {
        for flavor in [Flavor::Mocha, Flavor::Latte] {
            let theme = Theme::from_parts(ColorMode::None, flavor);
            assert_eq!(theme.tree_accent, Color::Reset);
            assert_eq!(theme.syn_keyword, Color::Reset);
            assert_eq!(theme.diff_add, Color::Reset);
            assert_eq!(theme.text_primary, Color::Reset);
        }
    }

    #[test]
    fn both_builtin_flavors_build_in_truecolor() {
        let mocha = Theme::from_parts(ColorMode::TrueColor, Flavor::Mocha);
        assert_eq!(mocha.syn_keyword, Color::Rgb(0xcb, 0xa6, 0xf7));
        assert_eq!(mocha.tree_accent, Color::Rgb(0x89, 0xb4, 0xfa));
        assert_eq!(mocha.content_accent, Color::Rgb(0xb4, 0xbe, 0xfe));
        let latte = Theme::from_parts(ColorMode::TrueColor, Flavor::Latte);
        assert_eq!(latte.syn_keyword, Color::Rgb(0x88, 0x39, 0xef));
        assert_ne!(mocha.text_primary, latte.text_primary);
    }

    #[test]
    fn palette_reference_lookup_covers_named_colors() {
        assert_eq!(MOCHA.get("blue"), Some(MOCHA.blue));
        assert_eq!(MOCHA.get("lavender"), Some(MOCHA.lavender));
        assert_eq!(MOCHA.get("nope"), None);
    }

    #[test]
    fn semantic_override_by_name_is_scoped_to_one_token() {
        let mut semantics = Semantics::from_palette(&MOCHA);
        assert!(semantics.set("syn_keyword", MOCHA.red));
        assert!(!semantics.set("not_a_token", MOCHA.red));
        let theme = semantics.resolve(ColorMode::TrueColor);
        assert_eq!(theme.syn_keyword, Color::Rgb(0xf3, 0x8b, 0xa8));
        // Untouched tokens keep the built-in mapping.
        assert_eq!(theme.syn_function, Color::Rgb(0x89, 0xb4, 0xfa));
    }

    #[test]
    fn preset_registry_matches_flavors() {
        assert!(is_preset("catppuccin-mocha"));
        assert!(is_preset("catppuccin-latte"));
        assert!(!is_preset("dracula"));
        assert_eq!(preset_flavor("catppuccin-latte"), Some(Flavor::Latte));
    }

    #[test]
    fn parses_hex_palette_ref_index_and_named_colors() {
        assert_eq!(
            parse_color_value("#12ab34", &MOCHA).map(|c| c.hex),
            Some((0x12, 0xab, 0x34))
        );
        // Short hex expands each nibble.
        assert_eq!(
            parse_color_value("#f0a", &MOCHA).map(|c| c.hex),
            Some((0xff, 0x00, 0xaa))
        );
        // Palette reference resolves to the same RawColor.
        assert_eq!(parse_color_value("$blue", &MOCHA), Some(MOCHA.blue));
        // Named color keeps an exact ansi16 while approximating hex.
        assert_eq!(
            parse_color_value("light-green", &MOCHA).map(|c| c.ansi16),
            Some(Color::LightGreen)
        );
        // Bare integer becomes an ANSI-256 index.
        assert_eq!(parse_color_value("196", &MOCHA).map(|c| c.idx256), Some(196));
    }

    #[test]
    fn rejects_bad_color_values_and_missing_references() {
        assert!(parse_color_value("", &MOCHA).is_none());
        assert!(parse_color_value("#12", &MOCHA).is_none());
        assert!(parse_color_value("#zzzzzz", &MOCHA).is_none());
        assert!(parse_color_value("not-a-color", &MOCHA).is_none());
        assert!(parse_color_value("$nope", &MOCHA).is_none());
    }

    #[test]
    fn custom_color_uses_same_degrade_chain() {
        let custom = parse_color_value("#1e66f5", &LATTE).unwrap();
        assert_eq!(custom.resolve(ColorMode::TrueColor), Color::Rgb(0x1e, 0x66, 0xf5));
        // Degrades without panicking on lower-fidelity terminals.
        assert!(matches!(custom.resolve(ColorMode::Ansi256), Color::Indexed(_)));
        assert_eq!(custom.resolve(ColorMode::None), Color::Reset);
    }
}
