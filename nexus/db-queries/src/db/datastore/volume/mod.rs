// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#[cfg(any(test, feature = "testing"))]
use std::collections::HashSet;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::net::SocketAddrV6;

use anyhow::Result;
use anyhow::bail;
use chrono::DateTime;
use chrono::Utc;
use uuid::Uuid;

use crate::db::model;
use nexus_types::identity::Asset;
use omicron_uuid_kinds::VolumeUuid;
use sled_agent_client::VolumeConstructionRequest;

mod datastore;
mod replacement;
mod test;

pub use datastore::*;
pub use replacement::*;

#[derive(Debug, Clone)]
pub enum Volume {
    V1(VolumeV1),
    // XXX what about InMemoryOnly { volume_id, vcr } ?
}

#[derive(Debug, Clone)]
pub struct VolumeV1 {
    db_model: model::Volume,
    // XXX state: requires randomization?
    volume_construction_request: VolumeConstructionRequest,
}

// XXX comments should say volume instead of volume construction request?
impl Volume {
    pub fn new(
        db_model: model::Volume,
        volume_construction_request: VolumeConstructionRequest,
    ) -> Volume {
        Volume::V1(VolumeV1 { db_model, volume_construction_request })
    }

    #[cfg(test)]
    pub fn new_from_only_construction_request(
        volume_construction_request: VolumeConstructionRequest,
    ) -> Volume {
        let vcr = serde_json::to_string(&volume_construction_request).unwrap();
        Volume::V1(VolumeV1 {
            db_model: model::Volume::new(VolumeUuid::new_v4(), vcr),
            volume_construction_request,
        })
    }

    /// Create a new Volume based on the consumed Volume's construction request,
    /// setting a new ID.
    pub fn new_id(
        self,
        new_id: VolumeUuid,
    ) -> Result<Volume, serde_json::Error> {
        match self {
            Volume::V1(volume) => {
                let vcr_string =
                    serde_json::to_string(&volume.volume_construction_request)?;
                Ok(Volume::new(
                    model::Volume::new(new_id, vcr_string),
                    volume.volume_construction_request,
                ))
            }
        }
    }

    // XXX should this consume the higher level type, updated the model with the
    // serialized VCR, and return that?
    pub fn model(&self) -> &model::Volume {
        match self {
            Volume::V1(volume) => &volume.db_model,
        }
    }

    /// Consume the object and return the underlying VolumeConstructionRequest
    // XXX why does this consume the object?
    pub fn volume_construction_request(self) -> VolumeConstructionRequest {
        match self {
            Volume::V1(volume) => volume.volume_construction_request,
        }
    }

    /// Consume the object and return the underlying VolumeConstructionRequest
    /// converted to a crucible_pantry_client::types::VolumeConstructionRequest
    pub fn crucible_pantry_volume_construction_request(
        self,
    ) -> crucible_pantry_client::types::VolumeConstructionRequest {
        // XXX ugly!
        let intermediary =
            serde_json::to_string(&self.volume_construction_request()).unwrap();

        serde_json::from_str(&intermediary).unwrap()
    }

    pub fn id(&self) -> VolumeUuid {
        self.model().id()
    }

    pub fn time_created(&self) -> DateTime<Utc> {
        self.model().time_created()
    }

    pub fn time_modified(&self) -> DateTime<Utc> {
        self.model().time_modified()
    }

    pub fn time_deleted(&self) -> Option<DateTime<Utc>> {
        self.model().time_deleted
    }

    pub fn resources_to_clean_up(&self) -> Option<&str> {
        self.model().resources_to_clean_up.as_deref()
    }

    pub fn generation(&self) -> i64 {
        self.model().generation()
    }

    /// Returns true if the sub-volumes of a Volume are all read-only
    pub fn read_only(&self) -> bool {
        match self {
            Volume::V1(volume) => volume.read_only(),
        }
    }

