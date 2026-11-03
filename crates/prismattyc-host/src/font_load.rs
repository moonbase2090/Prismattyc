//! Background loading for replacement font metrics.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;

use crate::mux::Wake;
use crate::raster::FontMetrics;

struct Request {
    owner: u64,
    id: u64,
    px: f32,
    primary: Option<PathBuf>,
    fallbacks: Vec<PathBuf>,
}

pub(crate) struct Completion {
    pub owner: u64,
    pub id: u64,
    pub result: anyhow::Result<FontMetrics>,
}

#[derive(Clone)]
pub(crate) struct Handle {
    owner: u64,
    next_id: Arc<AtomicU64>,
    requests: Sender<Request>,
}

impl Handle {
    pub(crate) fn owner_id(&self) -> u64 {
        self.owner
    }

    pub(crate) fn request(
        &self,
        px: f32,
        primary: Option<&std::path::Path>,
        fallbacks: &[PathBuf],
    ) -> Option<u64> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.requests
            .send(Request {
                owner: self.owner,
                id,
                px,
                primary: primary.map(PathBuf::from),
                fallbacks: fallbacks.to_vec(),
            })
            .ok()
            .map(|()| id)
    }
}

pub(crate) struct Worker {
    requests: Sender<Request>,
    results: Receiver<Completion>,
    next_owner: AtomicU64,
    next_id: Arc<AtomicU64>,
}

impl Worker {
    pub(crate) fn spawn(wake: Wake) -> Option<Self> {
        let (requests, inbox) = mpsc::channel::<Request>();
        let (outbox, results) = mpsc::channel();
        std::thread::Builder::new()
            .name("font-load".into())
            .spawn(move || {
                while let Ok(first) = inbox.recv() {
                    // Keep only the newest queued request for each live window.
                    let mut latest = HashMap::new();
                    latest.insert(first.owner, first);
                    for request in inbox.try_iter() {
                        latest.insert(request.owner, request);
                    }
                    for request in latest.into_values() {
                        let mut result = FontMetrics::load_with(
                            request.px,
                            request.primary.as_deref(),
                            &request.fallbacks,
                        );
                        if let Ok(font) = &mut result {
                            font.enable_async_fallback(&wake);
                        }
                        if outbox
                            .send(Completion {
                                owner: request.owner,
                                id: request.id,
                                result,
                            })
                            .is_err()
                        {
                            return;
                        }
                        wake();
                    }
                }
            })
            .ok()?;
        Some(Self {
            requests,
            results,
            next_owner: AtomicU64::new(1),
            next_id: Arc::new(AtomicU64::new(1)),
        })
    }

    pub(crate) fn handle(&self) -> Handle {
        Handle {
            owner: self.next_owner.fetch_add(1, Ordering::Relaxed),
            next_id: Arc::clone(&self.next_id),
            requests: self.requests.clone(),
        }
    }

    pub(crate) fn drain(&self) -> Vec<Completion> {
        self.results.try_iter().collect()
    }
}
