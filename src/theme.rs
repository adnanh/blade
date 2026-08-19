use std::path::{Path, PathBuf};

use ratatui::style::Color;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    pub accent: Color,
    pub accent_text: Color,
    pub muted: Color,
    pub text: Color,
    pub footer: Color,
    pub search: Color,
    pub waiting: Color,
    pub running: Color,
    pub stopping: Color,
    pub completed: Color,
    pub failed: Color,
}

pub const PRESETS: [&str; 17] = [
    "default",
    "red",
    "yellow",
    "orange",
    "matrix",
    "matrix-alt",
    "purple",
    "blue",
    "gray",
    "sand",
    "nord",
    "gruvbox",
    "dracula",
    "catppuccin",
    "tokyo-night",
    "solarized-dark",
    "monochrome",
];

#[derive(Debug, Clone, Default)]
pub struct ThemeCatalog {
    custom: Vec<NamedTheme>,
}

#[derive(Debug, Clone)]
struct NamedTheme {
    name: String,
    theme: Theme,
    description: Option<String>,
    source: Option<PathBuf>,
}

impl ThemeCatalog {
    pub fn insert_with_metadata(
        &mut self,
        name: String,
        theme: Theme,
        description: Option<String>,
        source: Option<PathBuf>,
    ) -> bool {
        if Theme::preset(&name).is_some()
            || self.custom.iter().any(|candidate| candidate.name == name)
        {
            return false;
        }
        self.custom.push(NamedTheme {
            name,
            theme,
            description,
            source,
        });
        true
    }

    pub fn resolve(&self, name: &str) -> Option<Theme> {
        Theme::preset(name).or_else(|| {
            let normalized = name.trim().to_ascii_lowercase();
            self.custom
                .iter()
                .find(|candidate| candidate.name == normalized)
                .map(|candidate| candidate.theme.clone())
        })
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        PRESETS
            .iter()
            .copied()
            .chain(self.custom.iter().map(|theme| theme.name.as_str()))
    }

    pub fn len(&self) -> usize {
        PRESETS.len() + self.custom.len()
    }

    pub fn name(&self, index: usize) -> Option<&str> {
        self.names().nth(index)
    }

    pub fn description(&self, name: &str) -> Option<&str> {
        let normalized = name.trim().to_ascii_lowercase();
        self.custom
            .iter()
            .find(|candidate| candidate.name == normalized)
            .and_then(|candidate| candidate.description.as_deref())
    }

    pub fn has_custom(&self) -> bool {
        !self.custom.is_empty()
    }

    pub fn source(&self, name: &str) -> Option<&Path> {
        let normalized = name.trim().to_ascii_lowercase();
        self.custom
            .iter()
            .find(|candidate| candidate.name == normalized)
            .and_then(|candidate| candidate.source.as_deref())
    }

    pub fn custom_name_for_source(&self, source: &Path) -> Option<&str> {
        self.custom
            .iter()
            .find(|candidate| candidate.source.as_deref() == Some(source))
            .map(|candidate| candidate.name.as_str())
    }

