use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GridCommand {
    Pause,
    Resume,
    Reconcile,
}