    pub fn read_only_resources_associated_with_volume(
        &self,
        crucible_targets: &mut CrucibleTargets,
    ) {
        match self {
            Volume::V1(volume) => volume
                .read_only_resources_associated_with_volume(crucible_targets),
        }
    }

    pub fn read_write_resources_associated_with_volume(
        &self,
        targets: &mut Vec<String>,
    ) {
        match self {
            Volume::V1(volume) => {
                volume.read_write_resources_associated_with_volume(targets)
            }
        }
    }

    // XXX comment about returns true when volume update required
    // XXX consume self?
    // XXX clearly state this is about the VCR gen numbers, not the model
    pub fn bump_generation_numbers(&mut self) -> bool {
        match self {
            Volume::V1(volume) => volume.bump_generation_numbers(),
        }
    }

    pub fn data(&self) -> Result<String, serde_json::Error> {
        match self {
            Volume::V1(volume) => volume.data(),
        }
    }

    pub fn count_read_write_sub_volumes(&self) -> usize {
        match self {
            Volume::V1(volume) => volume.count_read_write_sub_volumes(),
        }
    }

    /// Find Regions in a Volume's subvolumes list whose target match the
    /// argument IP, and add them to the supplied Vec.
    fn find_matching_rw_regions_in_volume(
        &self,
        ip: &std::net::Ipv6Addr,
        matched_targets: &mut Vec<SocketAddrV6>,
    ) {
        match self {
            Volume::V1(volume) => {
                volume.find_matching_rw_regions_in_volume(ip, matched_targets)
            }
        }
    }

    /// Collect all the region sets present in the volume
    fn region_sets(&self, region_sets: &mut Vec<Vec<SocketAddrV6>>) {
        match self {
            Volume::V1(volume) => volume.region_sets(region_sets),
        }
    }

    /// Check if an ipv6 address is referenced in a Volume Construction Request
    fn ipv6_addr_referenced_in_vcr(&self, ip: &std::net::Ipv6Addr) -> bool {
        match self {
            Volume::V1(volume) => volume.ipv6_addr_referenced_in_vcr(ip),
        }
    }

    /// Check if an ipv6 net is referenced in a Volume Construction Request
    fn ipv6_net_referenced_in_vcr(&self, net: &oxnet::Ipv6Net) -> bool {
        match self {
            Volume::V1(volume) => volume.ipv6_net_referenced_in_vcr(net),
        }
    }

    /// Check if a region (represented by an ipv6 address) is present in a
    /// Volume Construction Request
    pub fn region_in_vcr(&self, region: &SocketAddrV6) -> bool {
        match self {
            Volume::V1(volume) => volume.region_in_vcr(region),
        }
    }

    /// Check if a read-only target is present anywhere in a Volume Construction
    /// Request
    pub fn read_only_target_in_vcr(
        &self,
        read_only_target: &SocketAddrV6,
    ) -> bool {
        match self {
            Volume::V1(volume) => {
                volume.read_only_target_in_vcr(read_only_target)
            }
        }
    }

    /// Create new UUIDs for the volume construction request layers
    pub fn randomize_ids(&mut self) -> Result<()> {
        match self {
            Volume::V1(volume) => volume.randomize_ids(),
        }
    }

    /// Returns true if ROP moved from us to the other Volume
    pub fn move_rop(&mut self, other: &mut Volume) -> Result<bool> {
        match (self, other) {
            (Volume::V1(volume), Volume::V1(other)) => volume.move_rop(other),
        }
    }

    /// Replace a Region in a VolumeConstructionRequest
    ///
    /// Note that UUIDs are not randomized by this step: Crucible will reject a
    /// `target_replace` call if the replacement VolumeConstructionRequest does
    /// not exactly match the original, except for a single Region difference.
    ///
    /// Note that the generation number _is_ bumped in this step, otherwise
    /// `compare_vcr_for_update` will reject the update.
    pub fn replace_region_in_vcr(
        &mut self,
        old_region: SocketAddrV6,
        new_region: SocketAddrV6,
    ) -> anyhow::Result<()> {
        match self {
            Volume::V1(volume) => {
                volume.replace_region_in_vcr(old_region, new_region)
            }
        }
    }

