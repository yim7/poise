use std::sync::Arc;

use crate::config::Endpoints;
use crate::rest::BybitRestClient;

pub struct BybitWsClient {
    rest: Arc<BybitRestClient>,
    endpoints: Endpoints,
}

impl BybitWsClient {
    pub fn new(rest: Arc<BybitRestClient>, endpoints: Endpoints) -> Self {
        Self { rest, endpoints }
    }

    #[allow(dead_code)]
    pub(crate) fn rest(&self) -> &Arc<BybitRestClient> {
        &self.rest
    }

    #[allow(dead_code)]
    pub(crate) fn endpoints(&self) -> &Endpoints {
        &self.endpoints
    }
}
