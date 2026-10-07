// SPDX-License-Identifier: MPL-2.0
//! Clipboard paste delivery that never waits on the main thread (#195).
//!
//! With `async_paste = true`, a paste is one [`ChildWrite`] on the pane's
//! writer channel. The writer thread owns the back-pressure wait: the PTY
//! writer blocks in `write_all`, and the log writer chunks it into pmuxd
//! input. One message keeps `CSI 200 ~` and `CSI 201 ~` together and keeps
//! later keys behind the whole paste. Image-only clipboards are encoded to PNG
//! and written to disk on a worker thread; the host pastes the file reference
//! when the worker reports back.

use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;

use crate::mux::Wake;
use crate::rich::ChildWrite;

/// Outcome of offering a paste to a pane writer. Never blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PasteSend {
    /// The writer accepted the whole paste.
    Queued,
    /// The writer channel is full: the child has stopped reading and earlier
    /// input is still queued. Nothing was sent, so no bracket is left open.
    Busy,
    /// The writer is gone (the child exited).
    Closed,
}

/// Offer `bytes` to the writer as one message, without waiting.
pub(crate) fn send_paste(to_child: &mpsc::SyncSender<ChildWrite>, bytes: Vec<u8>) -> PasteSend {
    match to_child.try_send(ChildWrite::bytes(bytes)) {
        Ok(()) => PasteSend::Queued,
        Err(mpsc::TrySendError::Full(_)) => PasteSend::Busy,
        Err(mpsc::TrySendError::Disconnected(_)) => PasteSend::Closed,
    }
}

/// A finished image encode for the pane that was focused at paste time.
#[derive(Debug)]
pub(crate) struct EncodedImage {
    /// `PaneId::get()` of the pane focused at paste time.
    pub pane: u64,
    pub result: anyhow::Result<PathBuf>,
}

/// Clipboard RGBA pixels copied off the clipboard on the main thread.
pub(crate) struct ClipboardImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

type Encode = fn(u32, u32, &[u8]) -> anyhow::Result<PathBuf>;

/// Image pastes in flight for one window.
pub(crate) struct ImagePastes {
    tx: mpsc::Sender<EncodedImage>,
    rx: mpsc::Receiver<EncodedImage>,
    encode: Encode,
}

impl Default for ImagePastes {
    fn default() -> Self {
        Self::with_encoder(prismattyc_mux::write_paste_png)
    }
}

impl ImagePastes {
    fn with_encoder(encode: Encode) -> Self {
        let (tx, rx) = mpsc::channel();
        Self { tx, rx, encode }
    }

    /// Encode and write `image` on a worker; `wake` runs when it is done.
    pub(crate) fn start(&self, pane: u64, image: ClipboardImage, wake: Option<Wake>) {
        let tx = self.tx.clone();
        let encode = self.encode;
        let spawned = thread::Builder::new()
            .name("prism-image-paste".into())
            .spawn(move || {
                let result = encode(image.width, image.height, &image.rgba);
                let _ = tx.send(EncodedImage { pane, result });
                if let Some(wake) = wake {
                    wake();
                }
            });
        if let Err(error) = spawned {
            let _ = self.tx.send(EncodedImage {
                pane,
                result: Err(anyhow::anyhow!("spawn image paste worker: {error}")),
            });
        }
    }

    /// Encodes finished since the last call.
    pub(crate) fn take_done(&self) -> Vec<EncodedImage> {
        self.rx.try_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};