    /// Replace a read-only target in a VolumeConstructionRequest, returning how
    /// many times old_target appeared and was replaced with new_target.
    ///
    /// Note that UUIDs are not randomized by this step: Crucible will reject a
    /// `target_replace` call if the replacement VolumeConstructionRequest does
    /// not exactly match the original, except for a single Region difference.
    ///
    /// Note that the generation number _is not_ bumped in this step.
    pub fn replace_read_only_target_in_vcr(
        &mut self,
        old_target: ExistingTarget,
        new_target: ReplacementTarget,
    ) -> anyhow::Result<usize> {
        match self {
            Volume::V1(volume) => {
                volume.replace_read_only_target_in_vcr(old_target, new_target)
            }
        }
    }

    /// Currently used only for tests, zero out the generation number for a
    /// volume construction request to allow for an assert_eq test.
    #[cfg(any(test, feature = "testing"))]
    pub fn zero_out_gen_number(&mut self) {
        match self {
            Volume::V1(volume) => volume.zero_out_gen_number(),
        }
    }

    /// Currently used only for tests, gather all the unique IDs present in the
    /// volume and ensure there is no overlap
    #[cfg(any(test, feature = "testing"))]
    pub fn gather_ids(&self, ids: &mut HashSet<Uuid>) {
        match self {
            Volume::V1(volume) => volume.gather_ids(ids),
        }
    }
}

impl VolumeV1 {
    /// Returns true if the sub-volumes of a Volume are all read-only
    pub fn read_only(&self) -> bool {
        match &self.volume_construction_request {
            VolumeConstructionRequest::Volume { sub_volumes, .. } => {
                for sv in sub_volumes {
                    match sv {
                        VolumeConstructionRequest::Region { opts, .. } => {
                            if !opts.read_only {
                                return false;
                            }
                        }

                        _ => {}
                    }
                }

                true
            }

            VolumeConstructionRequest::Region { opts, .. } => opts.read_only,

            VolumeConstructionRequest::File { .. } => {
                // Effectively, this is read-only, as this BlockIO
                // implementation does not have a `write` implementation,
                true
            }

            VolumeConstructionRequest::Url { .. } => {
                // ImageSource::Url was deprecated but its BlockIO
                // implementation also does not have a `write` implementation.
                true
            }
        }
    }

    /// Return the read-only targets from a VolumeConstructionRequest.
    ///
    /// The targets of a volume construction request map to resources.
    pub fn read_only_resources_associated_with_volume(
        &self,
        crucible_targets: &mut CrucibleTargets,
    ) {
        let mut parts: VecDeque<&VolumeConstructionRequest> = VecDeque::new();
        parts.push_back(&self.volume_construction_request);

        while let Some(vcr_part) = parts.pop_front() {
            match vcr_part {
                VolumeConstructionRequest::Volume {
                    sub_volumes,
                    read_only_parent,
                    ..
                } => {
                    for sub_volume in sub_volumes {
                        parts.push_back(sub_volume);
                    }

                    if let Some(read_only_parent) = read_only_parent {
                        parts.push_back(read_only_parent);
                    }
                }

                VolumeConstructionRequest::Url { .. } => {
                    // no action required
                }

                VolumeConstructionRequest::Region { opts, .. } => {
                    for target in &opts.target {
                        if opts.read_only {
                            crucible_targets
                                .read_only_targets
                                .push(target.to_string());
                        }
                    }
                }

                VolumeConstructionRequest::File { .. } => {
                    // no action required
                }
            }
        }
    }

