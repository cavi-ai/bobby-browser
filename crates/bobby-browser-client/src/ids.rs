//! Typed UUID newtypes used across the `/v1` wire contract.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! uuid_id {
    ($name:ident) => {
        #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
        #[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }
    };
}

uuid_id!(SessionId);
uuid_id!(PageId);
uuid_id!(CommandId);
uuid_id!(WorkflowId);
uuid_id!(AttemptId);
uuid_id!(EvidenceId);
uuid_id!(WorkerId);
uuid_id!(JobId);
uuid_id!(ArtifactId);
uuid_id!(CheckpointId);
uuid_id!(CompanionId);
uuid_id!(ProfileId);
uuid_id!(AttachmentId);
