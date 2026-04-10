use crate::config::Endpoints;

pub struct BybitRestClient {
    endpoints: Endpoints,
    api_key: String,
    api_secret: String,
}

impl BybitRestClient {
    pub fn new(endpoints: Endpoints, api_key: String, api_secret: String) -> Self {
        Self {
            endpoints,
            api_key,
            api_secret,
        }
    }

    #[allow(dead_code)]
    pub(crate) fn endpoints(&self) -> &Endpoints {
        &self.endpoints
    }

    #[allow(dead_code)]
    pub(crate) fn api_key(&self) -> &str {
        &self.api_key
    }

    #[allow(dead_code)]
    pub(crate) fn api_secret(&self) -> &str {
        &self.api_secret
    }
}
