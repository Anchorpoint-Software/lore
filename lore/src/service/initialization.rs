use lore_base::error::InvalidPath;
use lore_base::runtime::LORE_CONTEXT;
use lore_error_set::ForwardStrict;
use lore_error_set::error_set;
use lore_revision::event::LoreErrorDetail;
use lore_revision::fs::swfs::mount_manager_state::MountManagerState;
use lore_revision::lore::execution_context;

use crate::call::setup_execution;
use crate::interface::LoreEvent;
use crate::interface::LoreEventCallback;
use crate::interface::LoreGlobalArgs;

#[error_set]
pub enum ServiceInitializationError {
    InvalidPath,
}

pub async fn initialize_service(globals: LoreGlobalArgs) -> Result<(), ServiceInitializationError> {
    let callback: LoreEventCallback = Some(Box::new(|event| match event {
        LoreEvent::Log(data) => {
            println!("Service log: {:?}", data.message.as_str());
        }
        LoreEvent::Error(data) => {
            println!("Service error: {:?}", data.error_inner.as_str());
        }
        _ => {}
    }));
    let execution = setup_execution(globals, callback);

    LORE_CONTEXT
        .scope(execution, async move {
            let result = MountManagerState::initialize()
                .await
                .forward::<ServiceInitializationError>("Failed initializing SWFS repositories");
            let detail = LoreErrorDetail::from_result(result);

            execution_context().dispatcher.complete(detail).await
        })
        .await;

    Ok(())
}
