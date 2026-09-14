use std::sync::Arc;

use async_trait::async_trait;

use crate::fs::filesystem_provider::FilesystemProvider;
use crate::fs::filesystem_provider::FsError;
use crate::fs::filesystem_provider::InstanceOperationImpl;

pub struct InvalidFilesystemProvider {}

impl InvalidFilesystemProvider {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {})
    }
}

#[async_trait]
impl FilesystemProvider for InvalidFilesystemProvider {
    async fn begin_operation(&self) -> Result<Arc<InstanceOperationImpl>, FsError> {
        Err(FsError::internal(
            "Attempting a filesystem operation on an invalid filesystem provider",
        ))
    }
}
