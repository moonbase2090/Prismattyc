//! System fallback faces, resolved off the main thread.
//!
//! Finding the system face that covers a character can read and index large
//! font collections, and loading that face reads the whole file. A
//! `FontMetrics` with a [`Worker`] sends each uncovered character here, paints
//! it blank meanwhile, and repaints when the result arrives. Loaded faces are
//! shared by every window and font size, so a file is read once per process.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};

use fontdue::{Font, FontSettings};

use crate::mux::Wake;
use crate::system_fonts::CoveringFace;

/// The fallback face for one character, or `None` when no system face covers it.
pub(crate) struct Resolved {
    pub ch: char,
    pub font: Option<Arc<Font>>,
}

/// Faces read from disk, by path and collection index. A fontdue `Font` does
/// not depend on the pixel size, so zoom reuses it. `None` records a face that
/// failed to load, so it is not read again.
type LoadedFaces = Mutex<HashMap<(PathBuf, u32), Arc<OnceLock<Option<Arc<Font>>>>>>;

fn loaded_faces() -> &'static LoadedFaces {
    static FACES: OnceLock<LoadedFaces> = OnceLock::new();
    FACES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cached_face(
    cache: &LoadedFaces,
    key: (PathBuf, u32),
    load: impl FnOnce() -> Option<Arc<Font>>,
) -> Option<Arc<Font>> {
    let cell = {
        let mut cache = cache.lock().ok()?;
        Arc::clone(
            cache
                .entry(key)
                .or_insert_with(|| Arc::new(OnceLock::new())),
        )
    };
    // The per-face cell serializes same-font loads while keeping disk I/O out
    // of the map lock, so unrelated faces can still load concurrently.
    cell.get_or_init(load).clone()
}

/// Load `face`, or reuse the copy another window or font size loaded.
fn load_face(face: &CoveringFace) -> Option<Arc<Font>> {
    let key = (face.path.clone(), face.index);
    cached_face(loaded_faces(), key, || {
        // Read outside the map lock so a slow read does not hold up unrelated
        // faces. The per-face OnceLock prevents duplicate concurrent reads.
        let font = crate::font_file::map(&face.path).ok().and_then(|data| {
            let settings = FontSettings {
                collection_index: face.index,
                ..FontSettings::default()
            };
            Font::from_bytes(data.as_ref(), settings).ok().map(Arc::new)
        });
        if font.is_some() {
            eprintln!(
                "prismattyc-host: system fallback {}#{}",
                face.path.display(),
                face.index
            );
        }
        font
    })
}

/// Find and load the system face for `ch`. May read font files.
pub(crate) fn resolve(ch: char) -> Option<Arc<Font>> {
    load_face(&crate::system_fonts::covering_face(ch)?)
}

/// A background thread that resolves fallback faces. It exits when the
/// `Worker` is dropped, for example when a font swap replaces its
/// `FontMetrics`.
pub(crate) struct Worker {
    requests: Sender<char>,
    results: Receiver<Resolved>,
}

impl Worker {
    /// Start the thread. `wake` runs after each result, so the event loop
    /// drains it on its next pump. `None` if the thread cannot start.
    pub(crate) fn spawn(wake: Wake) -> Option<Self> {
        let (requests, inbox) = mpsc::channel::<char>();
        let (outbox, results) = mpsc::channel();
        std::thread::Builder::new()
            .name("font-fallback".into())
            .spawn(move || {
                for ch in inbox {
                    let font = resolve(ch);
                    if outbox.send(Resolved { ch, font }).is_err() {
                        return;
                    }
                    wake();
                }
            })
            .ok()?;
        Some(Self { requests, results })
    }

    /// Queue `ch`. False if the thread has stopped.
    pub(crate) fn request(&self, ch: char) -> bool {
        self.requests.send(ch).is_ok()
    }

    /// Results that have arrived, without waiting.
    pub(crate) fn drain(&self) -> Vec<Resolved> {
        self.results.try_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Barrier;
    use std::thread;
    use std::time::{Duration, Instant};

    fn fixture(name: &str) -> CoveringFace {
        CoveringFace {
            path: Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("assets/fonts")
                .join(name),
            index: 0,
        }
    }

    #[test]
    fn a_face_is_read_once_and_shared() {
        let face = fixture("DejaVuSansMono.ttf");
        let first = load_face(&face).expect("bundled fixture loads");
        let second = load_face(&face).expect("bundled fixture loads");
        assert!(
            Arc::ptr_eq(&first, &second),
            "a second load must reuse the face"
        );
        assert_ne!(first.lookup_glyph_index('A'), 0);
    }

    #[test]
    fn concurrent_face_lookups_load_once() {
        let face = fixture("DejaVuSansMono.ttf");
        let key = (face.path.clone(), face.index);
        let cache = Arc::new(Mutex::new(HashMap::new()));
        let loads = Arc::new(AtomicUsize::new(0));
        let start = Arc::new(Barrier::new(8));
        let workers = (0..8)
            .map(|_| {
                let cache = Arc::clone(&cache);
                let loads = Arc::clone(&loads);
                let start = Arc::clone(&start);
                let key = key.clone();
                let path = face.path.clone();
                thread::spawn(move || {
                    start.wait();
                    cached_face(cache.as_ref(), key, || {
                        loads.fetch_add(1, Ordering::SeqCst);
                        thread::sleep(Duration::from_millis(20));
                        let bytes = crate::font_file::map(&path).ok()?;
                        Font::from_bytes(bytes.as_ref(), FontSettings::default())
                            .ok()
                            .map(Arc::new)
                    })
                    .expect("fixture font loads")
                })
            })
            .collect::<Vec<_>>();
        let faces = workers
            .into_iter()
            .map(|worker| worker.join().expect("face load thread completes"))
            .collect::<Vec<_>>();
        assert_eq!(loads.load(Ordering::SeqCst), 1, "font file read once");
        assert!(faces.iter().all(|font| Arc::ptr_eq(&faces[0], font)));
    }

    #[test]
    fn an_unreadable_face_loads_as_none() {
        let face = CoveringFace {
            path: PathBuf::from("/nonexistent/prismattyc-fallback-test.ttf"),
            index: 0,
        };
        assert!(load_face(&face).is_none());
        assert!(load_face(&face).is_none());
    }

    #[test]
    fn the_worker_answers_each_request_and_wakes() {
        let woke = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&woke);
        let worker = Worker::spawn(Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        }))
        .expect("worker starts");
        // A noncharacter no font covers.
        assert!(worker.request('\u{10ffff}'));
        let deadline = Instant::now() + Duration::from_secs(30);
        while woke.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let results = worker.drain();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].ch, '\u{10ffff}');
        assert!(results[0].font.is_none());
        assert_eq!(woke.load(Ordering::SeqCst), 1);
    }
}
