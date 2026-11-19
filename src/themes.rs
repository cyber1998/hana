//! The looks of the interactive view. Each theme is a palette plus the few words and glyphs that give it
//! a voice. Themes with a `bg` paint their own background; the others keep the terminal's.

use ratatui::style::{Color, Style};
use ratatui::widgets::BorderType;

pub struct Theme {
    pub name: &'static str,
    /// One line for `hana themes` and the settings panel.
    pub blurb: &'static str,

    pub bg: Option<Color>,
    pub text: Color,
    pub dim: Color,
    pub accent: Color,
    /// Text drawn on top of the accent color (buttons, the active tab).
    pub on_accent: Color,
    pub good: Color,
    pub warn: Color,
    pub info: Color,
    pub select_bg: Color,
    pub border: BorderType,

    pub logo_dirty: &'static str,
    pub logo_clean: &'static str,
    pub logo_loading: &'static str,
    pub section_left: &'static str,
    pub section_right: &'static str,
    pub lowercase_sections: bool,
    pub clean: &'static str,
    pub no_sessions: &'static str,
    pub no_data: &'static str,
    pub busy_dot: &'static str,
    pub idle_dot: &'static str,
    pub unknown_dot: &'static str,
    pub killing: &'static str,
    pub killed_verb: &'static str,
    pub ending: &'static str,
    pub measuring: &'static str,
    pub deleting: &'static str,
    pub freed: &'static str,
    pub freed_suffix: &'static str,
    pub marked: &'static str,
    pub unmarked: &'static str,
    pub copied: &'static str,
    /// Said when the theme is picked.
    pub confirm_title: &'static str,
    pub settings_title: &'static str,
    pub help_title: &'static str,
    pub key_sep: &'static str,
}

impl Theme {
    /// The style every cell starts from: the theme's background and text colors.
    pub fn base(&self) -> Style {
        let style = Style::new().fg(self.text);
        match self.bg {
            Some(bg) => style.bg(bg),
            None => style,
        }
    }
}

