//! Compile-time opt-in process allocation counters for profiling builds.
//!
//! The global allocator remains [`std::alloc::System`]; this wrapper only adds
//! relaxed monotonic counters. Keeping the module behind the
//! `allocation-profile` feature means ordinary builds pay no atomic overhead.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static ALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);
static DEALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static DEALLOCATED_BYTES: AtomicU64 = AtomicU64::new(0);

pub struct CountingAllocator;

fn record_allocation(size: usize) {
    ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
    ALLOCATED_BYTES.fetch_add(size as u64, Ordering::Relaxed);
}

fn record_deallocation(size: usize) {
    DEALLOCATIONS.fetch_add(1, Ordering::Relaxed);
    DEALLOCATED_BYTES.fetch_add(size as u64, Ordering::Relaxed);
}

// SAFETY: Every operation delegates to `System` with the original pointer and
// layout contract. Counters are updated only after successful allocations.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: The caller supplies the `GlobalAlloc` layout contract.
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            record_allocation(layout.size());
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: The caller supplies the `GlobalAlloc` layout contract.
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            record_allocation(layout.size());
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        record_deallocation(layout.size());
        // SAFETY: The caller supplies the matching pointer and layout.
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: The caller supplies the matching pointer/layout and the new
        // size required by the `GlobalAlloc` contract.
        let new_pointer = unsafe { System.realloc(pointer, layout, new_size) };
        if !new_pointer.is_null() {
            record_deallocation(layout.size());
            record_allocation(new_size);
        }
        new_pointer
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Snapshot {
    allocations: u64,
    allocated_bytes: u64,
    deallocations: u64,
    deallocated_bytes: u64,
}

impl Snapshot {
    pub fn delta_since(self, earlier: Self) -> Delta {
        Delta {
            allocations: self.allocations.wrapping_sub(earlier.allocations),
            allocated_bytes: self.allocated_bytes.wrapping_sub(earlier.allocated_bytes),
            deallocations: self.deallocations.wrapping_sub(earlier.deallocations),
            deallocated_bytes: self
                .deallocated_bytes
                .wrapping_sub(earlier.deallocated_bytes),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Delta {
    allocations: u64,
    allocated_bytes: u64,
    deallocations: u64,
    deallocated_bytes: u64,
}

impl Delta {
    pub fn allocations(self) -> u64 {
        self.allocations
    }

    pub fn allocated_bytes(self) -> u64 {
        self.allocated_bytes
    }

    pub fn deallocations(self) -> u64 {
        self.deallocations
    }

    pub fn deallocated_bytes(self) -> u64 {
        self.deallocated_bytes
    }

    pub fn live_bytes_change(self) -> i128 {
        i128::from(self.allocated_bytes) - i128::from(self.deallocated_bytes)
    }
}

pub fn snapshot() -> Snapshot {
    Snapshot {
        allocations: ALLOCATIONS.load(Ordering::Relaxed),
        allocated_bytes: ALLOCATED_BYTES.load(Ordering::Relaxed),
        deallocations: DEALLOCATIONS.load(Ordering::Relaxed),
        deallocated_bytes: DEALLOCATED_BYTES.load(Ordering::Relaxed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_delta_reports_traffic_and_live_byte_change() {
        let before = Snapshot {
            allocations: 10,
            allocated_bytes: 1_000,
            deallocations: 8,
            deallocated_bytes: 700,
        };
        let after = Snapshot {
            allocations: 14,
            allocated_bytes: 1_900,
            deallocations: 11,
            deallocated_bytes: 1_200,
        };

        let delta = after.delta_since(before);

        assert_eq!(delta.allocations, 4);
        assert_eq!(delta.allocated_bytes, 900);
        assert_eq!(delta.deallocations, 3);
        assert_eq!(delta.deallocated_bytes, 500);
        assert_eq!(delta.live_bytes_change(), 400);
    }
}
