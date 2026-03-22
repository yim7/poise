use std::collections::BTreeMap;

use anyhow::{Result, ensure};
use serde::Serialize;

use crate::Application;

#[derive(Clone)]
pub struct ApplicationRegistry {
    environment: String,
    default_symbol: String,
    instances: BTreeMap<String, Application>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstanceSummary {
    pub symbol: String,
    pub environment: String,
    pub is_default: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstancesDirectory {
    pub environment: String,
    pub default_symbol: String,
    pub instances: Vec<InstanceSummary>,
}

impl ApplicationRegistry {
    pub fn new(
        environment: impl Into<String>,
        default_symbol: impl Into<String>,
        instances: Vec<(String, Application)>,
    ) -> Result<Self> {
        ensure!(
            !instances.is_empty(),
            "application registry must contain at least one instance"
        );
        let environment = environment.into();
        let default_symbol = normalize_symbol(default_symbol);
        let instances = instances
            .into_iter()
            .map(|(symbol, application)| (normalize_symbol(symbol), application))
            .collect::<BTreeMap<_, _>>();
        ensure!(
            instances.contains_key(&default_symbol),
            "default symbol `{default_symbol}` must exist in registry"
        );

        Ok(Self {
            environment,
            default_symbol,
            instances,
        })
    }

    pub fn single(application: Application) -> Self {
        let snapshot = application.snapshot();
        let symbol = snapshot.runtime.symbol.trim().to_ascii_uppercase();
        let environment = snapshot.runtime.env;
        Self {
            environment,
            default_symbol: symbol.clone(),
            instances: BTreeMap::from([(symbol, application)]),
        }
    }

    pub fn environment(&self) -> &str {
        &self.environment
    }

    pub fn default_symbol(&self) -> &str {
        &self.default_symbol
    }

    pub fn default_application(&self) -> Application {
        self.instances
            .get(&self.default_symbol)
            .cloned()
            .expect("default application must exist in registry")
    }

    pub fn application(&self, symbol: &str) -> Option<Application> {
        self.instances.get(&normalize_symbol(symbol)).cloned()
    }

    pub fn instances(&self) -> Vec<InstanceSummary> {
        self.instances
            .keys()
            .cloned()
            .map(|symbol| InstanceSummary {
                is_default: symbol == self.default_symbol,
                symbol,
                environment: self.environment.clone(),
            })
            .collect()
    }

    pub fn directory(&self) -> InstancesDirectory {
        InstancesDirectory {
            environment: self.environment.clone(),
            default_symbol: self.default_symbol.clone(),
            instances: self.instances(),
        }
    }
}

impl From<Application> for ApplicationRegistry {
    fn from(value: Application) -> Self {
        Self::single(value)
    }
}

fn normalize_symbol(value: impl Into<String>) -> String {
    value.into().trim().to_ascii_uppercase()
}