const fn rgb(hex: u32) -> Color {
    Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

/// Rust's own colors: the rust-orange of its logo, warm greys, and the crab-shell #DEA584.
pub const RUST: Theme = Theme {
    name: "rust",
    blurb: "rust-orange on your terminal's own background",
    bg: None,
    text: rgb(0xE8DCCF),
    dim: rgb(0x8C7B70),
    accent: rgb(0xF74C00),
    on_accent: rgb(0x1A120D),
    good: rgb(0x8EC07C),
    warn: rgb(0xE6B450),
    info: rgb(0xDEA584),
    select_bg: rgb(0x3A2A22),
    border: BorderType::Plain,
    logo_dirty: " ◐ hana",
    logo_clean: " ● hana",
    logo_loading: " ○ hana",
    section_left: " ",
    section_right: "",
    lowercase_sections: false,
    clean: "  Nothing left behind. All clean.",
    no_sessions: "  No Claude Code session is open.",
    no_data: "  No session data on disk.",
    busy_dot: " ●",
    idle_dot: " ●",
    unknown_dot: " ○",
    killing: "killing…",
    killed_verb: "killed",
    ending: "ending…",
    measuring: "measuring…",
    deleting: "deleting…",
    freed: " Freed ",
    freed_suffix: "",
    marked: "■ ",
    unmarked: "□ ",
    copied: "Copied",
    confirm_title: " Confirm ",
    settings_title: " Settings ",
    help_title: " hana · any key to close ",
    key_sep: "  ",
};

pub const KAWAII: Theme = Theme {
    name: "kawaii",
    blurb: "pastel pinks, kaomoji and hearts",
    bg: None,
    text: rgb(0xF0E4FF),
    dim: rgb(0x968AB4),
    accent: rgb(0xFF79C6),
    on_accent: rgb(0x2A1030),
    good: rgb(0x78E8C0),
    warn: rgb(0xFFC88C),
    info: rgb(0x96CDFF),
    select_bg: rgb(0x482C54),
    border: BorderType::Rounded,
    logo_dirty: " (｡•︿•｡) hana",
    logo_clean: " (≧◡≦) hana",
    logo_loading: " (・_・ヾ hana",
    section_left: " ✿ ",
    section_right: " ✿",
    lowercase_sections: true,
    clean: "  ✧ nothing left behind ~ all sparkly clean! ٩(◕‿◕)۶ ✧",
    no_sessions: "  no claude sessions open ~ (－ω－) zzZ",
    no_data: "  no session data at all ~ so tidy! ♡",
    busy_dot: " ★",
    idle_dot: " ☾",
    unknown_dot: " ♡",
    killing: "bonk! ☆",
    killed_verb: "bonked",
    ending: "sayonara…",
    measuring: "counting… ⊂(・▽・⊂)",
    deleting: "poof! ✧",
    freed: " ✧ yay! freed ",
    freed_suffix: "  (ﾉ◕ヮ◕)ﾉ*:･ﾟ✧",
    marked: "♥ ",
    unmarked: "♡ ",
    copied: "copied! (๑˃ᴗ˂)ﻭ",
    confirm_title: " are you sure? (・・;) ",
    settings_title: " settings ♡ ",
    help_title: " hana help ✿ any key to close ",
    key_sep: " ♡ ",
};

pub const CYBERPUNK: Theme = Theme {
    name: "cyberpunk",
    blurb: "hot pink and electric cyan on midnight purple",
    bg: Some(rgb(0x0D0221)),
    text: rgb(0xD1F7FF),
    dim: rgb(0x6B5B95),
    accent: rgb(0xFF2A6D),
    on_accent: rgb(0x0D0221),
    good: rgb(0x01FFC3),
    warn: rgb(0xF9F002),
    info: rgb(0x05D9E8),
    select_bg: rgb(0x2A0A4A),
    border: BorderType::Thick,
    logo_dirty: " ▚▞ HANA//",
    logo_clean: " ▚▞ HANA//",
    logo_loading: " ▚▞ HANA//",
    section_left: " ▌",
    section_right: " //",
    clean: "  // GRID CLEAN. NO ROGUE PROCESSES.",
    no_sessions: "  // NO ACTIVE UPLINKS",
    no_data: "  // NO DATA SHARDS FOUND",
    busy_dot: " ▶",
    idle_dot: " ‖",
    unknown_dot: " ?",
    killing: "FLATLINING…",
    killed_verb: "flatlined",
    ending: "JACKING OUT…",
    measuring: "SCANNING…",
    deleting: "DEREZZING…",
    freed: " // RAM RECLAIMED: ",
    marked: "◆ ",
    unmarked: "◇ ",
    copied: "// COPIED TO BUFFER",
    confirm_title: " // CONFIRM EXECUTION ",
    settings_title: " // SYS.CONFIG ",
    help_title: " // HANA.MANUAL · ANY KEY ",
    key_sep: " │ ",
    ..RUST
};

pub const NEON: Theme = Theme {
    name: "neon",
    blurb: "glowing magenta, cyan and lime on black",
    bg: Some(rgb(0x0A0A12)),
    text: rgb(0xFFFFFF),
    dim: rgb(0x7A7A9D),
    accent: rgb(0xFF00FF),
    on_accent: rgb(0x0A0A12),
    good: rgb(0x39FF14),
    warn: rgb(0xFFFF00),
    info: rgb(0x00FFFF),
    select_bg: rgb(0x1E1E3F),
    border: BorderType::Double,
    logo_dirty: " ✦ hana ✦",
    logo_clean: " ✦ hana ✦",
    logo_loading: " ✧ hana ✧",
    section_left: " ≫ ",
    clean: "  ✦ all clear ✦",
    killing: "zap!",
    killed_verb: "zapped",
    freed: " ✦ freed ",
    marked: "◉ ",
    unmarked: "○ ",
    key_sep: " • ",
    ..RUST
};

/// After the Tokyo Night palette by enkia.
pub const TOKYO_NIGHT: Theme = Theme {
    name: "tokyo-night",
    blurb: "the soft blues of Tokyo Night",
    bg: Some(rgb(0x1A1B26)),
    text: rgb(0xC0CAF5),
    dim: rgb(0x565F89),
    accent: rgb(0x7AA2F7),
    on_accent: rgb(0x1A1B26),
    good: rgb(0x9ECE6A),
    warn: rgb(0xE0AF68),
    info: rgb(0x7DCFFF),
    select_bg: rgb(0x283457),
    border: BorderType::Rounded,
    logo_dirty: " 夜 hana",
    logo_clean: " 夜 hana",
    logo_loading: " 夜 hana",
    section_left: " ◆ ",
    lowercase_sections: true,
    clean: "  nothing left behind. the city sleeps.",
    killed_verb: "closed",
    freed: " freed ",
    marked: "◆ ",
    unmarked: "◇ ",
    ..RUST
};

/// After GitHub's dark default palette (Primer).
pub const GITHUB: Theme = Theme {
    name: "github",
    blurb: "GitHub dark: Primer blues and greens",
    bg: Some(rgb(0x0D1117)),
    text: rgb(0xE6EDF3),
    dim: rgb(0x7D8590),
    accent: rgb(0x2F81F7),
    on_accent: rgb(0xFFFFFF),
    good: rgb(0x3FB950),
    warn: rgb(0xD29922),
    info: rgb(0xA5D6FF),
    select_bg: rgb(0x1C2A3F),
    border: BorderType::Rounded,
    logo_dirty: " ◉ hana",
    logo_clean: " ✓ hana",
    clean: "  ✓ Nothing left behind. Working tree clean.",
    killed_verb: "closed",
    freed: " ✓ Freed ",
    marked: "☑ ",
    unmarked: "☐ ",
    ..RUST
};

pub const MATRIX: Theme = Theme {
    name: "matrix",
    blurb: "phosphor green on black",
    bg: Some(rgb(0x000000)),
    text: rgb(0x00FF41),
    dim: rgb(0x008F11),
    accent: rgb(0x00FF41),
    on_accent: rgb(0x000000),
    good: rgb(0x9CFF9C),
    warn: rgb(0xCCFF00),
    info: rgb(0x39FF14),
    select_bg: rgb(0x003B00),
    border: BorderType::Plain,
    logo_dirty: " ⣿ hana",
    logo_clean: " ⣿ hana",
    logo_loading: " ⣀ hana",
    section_left: " > ",
    lowercase_sections: true,
    clean: "  > no anomalies in the system",
    no_sessions: "  > no one is plugged in",
    killing: "unplugging…",
    killed_verb: "unplugged",
    deleting: "deleting…",
    freed: " > freed ",
    marked: "[x] ",
    unmarked: "[ ] ",
    copied: "> copied",
    key_sep: "  ",
    ..RUST
};

pub const DEEP_OCEAN: Theme = Theme {
    name: "deep-ocean",
    blurb: "teal and foam on the dark of the deep",
    bg: Some(rgb(0x0B1622)),
    text: rgb(0xCFE8F5),
    dim: rgb(0x4E6A80),
    accent: rgb(0x2EC4B6),
    on_accent: rgb(0x0B1622),
    good: rgb(0x7FDBCA),
    warn: rgb(0xF4D35E),
    info: rgb(0x5DADE2),
    select_bg: rgb(0x13293D),
    border: BorderType::Rounded,
    logo_dirty: " ≋ hana",
    logo_clean: " ≋ hana",
    logo_loading: " ∿ hana",
    section_left: " ≋ ",
    lowercase_sections: true,
    clean: "  calm waters. nothing left behind.",
    killing: "sinking…",
    killed_verb: "sunk",
    deleting: "washing away…",
    freed: " ≋ washed away ",
    marked: "● ",
    unmarked: "○ ",
    ..RUST
};

pub const AUTUMN_FOREST: Theme = Theme {
    name: "autumn-forest",
    blurb: "maple orange, moss and gold on bark brown",
    bg: Some(rgb(0x1C1410)),
    text: rgb(0xEADBC8),
    dim: rgb(0x8A7560),
    accent: rgb(0xD9822B),
    on_accent: rgb(0x1C1410),
    good: rgb(0x8DA35F),
    warn: rgb(0xE4B363),
    info: rgb(0xC9A227),
    select_bg: rgb(0x3A2A1E),
    border: BorderType::Rounded,
    logo_dirty: " ❦ hana",
    logo_clean: " ❦ hana",
    section_left: " ❦ ",
    lowercase_sections: true,
    clean: "  the path is swept. nothing left behind.",
    killing: "falling…",
    killed_verb: "raked up",
    freed: " ❦ raked up ",
    marked: "❧ ",
    unmarked: "· ",
    ..RUST
};

/// Hana means flower: this one is the namesake.
pub const CHERRY_BLOSSOM: Theme = Theme {
    name: "cherry-blossom",
    blurb: "sakura pink and spring green at dusk",
    bg: Some(rgb(0x1F1520)),
    text: rgb(0xFBE4EC),
    dim: rgb(0xA67F8E),
    accent: rgb(0xF4A7C0),
    on_accent: rgb(0x1F1520),
    good: rgb(0xB5E8C9),
    warn: rgb(0xFFD6A5),
    info: rgb(0xE8B4D8),
    select_bg: rgb(0x3D2433),
    border: BorderType::Rounded,
    logo_dirty: " ❀ hana",
    logo_clean: " ❀ hana",
    logo_loading: " ✿ hana",
    section_left: " ❀ ",
    lowercase_sections: true,
    clean: "  ❀ nothing left behind. only petals.",
    killing: "drifting…",
    killed_verb: "drifted away",
    deleting: "scattering…",
    freed: " ❀ freed ",
    marked: "✿ ",
    unmarked: "❀ ",
    key_sep: " · ",
    ..RUST
};

/// A light theme: ink on paper, apple red.
pub const WHITE_ORCHARD: Theme = Theme {
    name: "white-orchard",
    blurb: "light: apple red and leaf green on paper",
    bg: Some(rgb(0xFAF8F2)),
    text: rgb(0x3B3A36),
    dim: rgb(0x9A968A),
    accent: rgb(0xC0392B),
    on_accent: rgb(0xFFFFFF),
    good: rgb(0x4C8C3F),
    warn: rgb(0xB7791F),
    info: rgb(0x2E7D6B),
    select_bg: rgb(0xEDE6D6),
    border: BorderType::Rounded,
    logo_dirty: " ✾ hana",
    logo_clean: " ✾ hana",
    section_left: " ✾ ",
    lowercase_sections: true,
    clean: "  the orchard is tidy. nothing left behind.",
    killed_verb: "picked",
    freed: " ✾ freed ",
    marked: "● ",
    unmarked: "○ ",
    ..RUST
};

pub const THEMES: [&Theme; 11] =
    [&CHERRY_BLOSSOM, &RUST, &KAWAII, &CYBERPUNK, &NEON, &TOKYO_NIGHT, &GITHUB, &MATRIX, &DEEP_OCEAN, &AUTUMN_FOREST, &WHITE_ORCHARD];

/// The default look.
pub const DEFAULT: &Theme = &CHERRY_BLOSSOM;

/// The theme with this name (case and spaces/underscores ignored), or the default.
pub fn by_name(name: &str) -> &'static Theme {
    find(name).unwrap_or(DEFAULT)
}