    #[test]
    fn whole_bracketed_paste_is_one_writer_message() {
        let (tx, rx) = mpsc::sync_channel::<ChildWrite>(4);
        let mut payload = b"\x1b[200~".to_vec();
        payload.extend(std::iter::repeat_n(b'x', 64 * 1024));
        payload.extend_from_slice(b"\x1b[201~");
        assert_eq!(send_paste(&tx, payload.clone()), PasteSend::Queued);
        // A key typed after the paste must land after the closing bracket.
        tx.try_send(ChildWrite::bytes(b"k".to_vec())).unwrap();
        assert_eq!(rx.try_recv().unwrap().bytes, payload);
        assert_eq!(rx.try_recv().unwrap().bytes, b"k");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn full_writer_is_busy_without_waiting_or_sending() {
        let (tx, rx) = mpsc::sync_channel::<ChildWrite>(1);
        tx.try_send(ChildWrite::bytes(b"earlier".to_vec())).unwrap();
        let started = Instant::now();
        assert_eq!(send_paste(&tx, b"paste".to_vec()), PasteSend::Busy);
        // The old path polled in 2 ms sleeps for up to 250 ms here.
        assert!(
            started.elapsed() < Duration::from_millis(50),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(rx.try_recv().unwrap().bytes, b"earlier");
        assert!(rx.try_recv().is_err(), "nothing of the paste may be sent");
    }

    #[test]
    fn closed_writer_is_reported() {
        let (tx, rx) = mpsc::sync_channel::<ChildWrite>(1);
        drop(rx);
        assert_eq!(send_paste(&tx, b"paste".to_vec()), PasteSend::Closed);
    }

    /// Writes into a scratch directory, never the real `prism-paste` one,
    /// whose pruning could delete a person's recent paste files.
    fn scratch_encoder(width: u32, height: u32, rgba: &[u8]) -> anyhow::Result<PathBuf> {
        let dir = std::env::temp_dir().join(format!("paste-195-img-{}", std::process::id()));
        prismattyc_mux::write_paste_png_in(&dir, width, height, rgba)
    }

    /// Proof harness for #195 part 2: main-thread cost of an image paste.
    /// `cargo test -p prismattyc-host --bin prismattyc-host -- --ignored --nocapture image_paste`
    #[test]
    #[ignore = "measurement harness; run with --ignored"]
    fn image_paste_main_thread_cost_harness() {
        let (width, height) = (3024u32, 1964u32);
        let rgba: Vec<u8> = (0..width as usize * height as usize * 4)
            .map(|i| (i * 31 % 251) as u8)
            .collect();
        let started = Instant::now();
        scratch_encoder(width, height, &rgba).unwrap();
        let inline = started.elapsed();
        let pastes = ImagePastes::with_encoder(scratch_encoder);
        let started = Instant::now();
        pastes.start(
            1,
            ClipboardImage {
                width,
                height,
                rgba,
            },
            None,
        );
        let call = started.elapsed();
        let done = loop {
            let done = pastes.take_done();
            if !done.is_empty() {
                break done;
            }
            thread::sleep(Duration::from_millis(5));
        };
        let total = started.elapsed();
        let _ = std::fs::remove_dir_all(
            std::env::temp_dir().join(format!("paste-195-img-{}", std::process::id())),
        );
        println!(
            "image {width}x{height}: inline encode+write on main={inline:?}; \
             async start() on main={call:?}, worker done after {total:?}"
        );
        assert!(done[0].result.is_ok());
        assert!(call < Duration::from_millis(5), "start() took {call:?}");
    }

    static ENCODER_THREAD: OnceLock<Mutex<Option<thread::ThreadId>>> = OnceLock::new();

    fn recording_encoder(width: u32, height: u32, rgba: &[u8]) -> anyhow::Result<PathBuf> {
        *ENCODER_THREAD.get_or_init(Default::default).lock().unwrap() =
            Some(thread::current().id());
        Ok(PathBuf::from(format!(
            "{width}x{height}-{}.png",
            rgba.len()
        )))
    }

    #[test]
    fn image_encode_runs_off_the_calling_thread_and_reports_back() {
        let pastes = ImagePastes::with_encoder(recording_encoder);
        let pane = 7;
        let woke = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = woke.clone();
        let wake: Wake = std::sync::Arc::new(move || {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        pastes.start(
            pane,
            ClipboardImage {
                width: 2,
                height: 1,
                rgba: vec![0; 8],
            },
            Some(wake),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        let done = loop {
            let done = pastes.take_done();
            if !done.is_empty() || Instant::now() > deadline {
                break done;
            }
            thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].pane, pane);
        assert_eq!(
            done[0].result.as_ref().unwrap(),
            &PathBuf::from("2x1-8.png")
        );
        let encoder = ENCODER_THREAD.get().unwrap().lock().unwrap().unwrap();
        assert_ne!(encoder, thread::current().id(), "encode ran on the caller");
        assert!(
            woke.load(std::sync::atomic::Ordering::SeqCst),
            "worker must wake the host"
        );
    }
}
