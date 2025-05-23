use serde::Serialize;
use std::{
    cmp::{Eq, Ordering, PartialEq},
    net::SocketAddr,
    time::SystemTime,
};
use tokio::sync::OwnedSemaphorePermit;

use crate::balancer::status_update::StatusUpdate;

#[derive(Debug, Serialize)]
pub struct UpstreamPeer {
    pub agent_id: String,
    pub agent_name: Option<String>,
    pub error: Option<String>,
    pub external_llamacpp_addr: SocketAddr,
    /// None means undetermined, probably due to an error
    pub is_authorized: Option<bool>,
    /// None means undetermined, probably due to an error
    pub is_slots_endpoint_enabled: Option<bool>,
    pub last_update: SystemTime,
    pub quarantined_until: Option<SystemTime>,
    pub slots_idle: usize,
    pub slots_processing: usize,
    #[serde(skip_serializing)]
    pub slots_permissions: Option<OwnedSemaphorePermit>,
}

pub struct UpstreamPeerInfo {
    pub agent_id: String,
    pub external_llamacpp_addr: SocketAddr,
    pub last_update: SystemTime,
}

impl UpstreamPeer {
    pub fn new(
        agent_id: String,
        agent_name: Option<String>,
        error: Option<String>,
        external_llamacpp_addr: SocketAddr,
        is_authorized: Option<bool>,
        is_slots_endpoint_enabled: Option<bool>,
        slots_idle: usize,
        slots_processing: usize,
    ) -> Self {
        UpstreamPeer {
            agent_id,
            agent_name,
            error,
            external_llamacpp_addr,
            is_authorized,
            is_slots_endpoint_enabled,
            last_update: SystemTime::now(),
            quarantined_until: None,
            slots_idle,
            slots_processing,
            slots_permissions: None,
        }
    }

    pub fn new_from_status_update(agent_id: String, status_update: StatusUpdate) -> Self {
        Self::new(
            agent_id,
            status_update.agent_name.to_owned(),
            status_update.error.to_owned(),
            status_update.external_llamacpp_addr,
            status_update.is_authorized,
            status_update.is_slots_endpoint_enabled,
            status_update.idle_slots_count,
            status_update.processing_slots_count,
        )
    }

    pub fn info(&self) -> UpstreamPeerInfo {
        UpstreamPeerInfo {
            agent_id: self.agent_id.clone(),
            external_llamacpp_addr: self.external_llamacpp_addr,
            last_update: self.last_update,
        }
    }

    pub fn is_usable(&self) -> bool {
        self.slots_idle > 0
            && self.quarantined_until.is_none()
            && self.error.is_none()
            && matches!(self.is_authorized, Some(true))
    }

    pub fn release_slot(&mut self) {
        self.last_update = SystemTime::now();
        self.slots_idle += 1;
        self.slots_processing -= 1;
    }

    pub fn release_permits(&mut self, n: usize) {
        self.slots_permissions.as_mut().unwrap().split(n);
    }

    pub fn update_status(&mut self, status_update: StatusUpdate) {
        self.agent_name = status_update.agent_name.to_owned();
        self.error = status_update.error.to_owned();
        self.external_llamacpp_addr = status_update.external_llamacpp_addr;
        self.is_authorized = status_update.is_authorized;
        self.is_slots_endpoint_enabled = status_update.is_slots_endpoint_enabled;
        self.last_update = SystemTime::now();
        self.quarantined_until = None;

        if status_update.processing_slots_count < self.slots_processing {
            let slots_to_release = self.slots_processing - status_update.processing_slots_count;
            self.release_permits(slots_to_release);
        }

        self.slots_idle = status_update.idle_slots_count;
        self.slots_processing = status_update.processing_slots_count;
    }

    pub fn take_slot(&mut self) {
        self.last_update = SystemTime::now();
        self.slots_idle -= 1;
        self.slots_processing += 1;
    }

    pub fn store_permit(&mut self, permit: OwnedSemaphorePermit) {
        if let Some(permits_store) = self.slots_permissions.as_mut() {
            permits_store.merge(permit);
        } else {
            self.slots_permissions = Some(permit);
        }
    }

    pub fn slots_count(&self) -> usize {
        self.slots_idle + self.slots_processing
    }
}

impl Ord for UpstreamPeer {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .is_usable()
            .cmp(&self.is_usable())
            .then_with(|| other.slots_idle.cmp(&self.slots_idle))
            .then_with(|| self.slots_processing.cmp(&other.slots_processing))
            // compare by addr for stable sorting
            .then_with(|| {
                self.external_llamacpp_addr
                    .cmp(&other.external_llamacpp_addr)
            })
    }
}

impl PartialEq for UpstreamPeer {
    fn eq(&self, other: &Self) -> bool {
        self.agent_id == other.agent_id
    }
}

impl Eq for UpstreamPeer {}

impl PartialOrd for UpstreamPeer {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
