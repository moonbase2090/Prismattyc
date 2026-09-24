//! Console byte input and socket wakeups share native wait handles.
use prismattyc_mux::local_socket::UnixStream;
use std::{
    collections::VecDeque,
    io::{self, Read},
    os::windows::io::AsRawSocket,
    sync::{Arc, Condvar, Mutex},
    thread,
};
use windows_sys::Win32::{
    Foundation::*,
    Networking::WinSock::*,
    System::{Console::*, Threading::*},
};

#[derive(Clone, Copy)]
pub struct PollFlags(u8);
impl PollFlags {
    pub const IN: Self = Self(1);
    pub const HUP: Self = Self(2);
    pub const ERR: Self = Self(4);
    pub const NVAL: Self = Self(8);
    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
    pub fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }
}
impl std::ops::BitOr for PollFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

pub struct SocketWake {
    event: WSAEVENT,
    socket: usize,
}
impl SocketWake {
    pub fn new(stream: &UnixStream) -> io::Result<Self> {
        unsafe {
            let event = WSACreateEvent();
            if event == 0 {
                return Err(io::Error::from_raw_os_error(WSAGetLastError()));
            }
            let socket = stream.as_raw_socket() as usize;
            if WSAEventSelect(socket, event, FD_READ as i32 | FD_CLOSE as i32) == SOCKET_ERROR {
                let error = io::Error::from_raw_os_error(WSAGetLastError());
                WSACloseEvent(event);
                return Err(error);
            }
            Ok(Self { event, socket })
        }
    }
    fn acknowledge(&self) -> io::Result<bool> {
        unsafe {
            let mut events: WSANETWORKEVENTS = std::mem::zeroed();
            if WSAEnumNetworkEvents(self.socket, self.event, &mut events) == SOCKET_ERROR {
                return Err(io::Error::from_raw_os_error(WSAGetLastError()));
            }
            Ok(events.lNetworkEvents & FD_CLOSE as i32 != 0)
        }
    }
}
impl Drop for SocketWake {
    fn drop(&mut self) {
        unsafe {
            WSACloseEvent(self.event);
        }
    }
}

struct InputState {
    event: usize,
    queue: Mutex<VecDeque<io::Result<Vec<u8>>>>,
    space: Condvar,
}
impl Drop for InputState {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.event as HANDLE);
        }
    }
}
pub struct Input(Arc<InputState>);
impl Input {
    pub fn new() -> io::Result<Self> {
        static SHARED: Mutex<Option<Arc<InputState>>> = Mutex::new(None);
        let mut shared = SHARED.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(state) = shared.as_ref() {
            return Ok(Self(Arc::clone(state)));
        }
        let event = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if event.is_null() {
            return Err(io::Error::last_os_error());
        }
        let state = Arc::new(InputState {
            event: event as usize,
            queue: Mutex::new(VecDeque::new()),
            space: Condvar::new(),
        });
        let worker = Arc::clone(&state);
        thread::Builder::new()
            .name("attach-console-input".into())
            .spawn(move || {
                let mut stdin = io::stdin().lock();
                loop {
                    let mut bytes = vec![0u8; 256];
                    let result = stdin.read(&mut bytes).map(|n| {
                        bytes.truncate(n);
                        bytes
                    });
                    let end = result.as_ref().map_or(true, |b| b.is_empty());
                    let mut queue = worker.queue.lock().unwrap_or_else(|e| e.into_inner());
                    while queue.len() >= 32 {
                        queue = worker.space.wait(queue).unwrap_or_else(|e| e.into_inner());
                    }
                    queue.push_back(result);
                    unsafe {
                        SetEvent(worker.event as HANDLE);
                    }
                    if end {
                        break;
                    }
                }
            })?;
        *shared = Some(Arc::clone(&state));
        Ok(Self(state))
    }
    pub fn poll(&self, wake: Option<&SocketWake>) -> io::Result<(PollFlags, bool)> {
        let handles = [
            self.0.event as HANDLE,
            wake.map_or(std::ptr::null_mut(), |wake| wake.event as HANDLE),
        ];
        let count = if wake.is_some() { 2 } else { 1 };
        let result = unsafe { WaitForMultipleObjects(count, handles.as_ptr(), 0, 50) };
        if result == WAIT_FAILED {
            return Err(io::Error::last_os_error());
        }
        let input = if !self
            .0
            .queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
        {
            PollFlags::IN
        } else {
            PollFlags(0)
        };
        let closed = if let Some(wake) = wake {
            wake.acknowledge()?
        } else {
            false
        };
        Ok((input, closed))
    }
    pub fn read(&self, buffer: &mut [u8]) -> io::Result<usize> {
        let mut queue = self.0.queue.lock().unwrap_or_else(|e| e.into_inner());
        let result = queue
            .pop_front()
            .ok_or_else(|| io::Error::from(io::ErrorKind::WouldBlock))?;
        if queue.is_empty() {
            unsafe {
                ResetEvent(self.0.event as HANDLE);
            }
        }
        self.0.space.notify_one();
        let bytes = result?;
        assert!(bytes.len() <= buffer.len());
        buffer[..bytes.len()].copy_from_slice(&bytes);
        Ok(bytes.len())
    }
}

pub struct RawConsole {
    input: HANDLE,
    output: HANDLE,
    input_mode: u32,
    output_mode: u32,
}
impl RawConsole {
    pub fn enter() -> io::Result<Self> {
        unsafe {
            let input = GetStdHandle(STD_INPUT_HANDLE);
            let output = GetStdHandle(STD_OUTPUT_HANDLE);
            let mut input_mode = 0;
            let mut output_mode = 0;
            if GetConsoleMode(input, &mut input_mode) == 0
                || GetConsoleMode(output, &mut output_mode) == 0
            {
                return Err(io::Error::last_os_error());
            }
            let raw = (input_mode
                & !(ENABLE_LINE_INPUT
                    | ENABLE_ECHO_INPUT
                    | ENABLE_PROCESSED_INPUT
                    | ENABLE_QUICK_EDIT_MODE))
                | ENABLE_VIRTUAL_TERMINAL_INPUT
                | ENABLE_EXTENDED_FLAGS;
            if SetConsoleMode(input, raw) == 0 {
                return Err(io::Error::last_os_error());
            }
            if SetConsoleMode(output, output_mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) == 0 {
                let error = io::Error::last_os_error();
                SetConsoleMode(input, input_mode);
                return Err(error);
            }
            Ok(Self {
                input,
                output,
                input_mode,
                output_mode,
            })
        }
    }
    pub fn restore(&self) {
        unsafe {
            SetConsoleMode(self.input, self.input_mode);
            SetConsoleMode(self.output, self.output_mode);
        }
    }
}
impl Drop for RawConsole {
    fn drop(&mut self) {
        self.restore();
    }
}