    pub fn custom_name_for_theme(&self, theme: &Theme) -> Option<&str> {
        self.custom
            .iter()
            .find(|candidate| &candidate.theme == theme)
            .map(|candidate| candidate.name.as_str())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThemeOverrides {
    pub accent: Option<Color>,
    pub accent_text: Option<Color>,
    pub muted: Option<Color>,
    pub text: Option<Color>,
    pub footer: Option<Color>,
    pub search: Option<Color>,
    pub waiting: Option<Color>,
    pub running: Option<Color>,
    pub stopping: Option<Color>,
    pub completed: Option<Color>,
    pub failed: Option<Color>,
}

impl ThemeOverrides {
    pub fn apply(&self, theme: &mut Theme) {
        macro_rules! apply_override {
            ($field:ident) => {
                if let Some(color) = self.$field {
                    theme.$field = color;
                }
            };
        }

        apply_override!(accent);
        apply_override!(accent_text);
        apply_override!(muted);
        apply_override!(text);
        apply_override!(footer);
        apply_override!(search);
        apply_override!(waiting);
        apply_override!(running);
        apply_override!(stopping);
        apply_override!(completed);
        apply_override!(failed);
    }

    pub fn is_complete(&self) -> bool {
        self.accent.is_some()
            && self.accent_text.is_some()
            && self.muted.is_some()
            && self.text.is_some()
            && self.footer.is_some()
            && self.search.is_some()
            && self.waiting.is_some()
            && self.running.is_some()
            && self.stopping.is_some()
            && self.completed.is_some()
            && self.failed.is_some()
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            accent: Color::Cyan,
            accent_text: Color::Black,
            muted: Color::DarkGray,
            text: Color::Reset,
            footer: Color::Gray,
            search: Color::Yellow,
            waiting: Color::Yellow,
            running: Color::Green,
            stopping: Color::LightYellow,
            completed: Color::Cyan,
            failed: Color::Red,
        }
    }
}

impl Theme {
    pub fn preset(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "default" | "blade" => Some(Self::default()),
            "red" => Some(Self {
                accent: Color::Rgb(239, 68, 68),
                accent_text: Color::Rgb(24, 24, 27),
                muted: Color::Rgb(113, 113, 122),
                text: Color::Reset,
                footer: Color::Rgb(228, 228, 231),
                search: Color::Rgb(251, 191, 36),
                waiting: Color::Rgb(250, 204, 21),
                running: Color::Rgb(74, 222, 128),
                stopping: Color::Rgb(251, 146, 60),
                completed: Color::Rgb(248, 113, 113),
                failed: Color::Rgb(220, 38, 38),
            }),
            "yellow" => Some(Self {
                accent: Color::Rgb(250, 204, 21),
                accent_text: Color::Rgb(28, 25, 23),
                muted: Color::Rgb(120, 113, 108),
                text: Color::Reset,
                footer: Color::Rgb(231, 229, 228),
                search: Color::Rgb(253, 224, 71),
                waiting: Color::Rgb(234, 179, 8),
                running: Color::Rgb(132, 204, 22),
                stopping: Color::Rgb(245, 158, 11),
                completed: Color::Rgb(250, 204, 21),
                failed: Color::Rgb(239, 68, 68),
            }),
            "orange" => Some(Self {
                accent: Color::Rgb(249, 115, 22),
                accent_text: Color::Rgb(28, 25, 23),
                muted: Color::Rgb(120, 113, 108),
                text: Color::Reset,
                footer: Color::Rgb(231, 229, 228),
                search: Color::Rgb(251, 191, 36),
                waiting: Color::Rgb(245, 158, 11),
                running: Color::Rgb(101, 163, 13),
                stopping: Color::Rgb(249, 115, 22),
                completed: Color::Rgb(251, 146, 60),
                failed: Color::Rgb(220, 38, 38),
            }),
            "matrix" => Some(matrix_theme(Color::Rgb(134, 239, 172))),
            "matrix-alt" => Some(matrix_theme(Color::White)),
            "purple" => Some(Self {
                accent: Color::Rgb(168, 85, 247),
                accent_text: Color::Rgb(30, 16, 47),
                muted: Color::Rgb(107, 91, 123),
                text: Color::Reset,
                footer: Color::Rgb(233, 213, 255),
                search: Color::Rgb(240, 171, 252),
                waiting: Color::Rgb(216, 180, 254),
                running: Color::Rgb(134, 239, 172),
                stopping: Color::Rgb(245, 158, 11),
                completed: Color::Rgb(192, 132, 252),
                failed: Color::Rgb(251, 113, 133),
            }),
            "blue" => Some(Self {
                accent: Color::Rgb(59, 130, 246),
                accent_text: Color::Rgb(11, 18, 32),
                muted: Color::Rgb(100, 116, 139),
                text: Color::Reset,
                footer: Color::Rgb(219, 234, 254),
                search: Color::Rgb(103, 232, 249),
                waiting: Color::Rgb(250, 204, 21),
                running: Color::Rgb(74, 222, 128),
                stopping: Color::Rgb(251, 146, 60),
                completed: Color::Rgb(96, 165, 250),
                failed: Color::Rgb(248, 113, 113),
            }),
            "gray" => Some(Self {
                accent: Color::Rgb(156, 163, 175),
                accent_text: Color::Rgb(17, 24, 39),
                muted: Color::Rgb(75, 85, 99),
                text: Color::Reset,
                footer: Color::Rgb(209, 213, 219),
                search: Color::Rgb(229, 231, 235),
                waiting: Color::Rgb(251, 191, 36),
                running: Color::Rgb(134, 239, 172),
                stopping: Color::Rgb(251, 146, 60),
                completed: Color::Rgb(209, 213, 219),
                failed: Color::Rgb(248, 113, 113),
            }),
            "sand" => Some(Self {
                accent: Color::Rgb(214, 185, 140),
                accent_text: Color::Rgb(41, 36, 30),
                muted: Color::Rgb(120, 107, 90),
                text: Color::Reset,
                footer: Color::Rgb(231, 220, 200),
                search: Color::Rgb(241, 199, 91),
                waiting: Color::Rgb(217, 164, 65),
                running: Color::Rgb(163, 177, 138),
                stopping: Color::Rgb(217, 130, 87),
                completed: Color::Rgb(201, 166, 107),
                failed: Color::Rgb(198, 93, 75),
            }),
            "nord" => Some(Self {
                accent: Color::Rgb(136, 192, 208),
                accent_text: Color::Rgb(46, 52, 64),
                muted: Color::Rgb(76, 86, 106),
                text: Color::Reset,
                footer: Color::Rgb(216, 222, 233),
                search: Color::Rgb(235, 203, 139),
                waiting: Color::Rgb(235, 203, 139),
                running: Color::Rgb(163, 190, 140),
                stopping: Color::Rgb(208, 135, 112),
                completed: Color::Rgb(136, 192, 208),
                failed: Color::Rgb(191, 97, 106),
            }),
            "gruvbox" => Some(Self {
                accent: Color::Rgb(131, 165, 152),
                accent_text: Color::Rgb(40, 40, 40),
                muted: Color::Rgb(146, 131, 116),
                text: Color::Reset,
                footer: Color::Rgb(213, 196, 161),
                search: Color::Rgb(250, 189, 47),
                waiting: Color::Rgb(250, 189, 47),
                running: Color::Rgb(184, 187, 38),
                stopping: Color::Rgb(254, 128, 25),
                completed: Color::Rgb(131, 165, 152),
                failed: Color::Rgb(251, 73, 52),
            }),
            "dracula" => Some(Self {
                accent: Color::Rgb(189, 147, 249),
                accent_text: Color::Rgb(40, 42, 54),
                muted: Color::Rgb(98, 114, 164),
                text: Color::Reset,
                footer: Color::Rgb(248, 248, 242),
                search: Color::Rgb(241, 250, 140),
                waiting: Color::Rgb(241, 250, 140),
                running: Color::Rgb(80, 250, 123),
                stopping: Color::Rgb(255, 184, 108),
                completed: Color::Rgb(139, 233, 253),
                failed: Color::Rgb(255, 85, 85),
            }),
            "catppuccin" => Some(Self {
                accent: Color::Rgb(203, 166, 247),
                accent_text: Color::Rgb(17, 17, 27),
                muted: Color::Rgb(108, 112, 134),
                text: Color::Reset,
                footer: Color::Rgb(205, 214, 244),
                search: Color::Rgb(249, 226, 175),
                waiting: Color::Rgb(250, 179, 135),
                running: Color::Rgb(166, 227, 161),
                stopping: Color::Rgb(249, 226, 175),
                completed: Color::Rgb(116, 199, 236),
                failed: Color::Rgb(243, 139, 168),
            }),
            "tokyo-night" => Some(Self {
                accent: Color::Rgb(122, 162, 247),
                accent_text: Color::Rgb(26, 27, 38),
                muted: Color::Rgb(86, 95, 137),
                text: Color::Reset,
                footer: Color::Rgb(192, 202, 245),
                search: Color::Rgb(224, 175, 104),
                waiting: Color::Rgb(224, 175, 104),
                running: Color::Rgb(158, 206, 106),
                stopping: Color::Rgb(255, 158, 100),
                completed: Color::Rgb(125, 207, 255),
                failed: Color::Rgb(247, 118, 142),
            }),
            "solarized-dark" => Some(Self {
                accent: Color::Rgb(42, 161, 152),
                accent_text: Color::Rgb(0, 43, 54),
                muted: Color::Rgb(88, 110, 117),
                text: Color::Reset,
                footer: Color::Rgb(147, 161, 161),
                search: Color::Rgb(181, 137, 0),
                waiting: Color::Rgb(181, 137, 0),
                running: Color::Rgb(133, 153, 0),
                stopping: Color::Rgb(203, 75, 22),
                completed: Color::Rgb(42, 161, 152),
                failed: Color::Rgb(220, 50, 47),
            }),
            "monochrome" => Some(Self {
                accent: Color::White,
                accent_text: Color::Black,
                muted: Color::DarkGray,
                text: Color::Reset,
                footer: Color::Gray,
                search: Color::White,
                waiting: Color::Gray,
                running: Color::White,
                stopping: Color::Gray,
                completed: Color::White,
                failed: Color::Gray,
            }),
            _ => None,
        }
    }
}

