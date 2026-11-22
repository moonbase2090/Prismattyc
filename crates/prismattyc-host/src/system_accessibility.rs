//! The operating system's reduce-motion and reduce-transparency choices.
//!
//! The modal scrim asks for them when a modal opens. Reduce motion makes the
//! scrim appear at once; reduce transparency makes it dim without blurring.
//! A setting the platform cannot report counts as off.
//!
//! - macOS: Reduce motion and Reduce transparency in Accessibility > Display.
//! - Windows: "Animation effects" (client-area animation) and "Transparency
//!   effects" under Personalization > Colors.
//! - Linux: GNOME's `enable-animations` for motion; its high-contrast theme
//!   for transparency, since GNOME has no transparency switch of its own.

/// What the system asks apps to tone down.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Preferences {
    pub reduce_motion: bool,
    pub reduce_transparency: bool,
}

/// Read the current system choices.
pub(crate) fn read() -> Preferences {
    platform::read()
}

/// GNOME `enable-animations` is `true` or `false`; animations off means the
/// user wants reduced motion. A missing or unreadable value leaves it off.
#[cfg(any(test, not(any(target_os = "macos", windows))))]
pub(crate) fn linux_preferences(
    animations: Option<&str>,
    high_contrast: Option<&str>,
) -> Preferences {
    Preferences {
        reduce_motion: animations.is_some_and(|v| v.trim() == "false"),
        reduce_transparency: high_contrast.is_some_and(|v| v.trim() == "true"),
    }
}

/// Windows reports animation effects as a BOOL (on is true) and transparency
/// effects as the `EnableTransparency` DWORD (1 is on). A missing value
/// leaves the matching reduction off.
#[cfg(any(test, windows))]
pub(crate) fn windows_preferences(
    animations: Option<bool>,
    transparency: Option<u32>,
) -> Preferences {
    Preferences {
        reduce_motion: animations == Some(false),
        reduce_transparency: transparency == Some(0),
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::Preferences;
    use objc2_app_kit::NSWorkspace;

    pub(super) fn read() -> Preferences {
        let workspace = NSWorkspace::sharedWorkspace();
        Preferences {
            reduce_motion: workspace.accessibilityDisplayShouldReduceMotion(),
            reduce_transparency: workspace.accessibilityDisplayShouldReduceTransparency(),
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::{windows_preferences, Preferences};
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SystemParametersInfoW, SPI_GETCLIENTAREAANIMATION,
    };

    fn animations() -> Option<bool> {
        let mut enabled: i32 = 0;
        // SAFETY: the out pointer is a live BOOL and the action takes no size.
        let ok = unsafe {
            SystemParametersInfoW(
                SPI_GETCLIENTAREAANIMATION,
                0,
                std::ptr::addr_of_mut!(enabled).cast(),
                0,
            )
        };
        (ok != 0).then_some(enabled != 0)
    }

    fn transparency() -> Option<u32> {
        let key: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize\0"
            .encode_utf16()
            .collect();
        let name: Vec<u16> = "EnableTransparency\0".encode_utf16().collect();
        let mut value: u32 = 0;
        let mut size = std::mem::size_of::<u32>() as u32;
        // SAFETY: both names are NUL-terminated and the data pointer is a
        // live DWORD of the size passed.
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_DWORD,
                std::ptr::null_mut(),
                std::ptr::addr_of_mut!(value).cast(),
                &mut size,
            )
        };
        (status == 0).then_some(value)
    }

    pub(super) fn read() -> Preferences {
        windows_preferences(animations(), transparency())
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
mod platform {
    use super::{linux_preferences, Preferences};

    fn gsettings(schema: &str, key: &str) -> Option<String> {
        let output = prismattyc_process::spawn(
            prismattyc_process::capture_command("gsettings").args(["get", schema, key]),
        )
        .and_then(std::process::Child::wait_with_output)
        .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
    }

    pub(super) fn read() -> Preferences {
        linux_preferences(
            gsettings("org.gnome.desktop.interface", "enable-animations").as_deref(),
            gsettings("org.gnome.desktop.a11y.interface", "high-contrast").as_deref(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_animations_off_asks_for_reduced_motion() {
        assert_eq!(
            linux_preferences(Some("false\n"), Some("false\n")),
            Preferences {
                reduce_motion: true,
                reduce_transparency: false
            }
        );
        assert!(!linux_preferences(Some("true\n"), None).reduce_motion);
    }

    #[test]
    fn linux_high_contrast_asks_for_reduced_transparency() {
        assert_eq!(
            linux_preferences(Some("true"), Some("true")),
            Preferences {
                reduce_motion: false,
                reduce_transparency: true
            }
        );
    }

    #[test]
    fn an_unreadable_linux_setting_changes_nothing() {
        assert_eq!(linux_preferences(None, None), Preferences::default());
    }

    #[test]
    fn windows_animation_and_transparency_off_ask_for_reductions() {
        assert_eq!(
            windows_preferences(Some(false), Some(0)),
            Preferences {
                reduce_motion: true,
                reduce_transparency: true
            }
        );
        assert_eq!(
            windows_preferences(Some(true), Some(1)),
            Preferences::default()
        );
    }

    #[test]
    fn a_missing_windows_value_changes_nothing() {
        assert_eq!(windows_preferences(None, None), Preferences::default());
    }
}
