//! How much memory Feather uses, for the `--memory` report and for giving
//! memory back when the window hides.

use windows_sys::Win32::System::ProcessStatus::{
    K32GetProcessMemoryInfo, K32QueryWorkingSet, PROCESS_MEMORY_COUNTERS,
    PROCESS_MEMORY_COUNTERS_EX,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

/// Memory in use, in bytes.
#[derive(Clone, Copy, Debug, Default)]
pub struct Usage {
    /// What Task Manager shows as Memory: pages only this process uses.
    pub private_working_set: u64,
    /// Every page in memory, including shared ones like the mapped index.
    pub working_set: u64,
    /// Memory committed for this process alone.
    pub private_bytes: u64,
}

pub fn usage() -> Usage {
    let process = unsafe { GetCurrentProcess() };
    let mut counters: PROCESS_MEMORY_COUNTERS_EX = unsafe { std::mem::zeroed() };
    counters.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
    unsafe {
        K32GetProcessMemoryInfo(
            process,
            &mut counters as *mut PROCESS_MEMORY_COUNTERS_EX as *mut PROCESS_MEMORY_COUNTERS,
            counters.cb,
        );
    }
    Usage {
        private_working_set: private_working_set(counters.WorkingSetSize as u64),
        working_set: counters.WorkingSetSize as u64,
        private_bytes: counters.PrivateUsage as u64,
    }
}

/// Counts the pages in the working set that aren't shared.
fn private_working_set(working_set: u64) -> u64 {
    let process = unsafe { GetCurrentProcess() };
    // One entry per page, plus room for pages added while asking.
    let mut capacity = (working_set / 4096) as usize + 4096;
    for _ in 0..4 {
        let mut buffer = vec![0usize; capacity + 1];
        let ok = unsafe {
            K32QueryWorkingSet(
                process,
                buffer.as_mut_ptr() as *mut _,
                (buffer.len() * std::mem::size_of::<usize>()) as u32,
            )
        };
        let count = buffer[0];
        if ok != 0 {
            // Bit 8 of each entry marks a shared page.
            let private = buffer[1..=count.min(capacity)]
                .iter()
                .filter(|entry| *entry & (1 << 8) == 0)
                .count();
            return private as u64 * 4096;
        }
        capacity = count + 4096;
    }
    0
}

pub fn megabytes(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// Gives memory back: trims the heap and the working set. Used when the
/// window hides, so a Feather waiting in the tray stays small.
pub fn trim() {
    use windows_sys::Win32::System::Memory::{GetProcessHeap, HeapCompact};
    unsafe {
        HeapCompact(GetProcessHeap(), 0);
        windows_sys::Win32::System::ProcessStatus::K32EmptyWorkingSet(GetCurrentProcess());
    }
}