    /// Return the read-write targets from a VolumeConstructionRequest.
    ///
    /// The targets of a volume construction request map to resources.
    pub fn read_write_resources_associated_with_volume(
        &self,
        targets: &mut Vec<String>,
    ) {
        // XXX doesn't need VecDeque?
        let mut parts: VecDeque<&VolumeConstructionRequest> = VecDeque::new();
        parts.push_back(&self.volume_construction_request);

        while let Some(vcr_part) = parts.pop_front() {
            match vcr_part {
                VolumeConstructionRequest::Volume { sub_volumes, .. } => {
                    for sub_volume in sub_volumes {
                        parts.push_back(sub_volume);
                    }

                    // No need to look under read-only parent
                }

                VolumeConstructionRequest::Url { .. } => {
                    // no action required
                }

                VolumeConstructionRequest::Region { opts, .. } => {
                    if !opts.read_only {
                        for target in &opts.target {
                            targets.push(target.to_string());
                        }
                    }
                }

                VolumeConstructionRequest::File { .. } => {
                    // no action required
                }
            }
        }
    }

    pub fn bump_generation_numbers(&mut self) -> bool {
        // Look to see if the VCR is a Volume type, and if so, look at its
        // sub_volumes. If they are of type Region, then we need to update their
        // generation numbers.
        //
        // XXX comment about only top level in sub volume
        match &mut self.volume_construction_request {
            VolumeConstructionRequest::Volume { sub_volumes, .. } => {
                let mut updated = false;

                for sv in sub_volumes {
                    match sv {
                        VolumeConstructionRequest::Region {
                            generation,
                            ..
                        } => {
                            updated = true;
                            *generation += 1;
                        }

                        _ => {}
                    }
                }

                updated
            }

            VolumeConstructionRequest::Region { .. }
            | VolumeConstructionRequest::File { .. }
            | VolumeConstructionRequest::Url { .. } => false,
        }
    }

    pub fn data(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(&self.volume_construction_request)
    }

    pub fn count_read_write_sub_volumes(&self) -> usize {
        match &self.volume_construction_request {
            VolumeConstructionRequest::Volume { sub_volumes, .. } => {
                sub_volumes.len()
            }

            VolumeConstructionRequest::Url { .. }
            | VolumeConstructionRequest::Region { .. }
            | VolumeConstructionRequest::File { .. } => 0,
        }
    }

    /// Find Regions in a Volume's subvolumes list whose target match the
    /// argument IP, and add them to the supplied Vec.
    fn find_matching_rw_regions_in_volume(
        &self,
        ip: &std::net::Ipv6Addr,
        matched_targets: &mut Vec<SocketAddrV6>,
    ) {
        let mut parts: VecDeque<&VolumeConstructionRequest> = VecDeque::new();
        parts.push_back(&self.volume_construction_request);

        while let Some(vcr_part) = parts.pop_front() {
            match vcr_part {
                VolumeConstructionRequest::Volume { sub_volumes, .. } => {
                    for sub_volume in sub_volumes {
                        parts.push_back(sub_volume);
                    }
                }

                VolumeConstructionRequest::Url { .. } => {
                    // nothing required
                }

                VolumeConstructionRequest::Region { opts, .. } => {
                    if !opts.read_only {
                        for target in &opts.target {
                            if let SocketAddr::V6(target) = target {
                                if target.ip() == ip {
                                    matched_targets.push(*target);
                                }
                            }
                        }
                    }
                }

                VolumeConstructionRequest::File { .. } => {
                    // nothing required
                }
            }
        }
    }

