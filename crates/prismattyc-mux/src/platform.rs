//! OS operations shared by the daemon, clients and host.
use std::{fs, io, path::Path};

pub fn lock_exclusive(file: &fs::File) -> io::Result<()> {
    file.lock()
}
pub fn try_lock_exclusive(file: &fs::File) -> io::Result<()> {
    file.try_lock().map_err(|e| match e {
        fs::TryLockError::WouldBlock => io::Error::from(io::ErrorKind::WouldBlock),
        fs::TryLockError::Error(e) => e,
    })
}
pub fn unlock(file: &fs::File) -> io::Result<()> {
    file.unlock()
}

pub fn private_options() -> fs::OpenOptions {
    #[allow(unused_mut)]
    let mut options = fs::OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

#[cfg(unix)]
pub fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(windows)]
pub use windows::{
    owned_socket, process_alive, require_private_directory, secure_directory, set_mode,
    user_directory,
};
#[cfg(windows)]
mod windows {
    use super::*;
    use std::{
        os::windows::{ffi::OsStrExt, fs::MetadataExt},
        ptr,
    };
    use windows_sys::Win32::{
        Foundation::*,
        Security::{Authorization::*, *},
        System::Threading::*,
    };
    fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
        s.encode_wide().chain(Some(0)).collect()
    }
    struct Local(*mut core::ffi::c_void);
    impl Drop for Local {
        fn drop(&mut self) {
            unsafe {
                LocalFree(self.0);
            }
        }
    }
    struct Handle(HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    fn user_sid() -> io::Result<String> {
        unsafe {
            let mut token = ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return Err(io::Error::last_os_error());
            }
            let token = Handle(token);
            let mut len = 0;
            GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut len);
            if len == 0 {
                return Err(io::Error::last_os_error());
            }
            // Allocate pointer-aligned storage for TOKEN_USER and its trailing SID.
            let mut storage = vec![0usize; (len as usize).div_ceil(std::mem::size_of::<usize>())];
            if GetTokenInformation(
                token.0,
                TokenUser,
                storage.as_mut_ptr().cast(),
                len,
                &mut len,
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            let user = &*storage.as_ptr().cast::<TOKEN_USER>();
            let mut text = ptr::null_mut();
            if ConvertSidToStringSidW(user.User.Sid, &mut text) == 0 {
                return Err(io::Error::last_os_error());
            }
            let _guard = Local(text.cast());
            let mut n = 0;
            while *text.add(n) != 0 {
                n += 1;
            }
            Ok(String::from_utf16_lossy(std::slice::from_raw_parts(
                text, n,
            )))
        }
    }
    pub fn set_mode(path: &Path, _mode: u32) -> io::Result<()> {
        // Windows grants the current user full control and protects the DACL
        // from inherited access. Directory ACEs propagate to children.
        let sid = user_sid()?;
        let inherit = if path.is_dir() { "OICI" } else { "" };
        let sddl = wide(std::ffi::OsStr::new(&format!(
            "D:P(A;{inherit};FA;;;{sid})"
        )));
        unsafe {
            let mut sd = ptr::null_mut();
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                ptr::null_mut(),
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            let _guard = Local(sd);
            let mut present = 0;
            let mut defaulted = 0;
            let mut acl = ptr::null_mut();
            if GetSecurityDescriptorDacl(sd, &mut present, &mut acl, &mut defaulted) == 0
                || present == 0
            {
                return Err(io::Error::last_os_error());
            }
            let code = SetNamedSecurityInfoW(
                wide(path.as_os_str()).as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                acl,
                ptr::null_mut(),
            );
            if code != 0 {
                return Err(io::Error::from_raw_os_error(code as i32));
            }
        }
        Ok(())
    }
    pub fn user_directory() -> io::Result<std::path::PathBuf> {
        let base = std::env::var_os("LOCALAPPDATA")
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "LOCALAPPDATA is unset"))?;
        let dir = std::path::PathBuf::from(base)
            .join("Prismattyc")
            .join("run");
        if !dir.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "LOCALAPPDATA must be absolute",
            ));
        }
        fs::create_dir_all(&dir)?;
        secure_directory(&dir)?;
        Ok(dir)
    }
    fn directory_security(path: &Path, require_private: bool) -> io::Result<()> {
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        // Refuse redirects at every existing component before touching a DACL.
        for ancestor in path.ancestors() {
            if ancestor.as_os_str().is_empty() {
                continue;
            }
            if fs::symlink_metadata(ancestor)?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "runtime directory contains a reparse point",
                ));
            }
        }
        if !path.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "runtime path is not a directory",
            ));
        }
        unsafe {
            let mut owner = ptr::null_mut();
            let mut acl = ptr::null_mut();
            let mut sd = ptr::null_mut();
            let code = GetNamedSecurityInfoW(
                wide(path.as_os_str()).as_ptr(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                ptr::null_mut(),
                &mut acl,
                ptr::null_mut(),
                &mut sd,
            );
            if code != 0 {
                return Err(io::Error::from_raw_os_error(code as i32));
            }
            let _guard = Local(sd);
            let sid = user_sid()?;
            let mut expected = ptr::null_mut();
            if ConvertStringSidToSidW(wide(std::ffi::OsStr::new(&sid)).as_ptr(), &mut expected) == 0
            {
                return Err(io::Error::last_os_error());
            }
            let _sid = Local(expected);
            if owner.is_null() || EqualSid(owner, expected) == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "runtime directory belongs to another user",
                ));
            }
            if !require_private {
                return Ok(());
            }
            // Exactly one inheritable owner ACE: socket files are private from
            // creation, including the interval before their explicit DACL is set.
            if acl.is_null() || (*acl).AceCount != 1 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "socket parent requires a private inheritable user DACL",
                ));
            }
            let mut ace = ptr::null_mut();
            if GetAce(acl, 0, &mut ace) == 0 {
                return Err(io::Error::last_os_error());
            }
            let header = &*ace.cast::<ACE_HEADER>();
            if header.AceType != 0 /* ACCESS_ALLOWED_ACE_TYPE */ || u32::from(header.AceFlags) & (OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE) != (OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE)
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "socket parent requires an inheritable user allow ACE",
                ));
            }
            let allowed = &*ace.cast::<ACCESS_ALLOWED_ACE>();
            if EqualSid(
                (&allowed.SidStart as *const u32).cast_mut().cast(),
                expected,
            ) == 0
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "socket parent permits another user",
                ));
            }
        }
        Ok(())
    }
    /// Only apply a private DACL to an owned application directory, never a redirect.
    pub fn secure_directory(path: &Path) -> io::Result<()> {
        directory_security(path, false)?;
        set_mode(path, 0o700)?;
        require_private_directory(path)
    }
    pub fn require_private_directory(path: &Path) -> io::Result<()> {
        directory_security(path, true)
    }
    pub fn owned_socket(path: &Path) -> bool {
        let Ok(meta) = fs::symlink_metadata(path) else {
            return false;
        };
        // AF_UNIX endpoints are IO_REPARSE_TAG_AF_UNIX, not ordinary files.
        use std::os::windows::fs::OpenOptionsExt;
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::*;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
            return false;
        }
        let Ok(file) = fs::OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES | READ_CONTROL)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)
        else {
            return false;
        };
        unsafe {
            let mut info: FILE_ATTRIBUTE_TAG_INFO = std::mem::zeroed();
            if GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileAttributeTagInfo,
                (&mut info as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
                std::mem::size_of_val(&info) as u32,
            ) == 0
                || info.ReparseTag != 0x80000023
            {
                return false;
            }
            let mut owner = ptr::null_mut();
            let mut sd = ptr::null_mut();
            let code = GetNamedSecurityInfoW(
                wide(path.as_os_str()).as_ptr(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION,
                &mut owner,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                &mut sd,
            );
            if code != 0 {
                return false;
            }
            let _guard = Local(sd);
            let Ok(sid) = user_sid() else {
                return false;
            };
            let mut expected = ptr::null_mut();
            if ConvertStringSidToSidW(wide(std::ffi::OsStr::new(&sid)).as_ptr(), &mut expected) == 0
            {
                return false;
            }
            let _sid = Local(expected);
            EqualSid(owner, expected) != 0
        }
    }
    pub fn process_alive(pid: u32) -> bool {
        if pid == 0 {
            return false;
        }
        unsafe {
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if process.is_null() {
                return GetLastError() == ERROR_ACCESS_DENIED;
            }
            let process = Handle(process);
            let mut exit = 0;
            GetExitCodeProcess(process.0, &mut exit) != 0 && exit == STILL_ACTIVE as u32
        }
    }
}

