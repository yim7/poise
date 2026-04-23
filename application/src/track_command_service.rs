use std::sync::Arc;

use anyhow::Result;
use poise_engine::command::TrackCommand;
#[cfg(any(test, feature = "server-test-support"))]
use poise_engine::manager::TrackManager;
use poise_engine::transition::TrackTransition;
#[cfg(any(test, feature = "server-test-support"))]
use tokio::sync::RwLock;

use crate::mutation_executor::MutationExecutor;

#[derive(Clone)]
pub struct TrackCommandService {
    executor: Arc<MutationExecutor>,
}

impl TrackCommandService {
    pub(crate) fn from_executor(executor: Arc<MutationExecutor>) -> Self {
        Self { executor }
    }

    pub async fn has_track(&self, id: &str) -> bool {
        self.executor.has_track(id).await
    }

    pub async fn command(&self, id: &str, command: TrackCommand) -> Result<TrackTransition> {
        self.executor.command(id, command).await
    }

    #[cfg(any(test, feature = "server-test-support"))]
    pub fn manager(&self) -> Arc<RwLock<TrackManager>> {
        self.executor.manager()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use poise_engine::command::TrackCommand;
    use poise_engine::track::TrackId;

    use super::TrackCommandService;
    use crate::{
        ApplicationNotification, PersistedControlMode, TrackControlState, TrackQueryStore,
    };
    use crate::mutation_executor::test_support::{
        MemoryRepository, seeded_manager, track_write_services,
    };

    #[tokio::test]
    async fn command_service_pause_persists_state_and_notifies() {
        let repository = Arc::new(MemoryRepository::default());
        let service = test_service(repository.clone());
        let mut receiver = service.1.subscribe();

        service
            .0
            .command("btc-core", TrackCommand::Pause)
            .await
            .unwrap();

        assert_eq!(
            receiver.recv().await.unwrap(),
            ApplicationNotification::TrackChanged {
                track_id: TrackId::new("btc-core"),
            }
        );
        assert_eq!(
            service
                .0
                .manager()
                .read()
                .await
                .snapshot("btc-core")
                .unwrap()
                .status(),
            poise_engine::runtime::TrackStatus::Paused
        );
        assert_eq!(
            <MemoryRepository as TrackQueryStore>::load_track_persistent_state(
                repository.as_ref(),
                &TrackId::new("btc-core"),
            )
            .await
            .unwrap()
            .unwrap()
            .control_state,
            TrackControlState::Paused {
                resume_mode: PersistedControlMode::Automatic,
            }
        );
    }

    fn test_service(
        repository: Arc<MemoryRepository>,
    ) -> (
        TrackCommandService,
        tokio::sync::broadcast::Sender<ApplicationNotification>,
    ) {
        let (services, notifications) = track_write_services(seeded_manager(), repository);
        (services.command, notifications)
    }
}