fn matrix_theme(text: Color) -> Theme {
    Theme {
        accent: Color::Rgb(0, 255, 65),
        accent_text: Color::Rgb(0, 20, 5),
        muted: Color::Rgb(22, 101, 52),
        text,
        footer: Color::Rgb(74, 222, 128),
        search: Color::Rgb(190, 242, 100),
        waiting: Color::Rgb(163, 230, 53),
        running: Color::Rgb(0, 255, 65),
        stopping: Color::Rgb(250, 204, 21),
        completed: Color::Rgb(34, 197, 94),
        failed: Color::Rgb(255, 49, 49),
    }
}

pub fn parse_color(value: &str) -> Option<Color> {
    let normalized = value.trim().to_ascii_lowercase().replace('_', "-");
    let named = match normalized.as_str() {
        "reset" | "terminal" | "default" => Some(Color::Reset),
        "black" => Some(Color::Black),
        "red" => Some(Color::Red),
        "green" => Some(Color::Green),
        "yellow" => Some(Color::Yellow),
        "blue" => Some(Color::Blue),
        "magenta" => Some(Color::Magenta),
        "cyan" => Some(Color::Cyan),
        "gray" | "grey" => Some(Color::Gray),
        "dark-gray" | "dark-grey" => Some(Color::DarkGray),
        "light-red" => Some(Color::LightRed),
        "light-green" => Some(Color::LightGreen),
        "light-yellow" => Some(Color::LightYellow),
        "light-blue" => Some(Color::LightBlue),
        "light-magenta" => Some(Color::LightMagenta),
        "light-cyan" => Some(Color::LightCyan),
        "white" => Some(Color::White),
        _ => None,
    };
    if named.is_some() {
        return named;
    }

    if let Some(hex) = normalized.strip_prefix('#')
        && hex.len() == 6
    {
        let red = u8::from_str_radix(hex.get(0..2)?, 16).ok()?;
        let green = u8::from_str_radix(hex.get(2..4)?, 16).ok()?;
        let blue = u8::from_str_radix(hex.get(4..6)?, 16).ok()?;
        return Some(Color::Rgb(red, green, blue));
    }

    normalized
        .strip_prefix("ansi:")?
        .parse::<u8>()
        .ok()
        .map(Color::Indexed)
}

#[cfg(test)]
mod tests {
    use ratatui::style::Color;

    use super::{Theme, parse_color};

    #[test]
    fn parses_named_rgb_and_indexed_colors() {
        assert_eq!(parse_color("dark_gray"), Some(Color::DarkGray));
        assert_eq!(parse_color("#88c0d0"), Some(Color::Rgb(136, 192, 208)));
        assert_eq!(parse_color("ansi:123"), Some(Color::Indexed(123)));
        assert_eq!(parse_color("not-a-color"), None);
    }

    #[test]
    fn exposes_builtin_presets() {
        for preset in super::PRESETS {
            assert!(Theme::preset(preset).is_some(), "{preset}");
        }
        assert_eq!(
            Theme::preset("MATRIX").unwrap().text,
            Color::Rgb(134, 239, 172)
        );
        assert_eq!(Theme::preset("matrix-alt").unwrap().text, Color::White);
        assert!(Theme::preset("missing").is_none());
    }
}
