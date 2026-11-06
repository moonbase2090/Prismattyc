/// `ENABLE_VIRTUAL_TERMINAL_PROCESSING`.
pub const VT_OUTPUT_PROCESSING: u32 = 0x0004;
/// `DISABLE_NEWLINE_AUTO_RETURN`.
pub const VT_DISABLE_NEWLINE_AUTO_RETURN: u32 = 0x0008;
/// `ENABLE_VIRTUAL_TERMINAL_INPUT`.
pub const VT_INPUT: u32 = 0x0200;

pub fn console_vt_output_flags() -> u32 {
    VT_OUTPUT_PROCESSING
}

pub fn console_vt_raw_output_flags() -> u32 {
    VT_OUTPUT_PROCESSING | VT_DISABLE_NEWLINE_AUTO_RETURN
}

pub fn console_vt_input_flags() -> u32 {
    VT_INPUT
}

/// Line-oriented CLI output. Virtual-terminal processing stays on, and a
/// newline still returns to column 0. A console that already disabled
/// auto-return is corrected for this process.
pub fn cli_console_output_mode(mode: u32) -> u32 {
    (mode | VT_OUTPUT_PROCESSING) & !VT_DISABLE_NEWLINE_AUTO_RETURN
}

/// Full-screen attach. A bare line feed moves down without returning to
/// column 0, matching the stream the pane writes. Drop restores the mode
/// captured before this mask was applied.
pub fn raw_console_output_mode(mode: u32) -> u32 {
    mode | console_vt_raw_output_flags()
}

/// Console modes owned by one CLI invocation. Drop after any nested terminal
/// session has restored its own modes.
#[must_use]
pub struct ConsoleVt {
    stdout_ready: bool,
    #[cfg(windows)]
    modes: [Option<(windows_sys::Win32::Foundation::HANDLE, u32)>; 3],
}

impl ConsoleVt {
    pub fn stdout_ready(&self) -> bool {
        self.stdout_ready
    }
}

/// Enable VT for this invocation, preserving the caller's console modes.
pub fn enter_console_vt() -> ConsoleVt {
    #[cfg(not(windows))]
    {
        ConsoleVt { stdout_ready: true }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::{
            Foundation::INVALID_HANDLE_VALUE,
            System::Console::{
                GetConsoleMode, GetStdHandle, SetConsoleMode, STD_ERROR_HANDLE, STD_INPUT_HANDLE,
                STD_OUTPUT_HANDLE,
            },
        };
        // Stdout and stderr can name the same screen buffer. Capture every
        // original mode before changing any of them.
        let modes = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE].map(|id| unsafe {
            let handle = GetStdHandle(id);
            let mut mode = 0;
            if handle.is_null()
                || handle == INVALID_HANDLE_VALUE
                || GetConsoleMode(handle, &mut mode) == 0
            {
                None
            } else {
                Some((handle, mode))
            }
        });
        let mut stdout_ready = false;
        for (index, saved) in modes.iter().enumerate() {
            if let Some((handle, mode)) = saved {
                let enabled = if index == 0 {
                    mode | console_vt_input_flags()
                } else {
                    cli_console_output_mode(*mode)
                };
                let accepted = unsafe { SetConsoleMode(*handle, enabled) != 0 };
                if index == 1 {
                    stdout_ready = accepted;
                }
            }
        }
        ConsoleVt {
            stdout_ready,
            modes,
        }
    }
}

#[cfg(windows)]
impl Drop for ConsoleVt {
    fn drop(&mut self) {
        for (handle, mode) in self.modes.iter().flatten() {
            unsafe {
                windows_sys::Win32::System::Console::SetConsoleMode(*handle, *mode);
            }
        }
    }
}

#[cfg(any(windows, test))]
fn console_mouse_input_mode(mode: u32) -> u32 {
    const NATIVE_MOUSE: u32 = 0x0010 | 0x0008 | 0x0080;
    const QUICK_EDIT: u32 = 0x0040;
    (mode | NATIVE_MOUSE) & !(QUICK_EDIT | VT_INPUT)
}

/// Win32 input records for an interactive view. Enter after raw mode so
/// ConPTY registers mouse capture; drop before the terminal session restores.
#[cfg(windows)]
#[must_use]
pub struct ConsoleMouseInput {
    handle: windows_sys::Win32::Foundation::HANDLE,
    mode: u32,
}

#[cfg(windows)]
impl ConsoleMouseInput {
    pub fn enter() -> std::io::Result<Self> {
        use windows_sys::Win32::{
            Foundation::INVALID_HANDLE_VALUE,
            System::Console::{GetConsoleMode, GetStdHandle, SetConsoleMode, STD_INPUT_HANDLE},
        };
        let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "interactive view requires console stdin",
            ));
        }
        let mut mode = 0;
        if unsafe { GetConsoleMode(handle, &mut mode) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        if unsafe { SetConsoleMode(handle, console_mouse_input_mode(mode)) } == 0 {
            let error = std::io::Error::last_os_error();
            unsafe { SetConsoleMode(handle, mode) };
            return Err(error);
        }
        Ok(Self { handle, mode })
    }
}

#[cfg(windows)]
impl Drop for ConsoleMouseInput {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::System::Console::SetConsoleMode(self.handle, self.mode) };
    }
}

#[cfg(test)]
mod tests {
    use super::console_mouse_input_mode;

    #[test]
    fn native_mouse_input_replaces_vt_input_and_quick_edit() {
        assert_eq!(console_mouse_input_mode(0x0240), 0x0098);
    }

    #[test]
    fn native_mouse_input_preserves_unrelated_console_flags() {
        assert_eq!(console_mouse_input_mode(0x0220), 0x00b8);
    }
}
