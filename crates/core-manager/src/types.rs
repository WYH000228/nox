use crate::Map;
use ccp_shared::types::CUID;
use cpu_utils::pinning::pin_current_thread_to_cpuset;
use cpu_utils::{LogicalCoreId, PhysicalCoreId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, PartialEq, Eq, Hash, Clone, Serialize, Deserialize)]
pub enum WorkType {
    CapacityCommitment,
    Deal,
}

pub struct AcquireRequest {
    pub(crate) unit_ids: Vec<CUID>,
    pub(crate) worker_type: WorkType,
}

impl AcquireRequest {
    pub fn new(unit_ids: Vec<CUID>, worker_type: WorkType) -> Self {
        Self {
            unit_ids,
            worker_type,
        }
    }
}

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct Cores {
    pub physical_core_id: PhysicalCoreId,
    pub logical_core_ids: Vec<LogicalCoreId>,
}

#[derive(Debug, Eq, PartialEq, Clone)]
pub struct Assignment {
    pub physical_core_ids: BTreeSet<PhysicalCoreId>,
    pub logical_core_ids: BTreeSet<LogicalCoreId>,
    // We don't need a cryptographically secure hash and it is better to use a fx hash here
    // to improve performance
    pub cuid_cores: Map<CUID, Cores>,
}

impl Assignment {
    pub fn pin_current_thread(&self) {
        pin_current_thread_to_cpuset(self.logical_core_ids.iter().cloned());
    }
}
