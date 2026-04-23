use std::sync::Arc;

use crate::{
    PreparedTrackRegistry, TrackDebugQueryService, TrackObservationService, TrackQueryService,
    TrackQueryStore, track_diagnostic_event_loader::TrackDiagnosticEventLoader,
    track_read_source_loader::TrackReadSourceLoader,
};

#[derive(Clone)]
pub struct TrackReadServices {
    query_service: Arc<TrackQueryService>,
    debug_query_service: Arc<TrackDebugQueryService>,
}

impl TrackReadServices {
    pub fn new(
        repository: Arc<dyn TrackQueryStore>,
        prepared_registry: Arc<PreparedTrackRegistry>,
        observation: Arc<TrackObservationService>,
    ) -> Self {
        let diagnostic_loader = Arc::new(TrackDiagnosticEventLoader::new(
            repository.clone(),
            observation.clone(),
        ));
        let loader = Arc::new(TrackReadSourceLoader::new(
            repository,
            prepared_registry,
            observation,
        ));
        Self {
            query_service: Arc::new(TrackQueryService::from_loader(loader.clone())),
            debug_query_service: Arc::new(TrackDebugQueryService::from_loader(diagnostic_loader)),
        }
    }

    pub fn query_service(&self) -> Arc<TrackQueryService> {
        self.query_service.clone()
    }

    pub fn debug_query_service(&self) -> Arc<TrackDebugQueryService> {
        self.debug_query_service.clone()
    }
}
