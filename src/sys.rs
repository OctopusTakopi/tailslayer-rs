use crate::{Error, Result};
use std::fs::File;
use std::io::{ErrorKind, Read};
use std::mem;
use std::os::unix::fs::FileExt;
use std::ptr::NonNull;

pub fn pin_to_cpu(core: usize) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let mut cpuset: libc::cpu_set_t = unsafe { mem::zeroed() };
        unsafe {
            libc::CPU_ZERO(&mut cpuset);
            libc::CPU_SET(core, &mut cpuset);
        }
        let rc = unsafe { libc::sched_setaffinity(0, mem::size_of::<libc::cpu_set_t>(), &cpuset) };
        if rc == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error().into())
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = core;
        Err(Error::Unsupported {
            operation: "cpu pinning",
            details: "only implemented on Linux",
        })
    }
}

pub fn map_hugetlb(bytes: usize, hugepage_size: usize) -> Result<NonNull<u8>> {
    #[cfg(target_os = "linux")]
    {
        let huge_shift = hugepage_size.trailing_zeros() as i32;
        let flags = libc::MAP_PRIVATE
            | libc::MAP_ANONYMOUS
            | libc::MAP_HUGETLB
            | (huge_shift << libc::MAP_HUGE_SHIFT);

        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                bytes,
                libc::PROT_READ | libc::PROT_WRITE,
                flags,
                -1,
                0,
            )
        };

        if ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error().into());
        }

        unsafe {
            libc::mlock(ptr, bytes);
        }

        NonNull::new(ptr.cast::<u8>()).ok_or(Error::Unsupported {
            operation: "hugetlb mapping",
            details: "mmap returned a null pointer",
        })
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = (bytes, hugepage_size);
        Err(Error::Unsupported {
            operation: "hugetlb mapping",
            details: "only implemented on Linux",
        })
    }
}

pub unsafe fn unmap_hugetlb(ptr: NonNull<u8>, bytes: usize) {
    #[cfg(target_os = "linux")]
    unsafe {
        libc::munmap(ptr.as_ptr().cast::<libc::c_void>(), bytes);
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = (ptr, bytes);
    }
}

pub fn virt_to_phys(addr: *const u8) -> Result<u64> {
    #[cfg(target_os = "linux")]
    {
        let pagemap = File::open("/proc/self/pagemap")?;
        let mut entry = [0_u8; 8];
        let offset = (addr as u64 / 4096) * 8;
        let bytes_read = pagemap.read_at(&mut entry, offset)?;
        if bytes_read != entry.len() {
            return Err(std::io::Error::new(
                ErrorKind::UnexpectedEof,
                "short read from /proc/self/pagemap",
            )
            .into());
        }

        let entry = u64::from_ne_bytes(entry);
        if entry & (1_u64 << 63) == 0 {
            return Err(Error::ValidationFailed("page is not present"));
        }

        let pfn = entry & ((1_u64 << 55) - 1);
        if pfn == 0 {
            return Err(Error::ValidationFailed(
                "PFN unavailable from /proc/self/pagemap; run with elevated privileges",
            ));
        }

        Ok((pfn * 4096) | (addr as u64 & 0xfff))
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = addr;
        Err(Error::Unsupported {
            operation: "physical address lookup",
            details: "only implemented on Linux",
        })
    }
}

pub fn compute_channel(phys: u64, channel_bit: usize) -> usize {
    ((phys >> channel_bit) & 1) as usize
}

#[allow(dead_code)]
pub fn read_file(path: &str) -> Result<String> {
    let mut file = File::open(path)?;
    let mut buffer = String::new();
    file.read_to_string(&mut buffer)?;
    Ok(buffer)
}
