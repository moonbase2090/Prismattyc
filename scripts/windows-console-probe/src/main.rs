#[allow(dead_code)]
#[path = "../../../crates/prismattyc-mux/src/platform/console_vt.rs"]
mod console_vt;

#[cfg(windows)]
mod windows {
    use std::{fs, io, os::windows::process::CommandExt, path::PathBuf, process::Command};
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

    struct Scratch(PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn curl_resolution() -> io::Result<()> {
        let directory = Scratch(std::env::temp_dir().join(format!(
            "pmux-curl-probe-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        )));
        fs::create_dir(&directory.0)?;
        let tools = directory.0.join("tools");
        fs::create_dir(&tools)?;
        let executable = std::env::current_exe()?;
        let caller = directory.0.join("caller.exe");
        fs::copy(&executable, &caller)?;
        fs::copy(&executable, tools.join("curl.exe"))?;
        let path = std::env::join_paths(std::iter::once(tools).chain(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        )))
        .unwrap();
        let call = || {
            Command::new(&caller)
                .arg("--spawn-curl")
                .env("PATH", &path)
                .output()
        };
        let inherited_path = call()?;
        assert!(inherited_path.status.success());
        let fixture = b"pmux-private-curl-fixture\n";
        let path_selected_fixture = inherited_path.stdout == fixture;
        fs::copy(&executable, directory.0.join("curl.exe"))?;
        let beside_executable = call()?;
        assert!(beside_executable.status.success());
        assert_eq!(beside_executable.stdout, fixture);
        println!("PASS curl beside executable selected; inherited PATH selected fixture={path_selected_fixture}");
        Ok(())
    }

    pub fn run() -> io::Result<()> {
        if std::env::current_exe()?
            .file_stem()
            .is_some_and(|name| name == "curl")
        {
            println!("pmux-private-curl-fixture");
            return Ok(());
        }
        if std::env::args().any(|arg| arg == "--spawn-curl") {
            let output = Command::new("curl").arg("--version").output()?;
            use std::io::Write;
            io::stdout().write_all(&output.stdout)?;
            assert!(output.status.success());
            return Ok(());
        }
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
        curl_resolution()?;
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