#[cfg(unix)]
pub use std::os::unix::process::CommandExt as Exec;
#[cfg(windows)]
pub trait Exec {
    fn exec(&mut self) -> io::Error;
}
#[cfg(windows)]
impl Exec for std::process::Command {
    fn exec(&mut self) -> io::Error {
        match self.status() {
            Ok(status) => std::process::exit(status.code().unwrap_or(1)),
            Err(error) => error,
        }
    }
}
#[cfg(unix)]
pub use rustix::process::Signal;
#[cfg(windows)]
#[derive(Clone, Copy)]
pub enum Signal {
    TERM,
    KILL,
}
#[cfg(windows)]
pub fn terminate_process(pid: u32) -> io::Result<()> {
    use windows_sys::Win32::{Foundation::*, System::Threading::*};
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let result = TerminateProcess(handle, 1);
        let error = io::Error::last_os_error();
        CloseHandle(handle);
        if result == 0 {
            Err(error)
        } else {
            Ok(())
        }
    }
}
/// `CREATE_NO_WINDOW`. A GUI parent that starts a console child without this
/// flag gets a new console window.
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// `ENABLE_VIRTUAL_TERMINAL_PROCESSING`.
pub const VT_OUTPUT_PROCESSING: u32 = 0x0004;
/// `DISABLE_NEWLINE_AUTO_RETURN`.
pub const VT_DISABLE_NEWLINE_AUTO_RETURN: u32 = 0x0008;
/// `ENABLE_VIRTUAL_TERMINAL_INPUT`.
pub const VT_INPUT: u32 = 0x0200;

