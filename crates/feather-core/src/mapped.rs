//! Read-only file mapping. The Windows path uses `CreateFileMappingW`; other
//! platforms fall back to `std::fs::read` so the core crate stays testable.

#[cfg(windows)]
mod imp {
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, GetFileSizeEx, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Memory::{
        CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_READ,
        MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READONLY,
    };

    pub struct MappedFile {
        file: HANDLE,
        mapping: HANDLE,
        base: MEMORY_MAPPED_VIEW_ADDRESS,
        len: usize,
    }

    impl MappedFile {
        pub fn open(path: &Path) -> io::Result<Self> {
            let wide: Vec<u16> = path
                .as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();
            let file = unsafe {
                CreateFileW(
                    wide.as_ptr(),
                    GENERIC_READ,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    FILE_ATTRIBUTE_NORMAL,
                    std::ptr::null_mut(),
                )
            };
            if file == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }

            let mut size = 0i64;
            if unsafe { GetFileSizeEx(file, &mut size) } == 0 {
                let err = io::Error::last_os_error();
                unsafe { CloseHandle(file) };
                return Err(err);
            }
            if size <= 0 {
                return Ok(Self {
                    file,
                    mapping: std::ptr::null_mut(),
                    base: MEMORY_MAPPED_VIEW_ADDRESS {
                        Value: std::ptr::null_mut(),
                    },
                    len: 0,
                });
            }

            let mapping = unsafe {
                CreateFileMappingW(
                    file,
                    std::ptr::null(),
                    PAGE_READONLY,
                    0,
                    0,
                    std::ptr::null(),
                )
            };
            if mapping.is_null() {
                let err = io::Error::last_os_error();
                unsafe { CloseHandle(file) };
                return Err(err);
            }

            let base = unsafe { MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, size as usize) };
            if base.Value.is_null() {
                let err = io::Error::last_os_error();
                unsafe {
                    CloseHandle(mapping);
                    CloseHandle(file);
                }
                return Err(err);
            }

            Ok(Self {
                file,
                mapping,
                base,
                len: size as usize,
            })
        }

        pub fn byte_len(&self) -> usize {
            self.len
        }

        pub fn as_slice(&self) -> &[u8] {
            if self.len == 0 {
                &[]
            } else {
                unsafe { std::slice::from_raw_parts(self.base.Value as *const u8, self.len) }
            }
        }
    }

    impl Drop for MappedFile {
        fn drop(&mut self) {
            unsafe {
                if !self.base.Value.is_null() {
                    UnmapViewOfFile(self.base);
                }
                if !self.mapping.is_null() {
                    CloseHandle(self.mapping);
                }
                if self.file != INVALID_HANDLE_VALUE {
                    CloseHandle(self.file);
                }
            }
        }
    }

    unsafe impl Send for MappedFile {}
    unsafe impl Sync for MappedFile {}
}

#[cfg(not(windows))]
mod imp {
    use std::fs::File;
    use std::io;
    use std::path::Path;

    pub struct MappedFile {
        map: memmap2::Mmap,
    }

    impl MappedFile {
        pub fn open(path: &Path) -> io::Result<Self> {
            let file = File::open(path)?;
            // SAFETY: the index files are never written while open; the
            // builder writes a new generation and swaps the library pointer.
            let map = unsafe { memmap2::Mmap::map(&file)? };
            Ok(Self { map })
        }

        pub fn byte_len(&self) -> usize {
            self.map.len()
        }

        pub fn as_slice(&self) -> &[u8] {
            &self.map
        }
    }
}

pub use imp::MappedFile;