    /// Collect all the region sets present in the volume
    fn region_sets(&self, region_sets: &mut Vec<Vec<SocketAddrV6>>) {
        let mut parts: VecDeque<&VolumeConstructionRequest> = VecDeque::new();
        parts.push_back(&self.volume_construction_request);

        while let Some(work) = parts.pop_front() {
            match work {
                VolumeConstructionRequest::Volume {
                    sub_volumes,
                    read_only_parent,
                    ..
                } => {
                    for sub_volume in sub_volumes {
                        parts.push_back(&sub_volume);
                    }

                    if let Some(read_only_parent) = read_only_parent {
                        parts.push_back(&read_only_parent);
                    }
                }

                VolumeConstructionRequest::Url { .. } => {
                    // nothing required
                }

                VolumeConstructionRequest::Region { opts, .. } => {
                    let mut targets = vec![];

                    for target in &opts.target {
                        match target {
                            SocketAddr::V6(v6) => {
                                targets.push(*v6);
                            }

                            // XXX support v4? or bail?
                            SocketAddr::V4(_) => {}
                        }
                    }

                    region_sets.push(targets);
                }

                VolumeConstructionRequest::File { .. } => {
                    // nothing required
                }
            }
        }
    }

    /// Check if an ipv6 address is referenced in a Volume Construction Request
    fn ipv6_addr_referenced_in_vcr(&self, ip: &std::net::Ipv6Addr) -> bool {
        let mut parts: VecDeque<&VolumeConstructionRequest> = VecDeque::new();
        parts.push_back(&self.volume_construction_request);

        while let Some(vcr_part) = parts.pop_front() {
            match vcr_part {
                VolumeConstructionRequest::Volume {
                    sub_volumes,
                    read_only_parent,
                    ..
                } => {
                    for sub_volume in sub_volumes {
                        parts.push_back(sub_volume);
                    }

                    if let Some(read_only_parent) = read_only_parent {
                        parts.push_back(read_only_parent);
                    }
                }

                VolumeConstructionRequest::Url { .. } => {
                    // nothing required
                }

                VolumeConstructionRequest::Region { opts, .. } => {
                    for target in &opts.target {
                        match target {
                            SocketAddr::V6(t) => {
                                if t.ip() == ip {
                                    return true;
                                }
                            }

                            SocketAddr::V4(_) => {}
                        }
                    }
                }

                VolumeConstructionRequest::File { .. } => {
                    // nothing required
                }
            }
        }

        false
    }

    /// Check if an ipv6 net is referenced in a Volume Construction Request
    fn ipv6_net_referenced_in_vcr(&self, net: &oxnet::Ipv6Net) -> bool {
        let mut parts: VecDeque<&VolumeConstructionRequest> = VecDeque::new();
        parts.push_back(&self.volume_construction_request);

        while let Some(vcr_part) = parts.pop_front() {
            match vcr_part {
                VolumeConstructionRequest::Volume {
                    sub_volumes,
                    read_only_parent,
                    ..
                } => {
                    for sub_volume in sub_volumes {
                        parts.push_back(sub_volume);
                    }

                    if let Some(read_only_parent) = read_only_parent {
                        parts.push_back(read_only_parent);
                    }
                }

                VolumeConstructionRequest::Url { .. } => {
                    // nothing required
                }

                VolumeConstructionRequest::Region { opts, .. } => {
                    for target in &opts.target {
                        match target {
                            SocketAddr::V6(t) => {
                                if net.contains(*t.ip()) {
                                    return true;
                                }
                            }

                            SocketAddr::V4(_) => {}
                        }
                    }
                }

                VolumeConstructionRequest::File { .. } => {
                    // nothing required
                }
            }
        }

        false
    }