pub fn find(name: &str) -> Option<&'static Theme> {
    let want = name.trim().to_lowercase().replace([' ', '_'], "-");
    // Earlier names of the rust theme.
    let want = if want == "original" || want == "classic" { "rust".to_string() } else { want };
    THEMES.iter().copied().find(|t| t.name == want)
}

/// The theme after (or before) this one, wrapping around.
pub fn cycle(name: &str, forward: bool) -> &'static Theme {
    let n = THEMES.len();
    let at = THEMES.iter().position(|t| t.name == by_name(name).name).unwrap_or(0);
    THEMES[if forward { (at + 1) % n } else { (at + n - 1) % n }]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_resolve_and_cycle() {
        assert_eq!(by_name("Tokyo Night").name, "tokyo-night");
        assert_eq!(by_name("nope").name, "cherry-blossom");
        assert_eq!(by_name("original").name, "rust");
        assert_eq!(by_name("classic").name, "rust");
        assert_eq!(cycle("white-orchard", true).name, "cherry-blossom");
        assert_eq!(cycle("cherry-blossom", true).name, "rust");
        assert_eq!(cycle("cherry-blossom", false).name, "white-orchard");
        let mut names: Vec<&str> = THEMES.iter().map(|t| t.name).collect();
        names.dedup();
        assert_eq!(names.len(), THEMES.len());
    }
}
