//! Preserve Rust panic diagnostics when a GUI launch has no visible stderr.
use std::backtrace::Backtrace;
use std::io::{self, Write};
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn install(runtime: &Path) -> io::Result<()> {
    prismattyc_mux::private_fs::create_dir(runtime)?;
    let file = Mutex::new(prismattyc_mux::private_fs::append(
        &runtime.join("host-panic.log"),
    )?);
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let thread = std::thread::current();
        let backtrace = Backtrace::force_capture();
        let record = format!(
            "\nprismattyc-host {} pid={} unix_time={} thread={}\n{info}\nBacktrace:\n{backtrace}\n",
            env!("CARGO_PKG_VERSION"),
            std::process::id(),
            timestamp,
            thread.name().unwrap_or("unnamed"),
        );
        let result = (|| -> io::Result<()> {
            let mut file = file.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            file.write_all(record.as_bytes())?;
            file.flush()?;
            file.sync_data()
        })();
        if let Err(error) = result {
            let _ = writeln!(
                io::stderr().lock(),
                "prismattyc-host: could not persist panic: {error}"
            );
        }
        previous(info);
    }));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn panic_child() {
        let Some(directory) = std::env::var_os("PRISMATTYC_PANIC_TEST_DIR") else {
            return;
        };
        install(Path::new(&directory)).unwrap();
        panic!("host panic persistence proof");
    }

    #[test]
    fn persists_real_panics_and_backtraces_across_restarts() {
        let runtime = std::env::temp_dir().join(format!(
            "host-panic-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&runtime).unwrap();
        let path = runtime.join("host-panic.log");
        std::fs::write(&path, "previous launch\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        for _ in 0..2 {
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "panic_log::tests::panic_child", "--nocapture"])
                .env("PRISMATTYC_PANIC_TEST_DIR", &runtime)
                .env("RUST_BACKTRACE", "0")
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(101));
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("host panic persistence proof")
            );
        }
        let log = std::fs::read_to_string(&path).unwrap();
        assert!(log.starts_with("previous launch\n"));
        assert_eq!(log.matches("host panic persistence proof").count(), 2);
        assert_eq!(log.lines().filter(|line| *line == "Backtrace:").count(), 2);
        assert!(log.contains("panic_log::tests::panic_child"));
        assert!(log.contains("panic_log.rs:"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&runtime).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        std::fs::remove_dir_all(runtime).unwrap();
    }
}