    /// Check if a region (represented by an ipv6 address) is present in a
    /// Volume Construction Request
    pub fn region_in_vcr(&self, region: &SocketAddrV6) -> bool {
        let mut parts: VecDeque<&VolumeConstructionRequest> = VecDeque::new();
        parts.push_back(&self.volume_construction_request);

        let mut region_found = false;

        while let Some(vcr_part) = parts.pop_front() {
            match vcr_part {
                VolumeConstructionRequest::Volume { sub_volumes, .. } => {
                    for sub_volume in sub_volumes {
                        parts.push_back(sub_volume);
                    }

                    // Skip looking at read-only parent, this function only
                    // looks for R/W regions
                }

                VolumeConstructionRequest::Url { .. } => {
                    // nothing required
                }

                VolumeConstructionRequest::Region { opts, .. } => {
                    for target in &opts.target {
                        match target {
                            SocketAddr::V6(t) if *t == *region => {
                                region_found = true;
                                break;
                            }

                            SocketAddr::V6(_) | SocketAddr::V4(_) => {}
                        }
                    }
                }

                VolumeConstructionRequest::File { .. } => {
                    // nothing required
                }
            }
        }

        region_found
    }

    /// Check if a read-only target is present anywhere in a Volume Construction
    /// Request
    pub fn read_only_target_in_vcr(
        &self,
        read_only_target: &SocketAddrV6,
    ) -> bool {
        struct Work<'a> {
            vcr_part: &'a VolumeConstructionRequest,
            // XXX needed? why isn't this used?
            under_read_only_parent: bool,
        }

        let mut parts: VecDeque<Work> = VecDeque::new();
        parts.push_back(Work {
            vcr_part: &self.volume_construction_request,
            under_read_only_parent: false,
        });

        while let Some(work) = parts.pop_front() {
            match work.vcr_part {
                VolumeConstructionRequest::Volume {
                    sub_volumes,
                    read_only_parent,
                    ..
                } => {
                    for sub_volume in sub_volumes {
                        parts.push_back(Work {
                            vcr_part: &sub_volume,
                            under_read_only_parent: work.under_read_only_parent,
                        });
                    }

                    if let Some(read_only_parent) = read_only_parent {
                        parts.push_back(Work {
                            vcr_part: &read_only_parent,
                            under_read_only_parent: true,
                        });
                    }
                }

                VolumeConstructionRequest::Url { .. } => {
                    // nothing required
                }

                VolumeConstructionRequest::Region { opts, .. } => {
                    /*
                    if work.under_read_only_parent && !opts.read_only {
                        // This VCR isn't constructed properly, there's a
                        // read/write region under a read-only parent
                        bail!("read-write region under read-only parent");
                    }
                    */

                    for target in &opts.target {
                        match target {
                            SocketAddr::V6(t)
                                if *t == *read_only_target
                                    && opts.read_only =>
                            {
                                return true;
                            }

                            SocketAddr::V6(_) | SocketAddr::V4(_) => {}
                        }
                    }
                }

                VolumeConstructionRequest::File { .. } => {
                    // nothing required
                }
            }
        }