/// Flags for a short-lived helper. `CommandExt::creation_flags` replaces the
/// whole mask, and Windows ignores `CREATE_NO_WINDOW` when it is combined
/// with `DETACHED_PROCESS`, so this must not be applied to [`spawn_detached`].
pub fn helper_creation_flags() -> u32 {
    CREATE_NO_WINDOW
}

/// `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP`.
pub fn detach_creation_flags() -> u32 {
    0x0000_0008 | 0x0000_0200
}

pub fn console_vt_output_flags() -> u32 {
    VT_OUTPUT_PROCESSING | VT_DISABLE_NEWLINE_AUTO_RETURN
}

pub fn console_vt_input_flags() -> u32 {
    VT_INPUT
}

/// Start `program` without allocating a console window on Windows.
pub fn hidden_command(program: impl AsRef<std::ffi::OsStr>) -> std::process::Command {
    let mut command = std::process::Command::new(program);
    hide_console(&mut command);
    command
}

/// Hide a console child's window. No effect on Unix. Do not call this on a
/// command that then goes through [`detach_command`]: the later call replaces
/// these flags.
pub fn hide_console(command: &mut std::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(helper_creation_flags());
    }
    #[cfg(not(windows))]
    {
        let _ = command;
    }
}

/// Enable virtual-terminal sequences on a console CLI.
///
/// Returns true when stdout or stderr accepted output processing. A pipe, a
/// file, or a rejected mode returns false. Unix terminals already interpret
/// the sequences, so this reports ready there.
pub fn enable_console_vt() -> bool {
    #[cfg(not(windows))]
    {
        true
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::System::Console::{
            GetConsoleMode, GetStdHandle, SetConsoleMode, STD_ERROR_HANDLE, STD_INPUT_HANDLE,
            STD_OUTPUT_HANDLE,
        };
        unsafe fn enable(handle_id: u32, extra: u32) -> bool {
            let handle = GetStdHandle(handle_id);
            if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                return false;
            }
            let mut mode = 0u32;
            if GetConsoleMode(handle, &mut mode) == 0 {
                return false;
            }
            SetConsoleMode(handle, mode | extra) != 0
        }
        unsafe {
            let _input = enable(STD_INPUT_HANDLE, console_vt_input_flags());
            let out = enable(STD_OUTPUT_HANDLE, console_vt_output_flags());
            let err = enable(STD_ERROR_HANDLE, console_vt_output_flags());
            out || err
        }
    }
}

pub fn detach_command(command: &mut std::process::Command) {
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(|| {
            rustix::process::setsid().map_err(io::Error::from)?;
            Ok(())
        });
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(detach_creation_flags());
    }
}

