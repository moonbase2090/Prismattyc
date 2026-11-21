// SPDX-License-Identifier: MIT
use std::{
    io,
    os::fd::{AsFd, AsRawFd, OwnedFd},
    ptr::NonNull,
    sync::atomic::{AtomicU64, Ordering},
};

const RING_BYTES: usize = 4_198_400;
const SLOT_BYTES: usize = 1_048_576;

pub struct Producer {
    address: NonNull<u8>,
    _descriptor: OwnedFd,
    published: u64,
}

impl Producer {
    #[cfg(target_os = "linux")]
    pub fn from_legacy_setup(descriptor: OwnedFd, setup: &[u8]) -> io::Result<Self> {
        if setup.len() != 48
            || crate::u16_at(setup, 4) != 2
            || crate::u16_at(setup, 6) != 0
            || crate::u64_at(setup, 32) != RING_BYTES as u64
            || crate::u32_at(setup, 40) != 4
            || crate::u32_at(setup, 44) != SLOT_BYTES as u32
        {
            return Err(crate::invalid("legacy setup ring layout"));
        }
        crate::backing::validate(
            descriptor.as_fd(),
            RING_BYTES as u64,
            RING_BYTES as u64,
            1,
            false,
        )?;
        // The exact-length sealed Linux object is fixed before mapping.
        unsafe { Self::from_validated_minor(descriptor, crate::u64_at(setup, 8), 0) }
    }

    pub fn from_setup(descriptor: OwnedFd, setup: &[u8]) -> io::Result<Self> {
        if setup.len() != 128
            || crate::u16_at(setup, 4) != 9
            || crate::u16_at(setup, 6) != 0
            || crate::u64_at(setup, 32) != RING_BYTES as u64
            || crate::u32_at(setup, 48) != 4
            || crate::u32_at(setup, 52) != SLOT_BYTES as u32
            || crate::u16_at(setup, 58) != 1
            || crate::u32_at(setup, 60) != 0
            || setup[112..].iter().any(|&byte| byte != 0)
        {
            return Err(crate::invalid("setup ring layout"));
        }
        crate::backing::validate(
            descriptor.as_fd(),
            RING_BYTES as u64,
            crate::u64_at(setup, 40),
            crate::u16_at(setup, 56),
            false,
        )?;
        // Typed/sealed backing validation established the mapping's fixed extent.
        unsafe { Self::from_validated(descriptor, crate::u64_at(setup, 8)) }
    }

    /// The descriptor must already have passed platform backing validation.
    ///
    /// # Safety
    /// The supplied object must remain at least RING_BYTES long for this mapping's lifetime.
    pub unsafe fn from_validated(descriptor: OwnedFd, session: u64) -> io::Result<Self> {
        unsafe { Self::from_validated_minor(descriptor, session, 2) }
    }

    unsafe fn from_validated_minor(
        descriptor: OwnedFd,
        session: u64,
        minor: u16,
    ) -> io::Result<Self> {
        let address = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                RING_BYTES,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                descriptor.as_raw_fd(),
                0,
            )
        };
        if address == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let address = match NonNull::new(address.cast::<u8>()) {
            Some(address) => address,
            None => {
                unsafe { libc::munmap(address, RING_BYTES) };
                return Err(crate::invalid("null ring mapping"));
            }
        };
        let producer = Self {
            address,
            _descriptor: descriptor,
            published: 0,
        };
        let mut header = [0; 4096];
        unsafe {
            std::ptr::copy_nonoverlapping(address.as_ptr(), header.as_mut_ptr(), header.len())
        };
        let mut expected = [0; 4096];
        expected[..8].copy_from_slice(b"LUNARING");
        expected[8..10].copy_from_slice(&1u16.to_le_bytes());
        expected[10..12].copy_from_slice(&minor.to_le_bytes());
        expected[12..16].copy_from_slice(&4096u32.to_le_bytes());
        expected[16..24].copy_from_slice(&(RING_BYTES as u64).to_le_bytes());
        expected[24..28].copy_from_slice(&4u32.to_le_bytes());
        expected[28..32].copy_from_slice(&(SLOT_BYTES as u32).to_le_bytes());
        expected[32..40].copy_from_slice(&session.to_le_bytes());
        if session == 0 || header != expected {
            return Err(crate::invalid("ring header"));
        }
        Ok(producer)
    }

    fn counter(&self, at: usize) -> &AtomicU64 {
        unsafe { &*self.address.as_ptr().add(at).cast::<AtomicU64>() }
    }

    pub fn publish(&mut self, frame: &[u8]) -> io::Result<u64> {
        if frame.len() < 96
            || frame.len() > SLOT_BYTES
            || crate::u32_at(frame, 0) as usize != frame.len()
        {
            return Err(crate::invalid("frame size"));
        }
        let next = self
            .published
            .checked_add(1)
            .ok_or_else(|| crate::invalid("sequence exhausted"))?;
        if crate::u64_at(frame, 8) != next {
            return Err(crate::invalid("frame sequence"));
        }
        let consumed = self.counter(128).load(Ordering::Acquire);
        if consumed > self.published {
            return Err(crate::invalid("consumer counter"));
        }
        if self.published - consumed >= 4 {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "ring full"));
        }
        let at = 4096 + ((next - 1) % 4) as usize * SLOT_BYTES;
        unsafe {
            let destination = self.address.as_ptr().add(at);
            std::ptr::write_bytes(destination, 0, SLOT_BYTES);
            std::ptr::copy_nonoverlapping(frame.as_ptr(), destination, frame.len());
        }
        self.counter(64).store(next, Ordering::Release);
        self.published = next;
        Ok(next)
    }
}

impl Drop for Producer {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.address.as_ptr().cast(), RING_BYTES) };
    }
}