        false
    }

    /// Create new UUIDs for the volume construction request layers
    pub fn randomize_ids(&mut self) -> Result<()> {
        let mut parts: VecDeque<&mut VolumeConstructionRequest> =
            VecDeque::new();
        parts.push_back(&mut self.volume_construction_request);

        while let Some(vcr_part) = parts.pop_front() {
            match vcr_part {
                VolumeConstructionRequest::Volume {
                    id,
                    sub_volumes,
                    read_only_parent,
                    ..
                } => {
                    *id = Uuid::new_v4();

                    for sub_volume in sub_volumes {
                        parts.push_back(sub_volume);
                    }

                    if let Some(read_only_parent) = read_only_parent {
                        parts.push_back(read_only_parent);
                    }
                }

                VolumeConstructionRequest::Url { id, .. } => {
                    *id = Uuid::new_v4();
                }

                VolumeConstructionRequest::Region { opts, .. } => {
                    if !opts.read_only {
                        // Only one volume can "own" a Region, and that volume's
                        // UUID is recorded in the region table accordingly. It
                        // is an error to make a copy of a volume construction
                        // request that references non-read-only Regions.
                        //
                        // XXX is it possible to prevent this during creation
                        // instead?
                        bail!(
                            "only one Volume can reference a Region \
                            non-read-only!"
                        );
                    }

                    opts.id = Uuid::new_v4();
                }

                VolumeConstructionRequest::File { id, .. } => {
                    *id = Uuid::new_v4();
                }
            }
        }

        Ok(())
    }

    /// Returns true if ROP moved from us to the other Volume
    //
    // XXX return a richer enum type - what if the temp volume
    // id has no rop? what if it does?
    pub fn move_rop(&mut self, other: &mut VolumeV1) -> Result<bool> {
        match &mut self.volume_construction_request {
            VolumeConstructionRequest::Volume { read_only_parent, .. } => {
                if read_only_parent.is_none() {
                    // This volume has no read_only_parent
                    //
                    // XXX assume movement has occurred already?

                    Ok(false)
                } else {
                    match &mut other.volume_construction_request {
                        VolumeConstructionRequest::Volume {
                            read_only_parent: other_read_only_parent,
                            ..
                        } => {
                            // XXX note this clobbers the other volume's
                            // read-only parent if it existed before
                            *other_read_only_parent = read_only_parent.take();
                        }

                        _ => {
                            bail!(
                                "Target volume not a suitable variant for \
                                read-only parent movement"
                            );
                        }
                    }

                    Ok(true)
                }
            }

            _ => {
                bail!(
                    "Source volume not a suitable variant for read-only \
                    parent movement"
                );
            }
        }
    }

    /// Replace a Region in a VolumeConstructionRequest
    ///
    /// Note that UUIDs are not randomized by this step: Crucible will reject a
    /// `target_replace` call if the replacement VolumeConstructionRequest does
    /// not exactly match the original, except for a single Region difference.
    ///
    /// Note that the generation number _is_ bumped in this step, otherwise
    /// `compare_vcr_for_update` will reject the update.
    pub fn replace_region_in_vcr(
        &mut self,
        old_region: SocketAddrV6,
        new_region: SocketAddrV6,
    ) -> anyhow::Result<()> {
        let mut parts: VecDeque<&mut VolumeConstructionRequest> =
            VecDeque::new();
        parts.push_back(&mut self.volume_construction_request);

        let mut old_region_found = false;

        while let Some(vcr_part) = parts.pop_front() {
            match vcr_part {
                VolumeConstructionRequest::Volume { sub_volumes, .. } => {
                    for sub_volume in sub_volumes {
                        parts.push_back(sub_volume);
                    }

                    // Skip looking at read-only parent, this function only
                    // replaces R/W regions
                }

                VolumeConstructionRequest::Url { .. } => {
                    // nothing required
                }

                VolumeConstructionRequest::Region {
                    opts, generation, ..
                } => {
                    for target in &mut opts.target {
                        if let SocketAddr::V6(target) = target {
                            if *target == old_region {
                                *target = new_region;
                                old_region_found = true;
                            }
                        }
                    }

                    // Bump generation number, otherwise update will be rejected
                    *generation += 1;
                }

                VolumeConstructionRequest::File { .. } => {
                    // nothing required
                }
            }
        }

        if !old_region_found {
            bail!("old region {old_region} not found!");
        }

        Ok(())
    }

    /// Replace a read-only target in a VolumeConstructionRequest, returning how
    /// many times old_target appeared and was replaced with new_target.
    ///
    /// Note that UUIDs are not randomized by this step: Crucible will reject a
    /// `target_replace` call if the replacement VolumeConstructionRequest does
    /// not exactly match the original, except for a single Region difference.
    ///
    /// Note that the generation number _is not_ bumped in this step.
    pub fn replace_read_only_target_in_vcr(
        &mut self,
        old_target: ExistingTarget,
        new_target: ReplacementTarget,
    ) -> anyhow::Result<usize> {
        struct Work<'a> {
            vcr_part: &'a mut VolumeConstructionRequest,
            under_read_only_parent: bool,
        }

        let mut parts: VecDeque<Work> = VecDeque::new();
        parts.push_back(Work {
            vcr_part: &mut self.volume_construction_request,
            under_read_only_parent: false,
        });

        let mut replacements = 0;

        while let Some(work) = parts.pop_front() {
            match work.vcr_part {
                VolumeConstructionRequest::Volume {
                    sub_volumes,
                    read_only_parent,
                    ..
                } => {
                    for sub_volume in sub_volumes {
                        parts.push_back(Work {
                            vcr_part: sub_volume,
                            under_read_only_parent: work.under_read_only_parent,
                        });
                    }

                    if let Some(read_only_parent) = read_only_parent {
                        parts.push_back(Work {
                            vcr_part: read_only_parent,
                            under_read_only_parent: true,
                        });
                    }
                }

                VolumeConstructionRequest::Url { .. } => {
                    // nothing required
                }

                VolumeConstructionRequest::Region { opts, .. } => {
                    if work.under_read_only_parent && !opts.read_only {
                        // This VCR isn't constructed properly, there's a
                        // read/write region under a read-only parent
                        bail!("read-write region under read-only parent");
                    }

                    for target in &mut opts.target {
                        if let SocketAddr::V6(target) = target {
                            if *target == old_target.0 && opts.read_only {
                                *target = new_target.0;
                                replacements += 1;
                            }
                        }
                    }
                }

                VolumeConstructionRequest::File { .. } => {
                    // nothing required
                }
            }
        }

        if replacements == 0 {
            bail!("target {old_target:?} not found!");
        }

        Ok(replacements)
    }

    /// Currently used only for tests, zero out the generation number for a
    /// volume construction request to allow for an assert_eq test.
    #[cfg(any(test, feature = "testing"))]
    pub fn zero_out_gen_number(&mut self) {
        let mut parts: VecDeque<&mut VolumeConstructionRequest> =
            VecDeque::new();
        parts.push_back(&mut self.volume_construction_request);

        while let Some(vcr_part) = parts.pop_front() {
            match vcr_part {
                VolumeConstructionRequest::Volume {
                    sub_volumes,
                    read_only_parent,
                    ..
                } => {
                    for sv in sub_volumes {
                        parts.push_back(sv);
                    }

                    if let Some(rop) = read_only_parent {
                        parts.push_back(rop);
                    }
                }

                VolumeConstructionRequest::Region { generation, .. } => {
                    *generation = 0;
                }

                _ => {}
            }
        }
    }

    /// Currently used only for tests, gather all the unique IDs present in the
    /// volume and ensure there is no overlap
    #[cfg(any(test, feature = "testing"))]
    pub fn gather_ids(&self, ids: &mut HashSet<Uuid>) {
        let mut parts: VecDeque<&VolumeConstructionRequest> = VecDeque::new();
        parts.push_back(&self.volume_construction_request);

        while let Some(vcr_part) = parts.pop_front() {
            match vcr_part {
                VolumeConstructionRequest::Volume {
                    sub_volumes,
                    read_only_parent,
                    ..
                } => {
                    // Do not insert the volume's ID as that will not be used
                    // when constructing upstairs, and this test is specifically
                    // trying to catch when upstairs IDs are reused.

                    for sub_volume in sub_volumes {
                        parts.push_back(sub_volume);
                    }

                    if let Some(read_only_parent) = read_only_parent {
                        parts.push_back(read_only_parent);
                    }
                }

                VolumeConstructionRequest::Region { opts, .. } => {
                    if !ids.insert(opts.id) {
                        // Panic if there is ID reuse in different region sets
                        // in the same VCR
                        panic!(
                            "ID {} used in more than one region set!",
                            opts.id
                        );
                    }
                }

                VolumeConstructionRequest::Url { .. }
                | VolumeConstructionRequest::File { .. } => {
                    panic!("should not be constructing these anymore");
                }
            }
        }
    }
}
