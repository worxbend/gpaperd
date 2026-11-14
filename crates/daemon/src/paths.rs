//! Where gpaperd keeps its files, following the XDG base directory spec.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use gpaperd_core::style::Style;

/// Every file-system location the daemon uses. All paths are absolute,
/// except a relative `--config` override, which is kept as given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// `$XDG_CONFIG_HOME/gpaperd/config.toml`, or the `--config` override.
    pub config_file: PathBuf,
    /// `$XDG_DATA_HOME/gpaperd/slots`. Under data, not cache, so a cache
    /// cleaner cannot blank the desktop.
    pub slots_dir: PathBuf,
    /// `$XDG_STATE_HOME/gpaperd/state.json`.
    pub state_file: PathBuf,
    /// `$XDG_CACHE_HOME/gpaperd/remote`: one directory per remote URL.
    pub remote_cache_dir: PathBuf,
    /// `$HOME`. Expands `~` in the config.
    pub home_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathsError {
    #[error("HOME is not set or is not an absolute path")]
    NoHome,
}

impl Paths {
    /// Resolves all locations from an environment lookup. Pure: touches no
    /// file and creates no directory.
    ///
    /// An XDG variable that is unset, empty or not absolute is ignored, as
    /// the spec says, and its fallback is used: `$HOME/.config`,
    /// `$HOME/.local/share`, `$HOME/.local/state`, `$HOME/.cache`.
    ///
    /// # Errors
    ///
    /// [`PathsError::NoHome`] when `HOME` is unset, empty or not absolute,
    /// even if every XDG variable is set.
    pub fn resolve(
        env: &dyn Fn(&str) -> Option<OsString>,
        config_override: Option<&Path>,
    ) -> Result<Paths, PathsError> {
        let home_dir = absolute_dir(env, "HOME").ok_or(PathsError::NoHome)?;
        let app_dir = |variable: &str, fallback: &str| {
            absolute_dir(env, variable)
                .unwrap_or_else(|| home_dir.join(fallback))
                .join(APP_DIR_NAME)
        };
        let config_file = match config_override {
            Some(config_override) => config_override.to_path_buf(),
            None => app_dir("XDG_CONFIG_HOME", ".config").join("config.toml"),
        };
        Ok(Paths {
            config_file,
            slots_dir: app_dir("XDG_DATA_HOME", ".local/share").join("slots"),
            state_file: app_dir("XDG_STATE_HOME", ".local/state").join("state.json"),
            remote_cache_dir: app_dir("XDG_CACHE_HOME", ".cache").join("remote"),
            home_dir,
        })
    }

    /// The slot file of a style: `slots_dir/light` or `slots_dir/dark`, with
    /// no extension.
    pub fn slot(&self, style: Style) -> PathBuf {
        self.slots_dir.join(style.as_str())
    }
}

/// gpaperd's directory inside each XDG base directory.
const APP_DIR_NAME: &str = "gpaperd";

/// The directory a variable names, or `None` when the variable is unset,
/// empty or relative.
fn absolute_dir(env: &dyn Fn(&str) -> Option<OsString>, variable: &str) -> Option<PathBuf> {
    env(variable)
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve_with(
        vars: &[(&str, &str)],
        config_override: Option<&Path>,
    ) -> Result<Paths, PathsError> {
        let env = |name: &str| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(value))
        };
        Paths::resolve(&env, config_override)
    }

    #[test]
    fn unset_xdg_variables_fall_back_under_home() {
        let paths = resolve_with(&[("HOME", "/home/u")], None).unwrap();

        assert_eq!(
            paths,
            Paths {
                config_file: "/home/u/.config/gpaperd/config.toml".into(),
                slots_dir: "/home/u/.local/share/gpaperd/slots".into(),
                state_file: "/home/u/.local/state/gpaperd/state.json".into(),
                remote_cache_dir: "/home/u/.cache/gpaperd/remote".into(),
                home_dir: "/home/u".into(),
            }
        );
    }

    #[test]
    fn absolute_xdg_variables_replace_the_fallbacks() {
        let vars = [
            ("HOME", "/home/u"),
            ("XDG_CONFIG_HOME", "/x/config"),
            ("XDG_DATA_HOME", "/x/data"),
            ("XDG_STATE_HOME", "/x/state"),
            ("XDG_CACHE_HOME", "/x/cache"),
        ];

        let paths = resolve_with(&vars, None).unwrap();

        assert_eq!(
            paths,
            Paths {
                config_file: "/x/config/gpaperd/config.toml".into(),
                slots_dir: "/x/data/gpaperd/slots".into(),
                state_file: "/x/state/gpaperd/state.json".into(),
                remote_cache_dir: "/x/cache/gpaperd/remote".into(),
                home_dir: "/home/u".into(),
            }
        );
    }

    #[test]
    fn empty_or_relative_xdg_variables_are_ignored() {
        let vars = [
            ("HOME", "/home/u"),
            ("XDG_CONFIG_HOME", ""),
            ("XDG_DATA_HOME", "relative/data"),
            ("XDG_STATE_HOME", "./state"),
            ("XDG_CACHE_HOME", "cache"),
        ];

        let paths = resolve_with(&vars, None).unwrap();

        assert_eq!(paths, resolve_with(&[("HOME", "/home/u")], None).unwrap());
    }

    #[test]
    fn config_override_replaces_only_the_config_file() {
        let vars = [("HOME", "/home/u"), ("XDG_CONFIG_HOME", "/x/config")];
        let defaults = resolve_with(&vars, None).unwrap();

        let paths = resolve_with(&vars, Some(Path::new("conf/dev.toml"))).unwrap();

        assert_eq!(
            paths,
            Paths {
                config_file: "conf/dev.toml".into(),
                ..defaults
            }
        );
    }

    #[test]
    fn unset_home_is_an_error_even_with_every_xdg_variable_set() {
        let vars = [
            ("XDG_CONFIG_HOME", "/x/config"),
            ("XDG_DATA_HOME", "/x/data"),
            ("XDG_STATE_HOME", "/x/state"),
        ];

        assert_eq!(resolve_with(&vars, None), Err(PathsError::NoHome));
    }

    #[test]
    fn empty_home_is_an_error() {
        assert_eq!(resolve_with(&[("HOME", "")], None), Err(PathsError::NoHome));
    }

    #[test]
    fn relative_home_is_an_error() {
        assert_eq!(
            resolve_with(&[("HOME", "home/u")], None),
            Err(PathsError::NoHome)
        );
    }
}
