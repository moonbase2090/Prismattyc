#[allow(dead_code)]
#[path = "../../../crates/prismattyc-mux/src/platform/console_vt.rs"]
mod console_vt;

#[cfg(windows)]
mod windows {
    use std::{io, os::windows::process::CommandExt, process::Command};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE},
        Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING},
        System::Console::{
            AllocConsole, FreeConsole, GetConsoleMode, GetStdHandle, SetConsoleMode, SetStdHandle,
            DISABLE_NEWLINE_AUTO_RETURN, ENABLE_VIRTUAL_TERMINAL_INPUT,
            ENABLE_VIRTUAL_TERMINAL_PROCESSING, STD_ERROR_HANDLE, STD_INPUT_HANDLE,
            STD_OUTPUT_HANDLE,
        },
    };

    const IDS: [u32; 3] = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE];

    struct Console {
        previous: [HANDLE; 3],
        handles: Vec<HANDLE>,
    }

    impl Drop for Console {
        fn drop(&mut self) {
            unsafe {
                for (id, handle) in IDS.into_iter().zip(self.previous) {
                    SetStdHandle(id, handle);
                }
                for handle in &self.handles {
                    CloseHandle(*handle);
                }
                FreeConsole();
            }
        }
    }

    fn check(success: i32) -> io::Result<()> {
        if success == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn isolated_console() -> io::Result<Console> {
        let previous = IDS.map(|id| unsafe { GetStdHandle(id) });
        unsafe { check(AllocConsole())? };
        let mut console = Console {
            previous,
            handles: Vec::new(),
        };
        for name in ["CONIN$", "CONOUT$"] {
            let wide: Vec<_> = name.encode_utf16().chain(Some(0)).collect();
            let handle = unsafe {
                CreateFileW(
                    wide.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    0,
                    std::ptr::null_mut(),
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }
            console.handles.push(handle);
        }
        // Deliberately alias stdout/stderr, as a normal Windows console does.
        for (id, handle) in
            IDS.into_iter()
                .zip([console.handles[0], console.handles[1], console.handles[1]])
        {
            unsafe { check(SetStdHandle(id, handle))? };
        }
        Ok(console)
    }

    fn modes() -> io::Result<[u32; 3]> {
        let mut modes = [0; 3];
        for (id, mode) in IDS.into_iter().zip(&mut modes) {
            unsafe { check(GetConsoleMode(GetStdHandle(id), mode))? };
        }
        Ok(modes)
    }

    fn probe() -> io::Result<()> {
        let _console = isolated_console()?;
        let initial = modes()?;
        for input_vt in [false, true] {
            let input = if input_vt {
                initial[0] | ENABLE_VIRTUAL_TERMINAL_INPUT
            } else {
                initial[0] & !ENABLE_VIRTUAL_TERMINAL_INPUT
            };
            let output =
                (initial[1] | DISABLE_NEWLINE_AUTO_RETURN) & !ENABLE_VIRTUAL_TERMINAL_PROCESSING;
            unsafe {
                check(SetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), input))?;
                check(SetConsoleMode(GetStdHandle(STD_OUTPUT_HANDLE), output))?;
            }
            let before = modes()?;
            {
                let guard = super::console_vt::enter_console_vt();
                assert!(guard.stdout_ready());
                let active = modes()?;
                assert_eq!(active[0], input | ENABLE_VIRTUAL_TERMINAL_INPUT);
                assert_eq!(
                    active[1],
                    (output | ENABLE_VIRTUAL_TERMINAL_PROCESSING) & !DISABLE_NEWLINE_AUTO_RETURN
                );
                assert_eq!(active[1], active[2]);
            }
            assert_eq!(
                modes()?,
                before,
                "console mode leaked for input_vt={input_vt}"
            );
        }
        Ok(())
    }

    pub fn run() -> io::Result<()> {
        if std::env::args().any(|arg| arg == "--isolated-console") {
            probe()?;
            println!(
                "PASS original console modes restored with aliased outputs and VT input off/on"
            );
            return Ok(());
        }
        // Test only in a private child console, never in the CI runner's console.
        let output = Command::new(std::env::current_exe()?)
            .arg("--isolated-console")
            .creation_flags(0x0000_0008) // DETACHED_PROCESS
            .output()?;
        print!("{}", String::from_utf8_lossy(&output.stdout));
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
        assert!(output.status.success(), "isolated console probe failed");
        Ok(())
    }
}

fn main() -> std::io::Result<()> {
    #[cfg(windows)]
    windows::run()?;
    #[cfg(not(windows))]
    assert!(console_vt::enter_console_vt().stdout_ready());
    Ok(())
}
