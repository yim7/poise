use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrackCommand {
    Pause,
    Resume,
    Reconcile,
    Terminate,
    Flatten,
}