/// Detach `command` and spawn it without inheriting the caller's stdio pipes.
///
/// On Windows, `Command` sets `bInheritHandles`, so a detached `pmuxd` keeps
/// every inheritable handle, including a captured stdout pipe. The parent
/// then waits forever. The log handles assigned as the child's own stdio stay
/// inheritable. A standard handle that is already inheritable is marked
/// non-inheritable for this spawn, then put back. A handle that was already
/// non-inheritable is left unchanged.
pub fn spawn_detached(command: &mut std::process::Command) -> io::Result<std::process::Child> {
    detach_command(command);
    #[cfg(windows)]
    {
        spawn_without_parent_stdio(command)
    }
    #[cfg(not(windows))]
    {
        command.spawn()
    }
}

#[cfg(windows)]
fn spawn_without_parent_stdio(
    command: &mut std::process::Command,
) -> io::Result<std::process::Child> {
    use std::sync::Mutex;
    use windows_sys::Win32::Foundation::{
        GetHandleInformation, SetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT,
        INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };

    // Overlapping spawns would restore the inherit bit before the other spawn.
    static SPAWN_LOCK: Mutex<()> = Mutex::new(());
    let _guard = SPAWN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    unsafe fn standard_handle(id: u32) -> HANDLE {
        let handle = GetStdHandle(id);
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            std::ptr::null_mut()
        } else {
            handle
        }
    }

    struct RestoreInherit {
        handles: [HANDLE; 3],
        original: [u32; 3],
        restore: [bool; 3],
    }
    impl Drop for RestoreInherit {
        fn drop(&mut self) {
            for (index, handle) in self.handles.iter().copied().enumerate() {
                if self.restore[index] {
                    unsafe {
                        SetHandleInformation(handle, HANDLE_FLAG_INHERIT, self.original[index]);
                    }
                }
            }
        }
    }

    unsafe {
        let handles = [
            standard_handle(STD_INPUT_HANDLE),
            standard_handle(STD_OUTPUT_HANDLE),
            standard_handle(STD_ERROR_HANDLE),
        ];
        let mut restore = RestoreInherit {
            handles,
            original: [0; 3],
            restore: [false; 3],
        };
        for (index, handle) in handles.iter().copied().enumerate() {
            if handle.is_null() {
                continue;
            }
            let mut flags = 0u32;
            if GetHandleInformation(handle, &mut flags) == 0 {
                continue;
            }
            let inherit = flags & HANDLE_FLAG_INHERIT;
            restore.original[index] = inherit;
            // Already private. Setting the bit on the way out would publish it.
            if inherit == 0 {
                continue;
            }
            if SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) != 0 {
                restore.restore[index] = true;
            }
        }
        let child = command.spawn();
        drop(restore);
        child
    }
}

pub fn socket_identity(path: &Path) -> io::Result<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let m = fs::metadata(path)?;
        Ok(format!(
            "{}:{}:{}:{}",
            m.dev(),
            m.ino(),
            m.ctime(),
            m.ctime_nsec()
        ))
    }
    #[cfg(windows)]
    {
        use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
        use windows_sys::Win32::Storage::FileSystem::*;
        let file = fs::OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)?;
        unsafe {
            let mut info: BY_HANDLE_FILE_INFORMATION = std::mem::zeroed();
            if GetFileInformationByHandle(file.as_raw_handle(), &mut info) == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(format!(
                "{}:{}:{}:{}:{}",
                info.dwVolumeSerialNumber,
                info.nFileIndexHigh,
                info.nFileIndexLow,
                info.ftCreationTime.dwHighDateTime,
                info.ftCreationTime.dwLowDateTime
            ))
        }
    }
}

pub fn home_dir() -> Option<std::ffi::OsString> {
    std::env::var_os("HOME").or({
        #[cfg(windows)]
        {
            std::env::var_os("USERPROFILE")
        }
        #[cfg(not(windows))]
        {
            None
        }
    })
}
pub fn config_home() -> Option<std::ffi::OsString> {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .or({
            #[cfg(windows)]
            {
                std::env::var_os("APPDATA")
            }
            #[cfg(not(windows))]
            {
                None
            }
        })
}
pub fn data_home() -> Option<std::ffi::OsString> {
    std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .or({
            #[cfg(windows)]
            {
                std::env::var_os("LOCALAPPDATA")
            }
            #[cfg(not(windows))]
            {
                None
            }
        })
}
pub fn executable_name(name: &str) -> String {
    if cfg!(windows) && !name.to_ascii_lowercase().ends_with(".exe") {
        format!("{name}.exe")
    } else {
        name.into()
    }
}
pub fn default_shell() -> String {
    #[cfg(windows)]
    {
        std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into())
    }
    #[cfg(not(windows))]
    {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
    }
}
pub fn default_shell_command() -> Vec<String> {
    let mut command = vec![default_shell()];
    if cfg!(unix) {
        command.push("-l".into());
    }
    command
}

