// SPDX-License-Identifier: MIT
//! Process-wide admission credits follow the actual FD, pixels and worker owners.

use super::{backing::ImageDescriptor, wire12::Decline};
use std::sync::{Arc, Mutex, OnceLock};

const PROCESS_BYTES: u64 = 256 * 1024 * 1024;
const WORKERS: u8 = 2;
static POOL: OnceLock<Arc<ImagePool>> = OnceLock::new();

#[derive(Default)]
struct Usage {
    bytes: u64,
    workers: u8,
}

pub struct ImagePool {
    usage: Mutex<Usage>,
}

impl ImagePool {
    pub fn global() -> Arc<Self> {
        Arc::clone(POOL.get_or_init(Self::new))
    }

    fn new() -> Arc<Self> {
        Arc::new(Self {
            usage: Mutex::new(Usage::default()),
        })
    }

    #[cfg(test)]
    pub(crate) fn isolated() -> Arc<Self> {
        Self::new()
    }

    pub fn admit(self: &Arc<Self>, source: ImageDescriptor) -> Result<AdmittedSource, Decline> {
        let credits = self.reserve(source.logical_bytes(), source.physical_bytes())?;
        Ok(AdmittedSource { source, credits })
    }

    fn reserve(self: &Arc<Self>, logical: u64, physical: u64) -> Result<UploadCredits, Decline> {
        let bytes = logical.checked_add(physical).ok_or(Decline::ByteBudget)?;
        let mut usage = self.usage.lock().map_err(|_| Decline::Allocation)?;
        if usage.workers == WORKERS {
            return Err(Decline::Busy);
        }
        let total = usage
            .bytes
            .checked_add(bytes)
            .filter(|total| *total <= PROCESS_BYTES)
            .ok_or(Decline::ByteBudget)?;
        usage.bytes = total;
        usage.workers += 1;
        Ok(UploadCredits {
            pixels: PixelCredit {
                pool: Arc::clone(self),
                bytes: logical,
            },
            source: SourceCredit {
                pool: Arc::clone(self),
                bytes: physical,
            },
            worker: WorkerPermit {
                pool: Arc::clone(self),
            },
        })
    }
}

pub struct AdmittedSource {
    source: ImageDescriptor,
    credits: UploadCredits,
}

impl AdmittedSource {
    pub(super) fn into_parts(self) -> (ImageDescriptor, UploadCredits) {
        (self.source, self.credits)
    }
}

pub struct UploadCredits {
    pub pixels: PixelCredit,
    pub source: SourceCredit,
    pub worker: WorkerPermit,
}

macro_rules! byte_credit {
    ($name:ident) => {
        pub struct $name {
            pool: Arc<ImagePool>,
            bytes: u64,
        }
        impl Drop for $name {
            fn drop(&mut self) {
                let mut usage = self
                    .pool
                    .usage
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                usage.bytes = usage
                    .bytes
                    .checked_sub(self.bytes)
                    .expect("owned native byte credit");
            }
        }
    };
}
byte_credit!(PixelCredit);
byte_credit!(SourceCredit);

pub struct WorkerPermit {
    pool: Arc<ImagePool>,
}

impl Drop for WorkerPermit {
    fn drop(&mut self) {
        let mut usage = self
            .pool
            .usage
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        usage.workers = usage
            .workers
            .checked_sub(1)
            .expect("owned native worker permit");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const MIB: u64 = 1024 * 1024;

    fn decline(result: Result<UploadCredits, Decline>) -> Decline {
        match result {
            Err(reason) => reason,
            Ok(_) => panic!("upload was incorrectly admitted"),
        }
    }

    #[test]
    fn rounded_fd_and_owned_copy_both_count_before_a_worker_is_admitted() {
        let pool = ImagePool::new();
        let first = pool.reserve(64 * MIB, 64 * MIB + 16_384).unwrap();
        assert_eq!(
            decline(pool.reserve(64 * MIB, 64 * MIB + 16_384)),
            Decline::ByteBudget
        );
        drop(first);
        assert!(pool.reserve(64 * MIB, 64 * MIB + 16_384).is_ok());
        assert_eq!(decline(pool.reserve(u64::MAX, 1)), Decline::ByteBudget);
        assert!(pool.reserve(64 * MIB, 64 * MIB).is_ok());
    }

    #[test]
    fn source_credit_and_worker_permit_release_independently_of_live_pixels() {
        let pool = ImagePool::new();
        let first = pool.reserve(64 * MIB, 64 * MIB).unwrap();
        let second = pool.reserve(64 * MIB, 64 * MIB).unwrap();
        let UploadCredits {
            pixels,
            source,
            worker,
        } = first;
        drop(worker);
        assert_eq!(
            decline(pool.reserve(32 * MIB, 32 * MIB)),
            Decline::ByteBudget
        );
        drop(source);
        let third = pool.reserve(32 * MIB, 32 * MIB).unwrap();
        assert_eq!(decline(pool.reserve(4, 16_384)), Decline::Busy);
        drop(third);
        drop(pixels);
        drop(second);
        let _first_again = pool.reserve(64 * MIB, 64 * MIB).unwrap();
        assert!(pool.reserve(64 * MIB, 64 * MIB).is_ok());
    }

    #[test]
    fn raster_references_keep_pixels_charged_until_the_final_owner_drops() {
        let pool = ImagePool::new();
        let first = pool.reserve(64 * MIB, 64 * MIB).unwrap();
        let second = pool.reserve(64 * MIB, 64 * MIB).unwrap();
        let UploadCredits {
            pixels,
            source,
            worker,
        } = first;
        let registry_pixels = Arc::new(pixels);
        let raster_pixels = Arc::clone(&registry_pixels);
        drop(source);
        drop(worker);
        let UploadCredits {
            pixels: second_pixels,
            source: second_source,
            worker: second_worker,
        } = second;
        drop(second_source);
        drop(second_worker);
        let _pending = pool.reserve(64 * MIB, 64 * MIB).unwrap();
        assert_eq!(decline(pool.reserve(4, 16_384)), Decline::ByteBudget);
        drop(registry_pixels);
        assert_eq!(decline(pool.reserve(4, 16_384)), Decline::ByteBudget);
        drop(raster_pixels);
        assert!(pool.reserve(4, 16_384).is_ok());
        drop(second_pixels);
    }

    #[test]
    fn separate_session_owners_share_two_nonqueued_process_worker_permits() {
        let first_session = ImagePool::global();
        let second_session = ImagePool::global();
        let first = first_session.reserve(4, 16_384).unwrap();
        let second = second_session.reserve(4, 16_384).unwrap();
        assert_eq!(decline(first_session.reserve(4, 16_384)), Decline::Busy);
        let UploadCredits {
            pixels,
            source,
            worker,
        } = first;
        drop(worker);
        let third = second_session.reserve(4, 16_384).unwrap();
        assert_eq!(decline(second_session.reserve(4, 16_384)), Decline::Busy);
        drop((pixels, source, second, third));
    }
}
