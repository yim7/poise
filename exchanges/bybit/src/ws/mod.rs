use std::sync::Arc;

use crate::{Deployment, rest::BybitRestClient};

pub struct BybitWsClient {
    rest: Arc<BybitRestClient>,
    deployment: Deployment,
}

impl BybitWsClient {
    pub fn new(rest: Arc<BybitRestClient>, deployment: Deployment) -> Self {
        Self { rest, deployment }
    }

    #[allow(dead_code)]
    pub(crate) fn rest(&self) -> &Arc<BybitRestClient> {
        &self.rest
    }

    #[allow(dead_code)]
    pub(crate) fn deployment(&self) -> &Deployment {
        &self.deployment
    }
}