/// A private job whose drop terminates the probe and every descendant.
#[cfg(windows)]
pub struct ProbeJob(windows_sys::Win32::Foundation::HANDLE);
#[cfg(windows)]
impl Drop for ProbeJob {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}
#[cfg(windows)]
pub fn spawn_probe(
    command: &mut std::process::Command,
) -> io::Result<(std::process::Child, ProbeJob)> {
    use std::os::windows::{io::AsRawHandle, process::CommandExt};
    use windows_sys::Win32::{
        Foundation::*,
        System::{Diagnostics::ToolHelp::*, JobObjects::*, Threading::*},
    };
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return Err(io::Error::last_os_error());
        }
        let job = ProbeJob(job);
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of_val(&limits) as u32,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        command.creation_flags(CREATE_SUSPENDED | CREATE_NO_WINDOW);
        let mut child = command.spawn()?;
        if AssignProcessToJobObject(job.0, child.as_raw_handle()) == 0 {
            let error = io::Error::last_os_error();
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let mut entry: THREADENTRY32 = std::mem::zeroed();
        entry.dwSize = std::mem::size_of_val(&entry) as u32;
        let mut resumed = false;
        if Thread32First(snapshot, &mut entry) != 0 {
            loop {
                if entry.th32OwnerProcessID == child.id() {
                    let handle = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                    if !handle.is_null() {
                        resumed = ResumeThread(handle) != u32::MAX;
                        CloseHandle(handle);
                    }
                    break;
                }
                if Thread32Next(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);
        if !resumed {
            return Err(io::Error::other("could not resume version probe"));
        }
        Ok((child, job))
    }
}

/// Flush a regular file using the access rights required by the platform.
pub fn sync_file(path: &Path) -> io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    options.write(true);
    options.open(path)?.sync_all()
}

/// Directory fsync is available on Unix. Windows replacements use write-through.
pub fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        fs::File::open(path)?.sync_all()
    }
    #[cfg(windows)]
    {
        let _ = path;
        Ok(())
    }
}

