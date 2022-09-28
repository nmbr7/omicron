// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! [`DataStore`] methods on [`Volume`]s.

use super::DataStore;
use crate::db;
use crate::db::error::public_error_from_diesel_pool;
use crate::db::error::ErrorHandler;
use crate::db::error::TransactionError;
use crate::db::identity::Asset;
use crate::db::model::Dataset;
use crate::db::model::Region;
use crate::db::model::RegionSnapshot;
use crate::db::model::Volume;
use async_bb8_diesel::AsyncConnection;
use async_bb8_diesel::AsyncRunQueryDsl;
use chrono::Utc;
use diesel::prelude::*;
use diesel::OptionalExtension as DieselOptionalExtension;
use omicron_common::api::external::CreateResult;
use omicron_common::api::external::DeleteResult;
use omicron_common::api::external::Error;
use omicron_common::api::external::ListResultVec;
use omicron_common::api::external::LookupResult;
use omicron_common::api::external::LookupType;
use omicron_common::api::external::ResourceType;
use serde::Deserialize;
use serde::Serialize;
use sled_agent_client::types::VolumeConstructionRequest;
use uuid::Uuid;

impl DataStore {
    pub async fn volume_create(&self, volume: Volume) -> CreateResult<Volume> {
        use db::schema::volume::dsl;

        #[derive(Debug, thiserror::Error)]
        enum VolumeCreationError {
            #[error("Error from Volume creation: {0}")]
            Public(Error),

            #[error("Serde error during Volume creation: {0}")]
            SerdeError(#[from] serde_json::Error),
        }
        type TxnError = TransactionError<VolumeCreationError>;

        self.pool()
            .transaction(move |conn| {
                let maybe_volume: Option<Volume> = dsl::volume
                    .filter(dsl::id.eq(volume.id()))
                    .select(Volume::as_select())
                    .first(conn)
                    .optional()
                    .map_err(|e| {
                        TxnError::CustomError(VolumeCreationError::Public(
                            public_error_from_diesel_pool(
                                e.into(),
                                ErrorHandler::Server,
                            ),
                        ))
                    })?;

                // If the volume existed already, return it and do not increase
                // usage counts.
                if let Some(volume) = maybe_volume {
                    return Ok(volume);
                }

                // TODO do we need on_conflict do_nothing here? if the transaction
                // model is read-committed, the SELECT above could return nothing,
                // and the INSERT here could still result in a conflict.
                //
                // See also https://github.com/oxidecomputer/omicron/issues/1168
                let volume: Volume = diesel::insert_into(dsl::volume)
                    .values(volume.clone())
                    .on_conflict(dsl::id)
                    .do_nothing()
                    .returning(Volume::as_returning())
                    .get_result(conn)
                    .map_err(|e| {
                        TxnError::CustomError(VolumeCreationError::Public(
                            public_error_from_diesel_pool(
                                e.into(),
                                ErrorHandler::Conflict(
                                    ResourceType::Volume,
                                    volume.id().to_string().as_str(),
                                ),
                            ),
                        ))
                    })?;

                // Increase the usage count for Crucible resources according to the
                // contents of the volume.

                // Grab all the targets that the volume construction request references.
                let crucible_targets = {
                    let vcr: VolumeConstructionRequest = serde_json::from_str(
                        &volume.data(),
                    )
                    .map_err(|e: serde_json::Error| {
                        TxnError::CustomError(VolumeCreationError::SerdeError(
                            e,
                        ))
                    })?;

                    let mut crucible_targets = CrucibleTargets::default();
                    resources_associated_with_volume(
                        &vcr,
                        &mut crucible_targets,
                    );
                    crucible_targets
                };

                // Increase the number of uses for each referenced region snapshot.
                use db::schema::region_snapshot::dsl as rs_dsl;
                for read_only_target in &crucible_targets.read_only_targets {
                    diesel::update(rs_dsl::region_snapshot)
                        .filter(
                            rs_dsl::snapshot_addr.eq(read_only_target.clone()),
                        )
                        .set(
                            rs_dsl::volume_references
                                .eq(rs_dsl::volume_references + 1),
                        )
                        .execute(conn)
                        .map_err(|e| {
                            TxnError::CustomError(VolumeCreationError::Public(
                                public_error_from_diesel_pool(
                                    e.into(),
                                    ErrorHandler::Server,
                                ),
                            ))
                        })?;
                }

                Ok(volume)
            })
            .await
            .map_err(|e| match e {
                TxnError::CustomError(VolumeCreationError::Public(e)) => e,

                _ => {
                    Error::internal_error(&format!("Transaction error: {}", e))
                }
            })
    }

    pub async fn volume_hard_delete(&self, volume_id: Uuid) -> DeleteResult {
        use db::schema::volume::dsl;

        diesel::delete(dsl::volume)
            .filter(dsl::id.eq(volume_id))
            .execute_async(self.pool())
            .await
            .map_err(|e| {
                public_error_from_diesel_pool(
                    e,
                    ErrorHandler::NotFoundByLookup(
                        ResourceType::Volume,
                        LookupType::ById(volume_id),
                    ),
                )
            })?;

        Ok(())
    }

    pub async fn volume_get(&self, volume_id: Uuid) -> LookupResult<Volume> {
        use db::schema::volume::dsl;

        dsl::volume
            .filter(dsl::id.eq(volume_id))
            .select(Volume::as_select())
            .get_result_async(self.pool())
            .await
            .map_err(|e| public_error_from_diesel_pool(e, ErrorHandler::Server))
    }

    /// Find regions for deleted volumes that do not have associated region
    /// snapshots.
    pub async fn find_deleted_volume_regions(
        &self,
    ) -> ListResultVec<(Dataset, Region, Volume)> {
        use db::schema::dataset::dsl as dataset_dsl;
        use db::schema::region::dsl as region_dsl;
        use db::schema::region_snapshot::dsl;
        use db::schema::volume::dsl as volume_dsl;

        // Find all regions and datasets
        region_dsl::region
            .inner_join(
                volume_dsl::volume.on(region_dsl::volume_id.eq(volume_dsl::id)),
            )
            .inner_join(
                dataset_dsl::dataset
                    .on(region_dsl::dataset_id.eq(dataset_dsl::id)),
            )
            // where there either are no region snapshots, or the region
            // snapshot volume references have gone to zero
            .left_join(
                dsl::region_snapshot.on(dsl::region_id
                    .eq(region_dsl::id)
                    .and(dsl::dataset_id.eq(dataset_dsl::id))),
            )
            .filter(
                dsl::volume_references
                    .eq(0)
                    .or(dsl::volume_references.is_null()),
            )
            // where the volume has already been soft-deleted
            .filter(volume_dsl::time_deleted.is_not_null())
            // and return them (along with the volume so it can be hard deleted)
            .select((
                Dataset::as_select(),
                Region::as_select(),
                Volume::as_select(),
            ))
            .load_async(self.pool())
            .await
            .map_err(|e| public_error_from_diesel_pool(e, ErrorHandler::Server))
    }

    /// Decrease the usage count for Crucible resources according to the
    /// contents of the volume. Call this when deleting a volume (but before the
    /// volume record has been hard deleted).
    ///
    /// Returns a list of Crucible resources to clean up, and soft-deletes the
    /// volume. Note this function must be idempotent, it is called from a saga
    /// node.
    pub async fn decrease_crucible_resource_count_and_soft_delete_volume(
        &self,
        volume_id: Uuid,
    ) -> Result<CrucibleResources, Error> {
        #[derive(Debug, thiserror::Error)]
        enum DecreaseCrucibleResourcesError {
            #[error("Error during decrease Crucible resources: {0}")]
            DieselError(#[from] diesel::result::Error),

            #[error("Serde error during decrease Crucible resources: {0}")]
            SerdeError(#[from] serde_json::Error),
        }
        type TxnError = TransactionError<DecreaseCrucibleResourcesError>;

        // In a transaction:
        //
        // 1. decrease the number of references for each region snapshot that
        //    this Volume references
        // 2. soft-delete the volume
        // 3. record the resources to clean up
        //
        // Step 3 is important because this function is called from a saga node.
        // If saga execution crashes after steps 1 and 2, but before serializing
        // the resources to be cleaned up as part of the saga node context, then
        // that list of resources will be lost.
        //
        // We also have to guard against the case where this function is called
        // multiple times, and that is done by soft-deleting the volume during
        // the transaction, and returning the previously serialized list of
        // resources to clean up if a soft-delete has already occurred.
        //
        // TODO it would be nice to make this transaction_async, but I couldn't
        // get the async optional extension to work.
        self.pool()
            .transaction(move |conn| {
                // Grab the volume in question. If the volume record was already
                // hard-deleted, assume clean-up has occurred and return an empty
                // CrucibleResources. If the volume record was soft-deleted, then
                // return the serialized CrucibleResources.
                let volume = {
                    use db::schema::volume::dsl;

                    let volume = dsl::volume
                        .filter(dsl::id.eq(volume_id))
                        .select(Volume::as_select())
                        .get_result(conn)
                        .optional()?;

                    let volume = if let Some(v) = volume {
                        v
                    } else {
                        // the volume was hard-deleted, return an empty
                        // CrucibleResources
                        return Ok(CrucibleResources::V1(
                            CrucibleResourcesV1::default(),
                        ));
                    };

                    if volume.time_deleted.is_none() {
                        // a volume record exists, and was not deleted - this is the
                        // first time through this transaction for a certain volume
                        // id. Get the volume for use in the transaction.
                        volume
                    } else {
                        // this volume was soft deleted - this is a repeat time
                        // through this transaction.

                        if let Some(resources_to_clean_up) =
                            volume.resources_to_clean_up
                        {
                            // return the serialized CrucibleResources
                            return serde_json::from_str(
                                &resources_to_clean_up,
                            )
                            .map_err(|e| {
                                TxnError::CustomError(
                                    DecreaseCrucibleResourcesError::SerdeError(
                                        e,
                                    ),
                                )
                            });
                        } else {
                            // If no CrucibleResources struct was serialized, that's
                            // definitely a bug of some sort - the soft-delete below
                            // sets time_deleted at the same time as
                            // resources_to_clean_up! But, instead of a panic here,
                            // just return an empty CrucibleResources.
                            return Ok(CrucibleResources::V1(
                                CrucibleResourcesV1::default(),
                            ));
                        }
                    }
                };

                // Grab all the targets that the volume construction request references.
                let crucible_targets = {
                    let vcr: VolumeConstructionRequest =
                        serde_json::from_str(&volume.data()).map_err(|e| {
                            TxnError::CustomError(
                                DecreaseCrucibleResourcesError::SerdeError(e),
                            )
                        })?;

                    let mut crucible_targets = CrucibleTargets::default();
                    resources_associated_with_volume(
                        &vcr,
                        &mut crucible_targets,
                    );
                    crucible_targets
                };

                // Decrease the number of uses for each referenced region snapshot.
                use db::schema::region_snapshot::dsl;

                for read_only_target in &crucible_targets.read_only_targets {
                    diesel::update(dsl::region_snapshot)
                        .filter(dsl::snapshot_addr.eq(read_only_target.clone()))
                        .set(
                            dsl::volume_references
                                .eq(dsl::volume_references - 1),
                        )
                        .execute(conn)?;
                }

                // Return what results can be cleaned up
                let result = CrucibleResources::V1(CrucibleResourcesV1 {
                    // The only use of a read-write region will be at the top level of a
                    // Volume. These are not shared, but if any snapshots are taken this
                    // will prevent deletion of the region. Filter out any regions that
                    // have associated snapshots.
                    datasets_and_regions: {
                        use db::schema::dataset::dsl as dataset_dsl;
                        use db::schema::region::dsl as region_dsl;

                        // Return all regions for this volume_id, where there either are
                        // no region_snapshots, or region_snapshots.volume_references =
                        // 0.
                        region_dsl::region
                            .filter(region_dsl::volume_id.eq(volume_id))
                            .inner_join(
                                dataset_dsl::dataset
                                    .on(region_dsl::dataset_id
                                        .eq(dataset_dsl::id)),
                            )
                            .left_join(
                                dsl::region_snapshot.on(dsl::region_id
                                    .eq(region_dsl::id)
                                    .and(dsl::dataset_id.eq(dataset_dsl::id))),
                            )
                            .filter(
                                dsl::volume_references
                                    .eq(0)
                                    .or(dsl::volume_references.is_null()),
                            )
                            .select((Dataset::as_select(), Region::as_select()))
                            .get_results::<(Dataset, Region)>(conn)?
                    },

                    // A volume (for a disk or snapshot) may reference another nested
                    // volume as a read-only parent, and this may be arbitrarily deep.
                    // After decrementing volume_references above, get all region
                    // snapshot records where the volume_references has gone to 0.
                    // Consumers of this struct will be responsible for deleting the
                    // read-only downstairs running for the snapshot and the snapshot
                    // itself.
                    datasets_and_snapshots: {
                        use db::schema::dataset::dsl as dataset_dsl;

                        dsl::region_snapshot
                            .filter(dsl::volume_references.eq(0))
                            .inner_join(
                                dataset_dsl::dataset
                                    .on(dsl::dataset_id.eq(dataset_dsl::id)),
                            )
                            .select((
                                Dataset::as_select(),
                                RegionSnapshot::as_select(),
                            ))
                            .get_results::<(Dataset, RegionSnapshot)>(conn)?
                    },
                });

                // Soft delete this volume, and serialize the resources that are to
                // be cleaned up.
                use db::schema::volume::dsl as volume_dsl;

                let now = Utc::now();
                diesel::update(volume_dsl::volume)
                    .filter(volume_dsl::id.eq(volume_id))
                    .set((
                        volume_dsl::time_deleted.eq(now),
                        volume_dsl::resources_to_clean_up.eq(
                            serde_json::to_string(&result).map_err(|e| {
                                TxnError::CustomError(
                                    DecreaseCrucibleResourcesError::SerdeError(
                                        e,
                                    ),
                                )
                            })?,
                        ),
                    ))
                    .execute(conn)?;

                Ok(result)
            })
            .await
            .map_err(|e| match e {
                TxnError::CustomError(
                    DecreaseCrucibleResourcesError::DieselError(e),
                ) => public_error_from_diesel_pool(
                    e.into(),
                    ErrorHandler::Server,
                ),

                _ => {
                    Error::internal_error(&format!("Transaction error: {}", e))
                }
            })
    }
}

#[derive(Default)]
struct CrucibleTargets {
    read_only_targets: Vec<String>,
}

// Serialize this enum into the `resources_to_clean_up` column to handle
// different versions over time.
#[derive(Debug, Serialize, Deserialize)]
pub enum CrucibleResources {
    V1(CrucibleResourcesV1),
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct CrucibleResourcesV1 {
    pub datasets_and_regions: Vec<(Dataset, Region)>,
    pub datasets_and_snapshots: Vec<(Dataset, RegionSnapshot)>,
}

/// Return the targets from a VolumeConstructionRequest.
///
/// The targets of a volume construction request map to resources.
fn resources_associated_with_volume(
    vcr: &VolumeConstructionRequest,
    crucible_targets: &mut CrucibleTargets,
) {
    match vcr {
        VolumeConstructionRequest::Volume {
            id: _,
            block_size: _,
            sub_volumes,
            read_only_parent,
        } => {
            for sub_volume in sub_volumes {
                resources_associated_with_volume(sub_volume, crucible_targets);
            }

            if let Some(read_only_parent) = read_only_parent {
                resources_associated_with_volume(
                    read_only_parent,
                    crucible_targets,
                );
            }
        }

        VolumeConstructionRequest::Url { id: _, block_size: _, url: _ } => {
            // no action required
        }

        VolumeConstructionRequest::Region { block_size: _, opts, gen: _ } => {
            for target in &opts.target {
                if opts.read_only {
                    crucible_targets.read_only_targets.push(target.clone());
                }
            }
        }

        VolumeConstructionRequest::File { id: _, block_size: _, path: _ } => {
            // no action required
        }
    }
}
