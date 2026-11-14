//! Light/dark style and a container holding one value per style.

use std::fmt;
use std::ops::{Index, IndexMut};

use serde::{Deserialize, Serialize};

/// GNOME appearance style. Each style has its own slot file, catalog and clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Style {
    Light,
    Dark,
}

impl Style {
    /// Both styles, light first.
    pub const ALL: [Style; 2] = [Style::Light, Style::Dark];

    /// The style gnome-shell shows for a `color-scheme` value.
    ///
    /// Dark only for `prefer-dark`. `default`, `prefer-light` and any unknown
    /// value mean light (gnome-shell 51.1 `js/ui/background.js` 749-757).
    pub fn from_color_scheme(color_scheme: &str) -> Style {
        if color_scheme == "prefer-dark" {
            Style::Dark
        } else {
            Style::Light
        }
    }

    /// The opposite style.
    pub const fn other(self) -> Style {
        match self {
            Style::Light => Style::Dark,
            Style::Dark => Style::Light,
        }
    }

    /// `"light"` or `"dark"`: the config table name and the slot file name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Style::Light => "light",
            Style::Dark => "dark",
        }
    }
}

impl fmt::Display for Style {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One value per style. Index it with a [`Style`].
///
/// Serialises as `{ "light": …, "dark": … }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PerStyle<T> {
    pub light: T,
    pub dark: T,
}

impl<T> PerStyle<T> {
    /// Builds both values from the style, light first.
    pub fn from_fn(mut value_for: impl FnMut(Style) -> T) -> PerStyle<T> {
        PerStyle {
            light: value_for(Style::Light),
            dark: value_for(Style::Dark),
        }
    }
}

impl<T> Index<Style> for PerStyle<T> {
    type Output = T;

    fn index(&self, style: Style) -> &T {
        match style {
            Style::Light => &self.light,
            Style::Dark => &self.dark,
        }
    }
}

impl<T> IndexMut<Style> for PerStyle<T> {
    fn index_mut(&mut self, style: Style) -> &mut T {
        match style {
            Style::Light => &mut self.light,
            Style::Dark => &mut self.dark,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_prefer_dark_is_the_dark_style() {
        assert_eq!(Style::from_color_scheme("prefer-dark"), Style::Dark);
    }

    #[test]
    fn default_prefer_light_and_unknown_values_are_the_light_style() {
        for color_scheme in ["default", "prefer-light", "", "dark", "Prefer-Dark"] {
            assert_eq!(
                Style::from_color_scheme(color_scheme),
                Style::Light,
                "{color_scheme:?}"
            );
        }
    }
}