pub fn replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        fs::rename(source, destination)
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::*;
        let wide = |p: &Path| {
            p.as_os_str()
                .encode_wide()
                .chain(Some(0))
                .collect::<Vec<_>>()
        };
        let result = unsafe {
            MoveFileExW(
                wide(source).as_ptr(),
                wide(destination).as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if result == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use std::io::Read;
    use std::os::windows::io::AsRawHandle;
    use std::process::Command;
    use std::time::Duration;
    use windows_sys::Win32::Foundation::{
        GetHandleInformation, SetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT,
    };
    use windows_sys::Win32::System::Console::{GetStdHandle, SetStdHandle, STD_OUTPUT_HANDLE};

    struct RestoreStdout(HANDLE);
    impl Drop for RestoreStdout {
        fn drop(&mut self) {
            unsafe {
                SetStdHandle(STD_OUTPUT_HANDLE, self.0);
            }
        }
    }

    struct KillChild(std::process::Child);
    impl Drop for KillChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn detached_spawn_closes_the_caller_stdout_pipe() {
        let (mut reader, writer) = std::io::pipe().expect("pipe");
        let write_handle = writer.as_raw_handle() as HANDLE;
        let mut flags = 0u32;
        assert_ne!(
            unsafe { GetHandleInformation(write_handle, &mut flags) },
            0,
            "pipe handle flags"
        );
        assert_ne!(
            flags & HANDLE_FLAG_INHERIT,
            0,
            "the test pipe must start inheritable"
        );
        let previous = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
        assert_ne!(
            unsafe { SetStdHandle(STD_OUTPUT_HANDLE, write_handle) },
            0,
            "install test stdout"
        );
        let child = {
            let _restore_stdout = RestoreStdout(previous);
            let mut command = Command::new(format!(
                r"{}\System32\ping.exe",
                std::env::var("SystemRoot").expect("SystemRoot")
            ));
            command
                .args(["-n", "30", "127.0.0.1"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            KillChild(super::spawn_detached(&mut command).expect("spawn ping"))
        };
        let mut flags_after = 0u32;
        assert_ne!(
            unsafe { GetHandleInformation(write_handle, &mut flags_after) },
            0
        );
        assert_ne!(
            flags_after & HANDLE_FLAG_INHERIT,
            0,
            "parent stdout inherit bit was not restored"
        );
        drop(writer);
        let (sent, received) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buffer = [0u8; 1];
            let _ = sent.send(reader.read(&mut buffer));
        });
        match received.recv_timeout(Duration::from_secs(2)) {
            Ok(Ok(0)) => {}
            Ok(other) => panic!("caller stdout pipe returned {other:?}, expected EOF"),
            Err(_) => panic!("detached child kept the caller stdout pipe open"),
        }
        drop(child);
    }

    #[test]
    fn detached_spawn_keeps_a_non_inheritable_stdout_private() {
        let (mut reader, writer) = std::io::pipe().expect("pipe");
        let write_handle = writer.as_raw_handle() as HANDLE;
        assert_ne!(
            unsafe { SetHandleInformation(write_handle, HANDLE_FLAG_INHERIT, 0) },
            0,
            "clear inherit"
        );
        let mut flags = 0u32;
        assert_ne!(
            unsafe { GetHandleInformation(write_handle, &mut flags) },
            0,
            "pipe handle flags"
        );
        assert_eq!(
            flags & HANDLE_FLAG_INHERIT,
            0,
            "the test pipe must start non-inheritable"
        );
        let previous = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
        assert_ne!(
            unsafe { SetStdHandle(STD_OUTPUT_HANDLE, write_handle) },
            0,
            "install test stdout"
        );
        let child = {
            let _restore_stdout = RestoreStdout(previous);
            let mut command = Command::new(format!(
                r"{}\System32\ping.exe",
                std::env::var("SystemRoot").expect("SystemRoot")
            ));
            command
                .args(["-n", "30", "127.0.0.1"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            KillChild(super::spawn_detached(&mut command).expect("spawn ping"))
        };
        let mut flags_after = 0u32;
        assert_ne!(
            unsafe { GetHandleInformation(write_handle, &mut flags_after) },
            0
        );
        assert_eq!(
            flags_after & HANDLE_FLAG_INHERIT,
            0,
            "spawn made a private stdout handle inheritable"
        );
        drop(writer);
        let (sent, received) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buffer = [0u8; 1];
            let _ = sent.send(reader.read(&mut buffer));
        });
        match received.recv_timeout(Duration::from_secs(2)) {
            Ok(Ok(0)) => {}
            Ok(other) => panic!("caller stdout pipe returned {other:?}, expected EOF"),
            Err(_) => panic!("detached child kept the caller stdout pipe open"),
        }
        drop(child);
    }
}

#[cfg(test)]
mod console_flags {
    #[test]
    fn helper_hides_the_console_and_detach_stays_a_separate_mask() {
        assert_eq!(super::helper_creation_flags(), 0x0800_0000);
        assert_eq!(super::detach_creation_flags(), 0x0000_0008 | 0x0000_0200);
        assert_eq!(
            super::helper_creation_flags() & super::detach_creation_flags(),
            0
        );
    }

    #[test]
    fn console_vt_flags_match_the_win32_values() {
        assert_eq!(super::console_vt_output_flags(), 0x0004 | 0x0008);
        assert_eq!(super::console_vt_input_flags(), 0x0200);
        assert_ne!(
            super::console_vt_output_flags(),
            super::console_vt_input_flags()
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_terminals_already_accept_vt() {
        assert!(super::enable_console_vt());
    }

    #[cfg(windows)]
    #[test]
    fn vt_flags_match_the_sdk() {
        use windows_sys::Win32::System::Console::{
            DISABLE_NEWLINE_AUTO_RETURN, ENABLE_VIRTUAL_TERMINAL_INPUT,
            ENABLE_VIRTUAL_TERMINAL_PROCESSING,
        };
        assert_eq!(
            super::console_vt_output_flags(),
            ENABLE_VIRTUAL_TERMINAL_PROCESSING | DISABLE_NEWLINE_AUTO_RETURN
        );
        assert_eq!(
            super::console_vt_input_flags(),
            ENABLE_VIRTUAL_TERMINAL_INPUT
        );
    }
}
