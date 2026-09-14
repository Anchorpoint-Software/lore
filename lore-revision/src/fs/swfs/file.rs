use std::ffi::CString;

use crate::fs::filesystem_provider::FileInfo;
use crate::fs::swfs::api_interface::SwfsInterfaceError;
use crate::fs::swfs::api_interface::swfs_api::SWFSFInfo;
use crate::fs::swfs::api_interface::swfs_api::SWFSFile;
use crate::fs::swfs::api_interface::swfs_api::swfs_u16;

pub struct SwfsFileArray {
    files: Box<[SwfsFile]>,
}

impl SwfsFileArray {
    pub fn empty() -> Self {
        Self::new(Vec::new())
    }

    pub fn new(files: Vec<SwfsFile>) -> Self {
        let mut this = SwfsFileArray {
            files: files.into_boxed_slice(),
        };
        for i in 1..this.files.len() {
            this.files[i - 1].file.next = std::ptr::from_ref(this.files[i].file()) as *mut SWFSFile;
        }
        this
    }

    pub fn array_ptr(&self) -> *const SWFSFile {
        if self.files.is_empty() {
            std::ptr::null()
        } else {
            self.files.as_ptr().cast()
        }
    }
}

/// Binary equivalent wrapper of an `SWFSFile` that owns the memory for the file name.
#[repr(C)]
pub struct SwfsFile {
    file: SWFSFile,
}

impl Drop for SwfsFile {
    fn drop(&mut self) {
        unsafe {
            drop(CString::from_raw(self.file.path));
        }
    }
}

impl SwfsFile {
    pub fn new(
        name: CString,
        file_info: &FileInfo,
        flags: Option<swfs_u16>,
    ) -> Result<SwfsFile, SwfsInterfaceError> {
        let name = name.into_raw();
        Ok(Self {
            file: SWFSFile {
                path: name,
                finfo: SWFSFInfo {
                    time_create: file_info.mtime(),
                    time_write: file_info.mtime(),
                    size: file_info.size(),
                    attrs: if file_info.is_dir() {
                        // FILE_ATTRIBUTE_DIRECTORY
                        0x00000010
                    } else {
                        0
                    },
                },
                file_index: 0,
                mod_flags: flags.unwrap_or(0),
                next: std::ptr::null_mut(),
            },
        })
    }

    pub fn file(&self) -> &SWFSFile {
        &self.file
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::CString;
    use std::ffi::c_char;

    use crate::fs::filesystem_provider::FileInfo;
    use crate::fs::swfs::api_interface::swfs_api::SWFSFile;
    use crate::fs::swfs::file::SwfsFile;
    use crate::fs::swfs::file::SwfsFileArray;

    #[test]
    fn swfs_file_sizing() {
        let files = SwfsFileArray::new(vec![
            SwfsFile::new(CString::new("a").unwrap(), &FileInfo::NotExist, None).unwrap(),
            SwfsFile::new(CString::new("b").unwrap(), &FileInfo::NotExist, None).unwrap(),
        ]);
        let file_a = files.array_ptr();
        unsafe {
            let file_b = (*file_a).next;
            assert!(std::ptr::addr_eq(file_a.offset(1), file_b));
            assert_eq!(size_of::<SwfsFile>(), size_of::<SWFSFile>());
            assert_eq!(*(*file_a).path, 'a' as c_char);
            assert_eq!(*(*file_a).path.offset(1), '\0' as c_char);
            assert_eq!(*(*file_b).path, 'b' as c_char);
            assert_eq!(*(*file_b).path.offset(1), '\0' as c_char);
        }
    }
}
