// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! [`DataStore`] methods related to SCIM

use anyhow::bail;
use anyhow::anyhow;
use async_bb8_diesel::AsyncRunQueryDsl;
use crate::authz;
use crate::context::OpContext;
use crate::db::model::SiloScimClientBearerToken;
use crate::db::model::SiloUser;
use crate::db::model::SiloGroup;
use crate::db::model::SiloGroupMembership;
use omicron_common::api::external::Error; // XXX typed ProviderStoreError?
use nexus_db_errors::OptionalError;
use diesel::prelude::*;
use nexus_db_errors::ErrorHandler;
use nexus_db_errors::public_error_from_diesel;
use omicron_common::api::external::LookupResult;
use omicron_common::api::external::ListResultVec;
use omicron_common::api::external::CreateResult;
use omicron_common::api::external::DeleteResult;
use super::DataStore;
use rand::{RngCore, SeedableRng, rngs::StdRng};
use uuid::Uuid;
use chrono::Utc;

fn generate_scim_client_bearer_token() -> String {
    let mut rng = StdRng::from_entropy();
    let mut random_bytes: [u8; 20] = [0; 20];
    rng.fill_bytes(&mut random_bytes);
    hex::encode(random_bytes)
}

impl DataStore {
    // SCIM tokens

    pub async fn scim_idp_get_tokens(
        &self,
        opctx: &OpContext,
        authz_silo: &authz::Silo,
    ) -> ListResultVec<SiloScimClientBearerToken> {
        opctx.authorize(authz::Action::ListChildren, authz_silo).await?;

        let conn = self.pool_connection_authorized(opctx).await?;

        use nexus_db_schema::schema::silo_scim_client_bearer_token::dsl;
        let tokens = dsl::silo_scim_client_bearer_token
            .filter(dsl::silo_id.eq(authz_silo.id()))
            .select(SiloScimClientBearerToken::as_select())
            .load_async::<SiloScimClientBearerToken>(&*conn)
            .await
            .map_err(|e| public_error_from_diesel(e, ErrorHandler::Server))?;

        Ok(tokens)
    }

    pub async fn scim_idp_create_token(
        &self,
        opctx: &OpContext,
        authz_silo: &authz::Silo,
    ) -> CreateResult<SiloScimClientBearerToken> {
        opctx.authorize(authz::Action::CreateChild, authz_silo).await?;

        let conn = self.pool_connection_authorized(opctx).await?;

        let new_token = SiloScimClientBearerToken {
            id: Uuid::new_v4(),
            time_created: Utc::now(),
            time_deleted: None,
            silo_id: authz_silo.id(),
            bearer_token: generate_scim_client_bearer_token(),
        };

        use nexus_db_schema::schema::silo_scim_client_bearer_token::dsl;
        diesel::insert_into(dsl::silo_scim_client_bearer_token)
            .values(new_token.clone())
            .execute_async(&*conn)
            .await
            .map_err(|e| public_error_from_diesel(e, ErrorHandler::Server))?;

        Ok(new_token)
    }

    pub async fn scim_idp_get_token_by_id(
        &self,
        opctx: &OpContext,
        authz_silo: &authz::Silo,
        id: Uuid,
    ) -> LookupResult<Option<SiloScimClientBearerToken>> {
        opctx.authorize(authz::Action::ListChildren, authz_silo).await?;

        let conn = self.pool_connection_authorized(opctx).await?;

        use nexus_db_schema::schema::silo_scim_client_bearer_token::dsl;
        let token = dsl::silo_scim_client_bearer_token
            .filter(dsl::silo_id.eq(authz_silo.id()))
            .filter(dsl::id.eq(id))
            .select(SiloScimClientBearerToken::as_select())
            .first_async::<SiloScimClientBearerToken>(&*conn)
            .await
            .optional()
            .map_err(|e| public_error_from_diesel(e, ErrorHandler::Server))?;

        Ok(token)
    }

    pub async fn scim_idp_delete_token_by_id(
        &self,
        opctx: &OpContext,
        authz_silo: &authz::Silo,
        id: Uuid,
    ) -> DeleteResult {
        opctx.authorize(authz::Action::Modify, authz_silo).await?;

        let conn = self.pool_connection_authorized(opctx).await?;

        use nexus_db_schema::schema::silo_scim_client_bearer_token::dsl;
        diesel::delete(dsl::silo_scim_client_bearer_token)
            .filter(dsl::silo_id.eq(authz_silo.id()))
            .filter(dsl::id.eq(id))
            .execute_async(&*conn)
            .await
            .map_err(|e| public_error_from_diesel(e, ErrorHandler::Server))?;

        Ok(())
    }

    pub async fn scim_idp_delete_tokens_for_silo(
        &self,
        opctx: &OpContext,
        authz_silo: &authz::Silo,
    ) -> DeleteResult {
        opctx.authorize(authz::Action::Modify, authz_silo).await?;

        let conn = self.pool_connection_authorized(opctx).await?;

        use nexus_db_schema::schema::silo_scim_client_bearer_token::dsl;
        diesel::delete(dsl::silo_scim_client_bearer_token)
            .filter(dsl::silo_id.eq(authz_silo.id()))
            .execute_async(&*conn)
            .await
            .map_err(|e| public_error_from_diesel(e, ErrorHandler::Server))?;

        Ok(())
    }

    // SCIM implementation

    // XXX explain why SCIM has its own auth (not actor!)
    pub async fn scim_idp_lookup_token_by_bearer(
        &self,
        bearer_token: String,
    ) -> LookupResult<Option<SiloScimClientBearerToken>> {
        let conn = self.pool_connection_unauthorized().await?;

        use nexus_db_schema::schema::silo_scim_client_bearer_token::dsl;
        let maybe_token = dsl::silo_scim_client_bearer_token
            .filter(dsl::bearer_token.eq(bearer_token))
            .first_async(&*conn)
            .await
            .optional()
            .map_err(|e| public_error_from_diesel(e, ErrorHandler::Server))?;

        Ok(maybe_token)
    }

    pub async fn scim_idp_provider_store_for_silo(
        self: &Arc<DataStore>,
        silo_id: Uuid,
    ) -> CrdbScimProviderStore {
        CrdbScimProviderStore { silo_id, datastore: self.clone() }
    }
}

use super::Pool;
use std::sync::Arc;

pub struct CrdbScimProviderStore {
    silo_id: Uuid,
    datastore: Arc<DataStore>,
}

use scim2_rs::ProviderStore;
use scim2_rs::StoredUser;
use scim2_rs::StoredGroup;
use scim2_rs::QueryParams;
use scim2_rs::ProviderStoreError;
use scim2_rs::CreateUserRequest;
use scim2_rs::CreateGroupRequest;
use scim2_rs::StoredGroupMember;
use scim2_rs::UserGroup;
use scim2_rs::UserGroupType;

fn convert_silo_to_stored_user(silo_user: SiloUser) -> StoredUser {
    StoredUser {
        id: silo_user.identity.id.to_string(),
        name: silo_user.identity.id.to_string(), // XXX
        active: true, // XXX
        external_id: Some(silo_user.external_id), // XXX
        created: silo_user.identity.time_created,
        last_modified: silo_user.identity.time_modified,
        version: "W/unimplemented".to_string(),
    }
}

fn convert_silo_to_stored_group_membership(
    silo_group_membership: SiloGroupMembership,
) -> StoredGroupMember {
    StoredGroupMember {
        resource_type: scim2_rs::ResourceType::User,
        value: silo_group_membership.silo_user_id.to_string(),
    }
}

fn convert_silo_to_stored_group(
    silo_group: SiloGroup,
    silo_group_memberships: Vec<SiloGroupMembership>,
) -> StoredGroup {
    StoredGroup {
        id: silo_group.identity.id.to_string(),
        display_name: silo_group.identity.id.to_string(), // XXX
        external_id: Some(silo_group.external_id), // XXX
        created: silo_group.identity.time_created,
        last_modified: silo_group.identity.time_modified,
        version: "W/unimplemented".to_string(),
        members: silo_group_memberships
            .into_iter()
            .map(convert_silo_to_stored_group_membership)
            .collect(),
    }
}


#[async_trait::async_trait]
impl ProviderStore for CrdbScimProviderStore {
    async fn get_user_by_id(
        &self,
        user_id: String,
    ) -> Result<Option<StoredUser>, ProviderStoreError> {
        let user_id: Uuid = match user_id.parse() {
            Ok(v) => v,
            Err(_) => {
                return Err(ProviderStoreError::StoreError(
                    anyhow!("user id must be uuid")
                ));
            }
        };

        let conn = self
            .datastore
            .pool_connection_unauthorized()
            .await
            .map_err(|err| ProviderStoreError::StoreError(
            // XXX Error::unavail with ProviderStoreError type
                anyhow!("Failed to access DB connection: {err}")
            ))?;

        use nexus_db_schema::schema::silo_user::dsl;
        let maybe_user = dsl::silo_user
            .filter(dsl::silo_id.eq(self.silo_id))
            .filter(dsl::id.eq(user_id))
            .filter(dsl::time_deleted.is_null())
            .select(SiloUser::as_returning())
            .first_async(&*conn)
            .await
            .optional()
            .map_err(|e| ProviderStoreError::StoreError(e.into()))?;

        Ok(maybe_user.map(convert_silo_to_stored_user))
    }

    async fn get_user_by_username(
        &self,
        _user_name: String,
    ) -> Result<Option<StoredUser>, ProviderStoreError> {
        // XXX not used by provider yet
        unimplemented!();
        /*
        let conn = self
            .datastore
            .pool_connection_unauthorized()
            .await
            .map_err(|err| ProviderStoreError::StoreError(
            // XXX Error::unavail with ProviderStoreError type
                anyhow!("Failed to access DB connection: {err}")
            ))?;

        use nexus_db_schema::schema::silo_user::dsl;
        let maybe_user = dsl::silo_user
            .filter(dsl::silo_id.eq(self.silo_id))
            // .filter(dsl::name.eq(user_name)) // XXX no user name!
            .filter(dsl::time_deleted.is_null())
            .select(SiloUser::as_returning())
            .load_async(&*conn)
            .await
            .optional()
            .map_err(|e| ProviderStoreError::StoreError(e.into()))?;

        Ok(maybe_user)
        */
    }

    async fn create_user(
        &self,
        user_request: CreateUserRequest,
    ) -> Result<StoredUser, ProviderStoreError> {
        let new_user = SiloUser::new(
            self.silo_id,
            Uuid::new_v4(),
            user_request.external_id.unwrap_or(user_request.name), // XXX WRONG
        );

        let conn = self
            .datastore
            .pool_connection_unauthorized()
            .await
            .map_err(|err| ProviderStoreError::StoreError(
            // XXX Error::unavail with ProviderStoreError type
                anyhow!("Failed to access DB connection: {err}")
            ))?;

        use nexus_db_schema::schema::silo_user::dsl;
        diesel::insert_into(dsl::silo_user)
            .values(new_user.clone())
            .execute_async(&*conn)
            .await
            .map_err(|e| ProviderStoreError::StoreError(e.into()))?;

        Ok(convert_silo_to_stored_user(new_user))
    }

    async fn list_users(
        &self,
        query_params: QueryParams,
    ) -> Result<Vec<StoredUser>, ProviderStoreError> {
        let maybe_filter = query_params.filter();

        let silo_users = {
            let conn = self
                .datastore
                .pool_connection_unauthorized()
                .await
                .map_err(|err| ProviderStoreError::StoreError(
                // XXX Error::unavail with ProviderStoreError type
                    anyhow!("Failed to access DB connection: {err}")
                ))?;

            use nexus_db_schema::schema::silo_user::dsl;
            let query = dsl::silo_user
                .filter(dsl::silo_id.eq(self.silo_id))
                .filter(dsl::time_deleted.is_null());

            if let Some(filter) = maybe_filter {
                match filter {
                    scim2_rs::FilterOp::UserNameEq(_username) => {
                        // XXX case sensitive?
                        // define_sql_function!(fn lower(a: diesel::sql_types::VarChar) -> diesel::sql_types::VarChar);

                        /* // XXX no username
                        query = query
                            .filter(dsl::user_name.eq(username.clone()));
                        */
                    }

                    _ => {
                        return Err(scim2_rs::Error::invalid_filter(
                            "invalid or unsupported filter".to_string(),
                        ).into());
                    }
                }
            };

            query
                .select(SiloUser::as_returning())
                .load_async(&*conn)
                .await
                .map_err(|e| ProviderStoreError::StoreError(e.into()))?
        };

        let stored_users = silo_users
            .into_iter()
            .map(convert_silo_to_stored_user)
            .collect();

        Ok(stored_users)
    }

    async fn replace_user(
        &self,
        user_id: String,
        user_request: CreateUserRequest,
    ) -> Result<StoredUser, ProviderStoreError> {
        let user_id: Uuid = match user_id.parse() {
            Ok(v) => v,
            Err(_) => {
                return Err(ProviderStoreError::StoreError(
                    anyhow!("user id must be uuid")
                ));
            }
        };

        // XXX provider store responsibility split is weird here
        unimplemented!();
    }

    async fn delete_user_by_id(
        &self,
        user_id: String,
    ) -> Result<Option<StoredUser>, ProviderStoreError> {
        let user_id: Uuid = match user_id.parse() {
            Ok(v) => v,
            Err(_) => {
                return Err(ProviderStoreError::StoreError(
                    anyhow!("user id must be uuid")
                ));
            }
        };

        let conn = self
            .datastore
            .pool_connection_unauthorized()
            .await
            .map_err(|err| ProviderStoreError::StoreError(
            // XXX Error::unavail with ProviderStoreError type
                anyhow!("Failed to access DB connection: {err}")
            ))?;

        let err = OptionalError::new();

        let maybe_deleted_user: Option<SiloUser> =
            self.datastore.transaction_retry_wrapper("scim_delete_user_by_id")
                .transaction(&conn, |conn| {
                    let silo_id = self.silo_id.clone();
                    let user_id = user_id.clone();
                    let err = err.clone();

                    async move {
                        use nexus_db_schema::schema::silo_user::dsl;

                        let maybe_user = dsl::silo_user
                            .filter(dsl::silo_id.eq(silo_id))
                            .filter(dsl::id.eq(user_id))
                            .filter(dsl::time_deleted.is_null())
                            .select(SiloUser::as_select())
                            .first_async(&conn)
                            .await
                            .optional()?;

                        let Some(user) = maybe_user else {
                            return Ok(None);
                        };

                        let updated = diesel::update(dsl::silo_user)
                            .filter(dsl::silo_id.eq(silo_id))
                            .filter(dsl::id.eq(user_id))
                            .filter(dsl::time_deleted.is_null())
                            .set(dsl::time_deleted.eq(Utc::now()))
                            .execute_async(&conn)
                            .await?;

                        if updated != 1 {
                            return Err(err.bail(anyhow!(
                                "expected 1 row to be updated, not {updated}"
                            )));
                        }

                        Ok(Some(user))
                    }
                })
                .await
                .map_err(|e| {
                    if let Some(e) = err.take() {
                        ProviderStoreError::StoreError(e.into())
                    } else {
                        ProviderStoreError::StoreError(e.into())
                    }
                })?;

        Ok(maybe_deleted_user.map(convert_silo_to_stored_user))
    }

    async fn get_group_by_id(
        &self,
        group_id: String,
    ) -> Result<Option<StoredGroup>, ProviderStoreError> {
        let group_id: Uuid = match group_id.parse() {
            Ok(v) => v,
            Err(_) => {
                return Err(ProviderStoreError::StoreError(
                    anyhow!("group id must be uuid")
                ));
            }
        };

        let conn = self
            .datastore
            .pool_connection_unauthorized()
            .await
            .map_err(|err| ProviderStoreError::StoreError(
            // XXX Error::unavail with ProviderStoreError type
                anyhow!("Failed to access DB connection: {err}")
            ))?;

        use nexus_db_schema::schema::silo_group::dsl;
        let maybe_group = dsl::silo_group
            .filter(dsl::silo_id.eq(self.silo_id))
            .filter(dsl::id.eq(group_id))
            .filter(dsl::time_deleted.is_null())
            .select(SiloGroup::as_returning())
            .first_async(&*conn)
            .await
            .optional()
            .map_err(|e| ProviderStoreError::StoreError(e.into()))?;

        // XXX members

        Ok(maybe_group
            .map(|group| convert_silo_to_stored_group(group, vec![]))
        )
    }

    async fn get_group_by_displayname(
        &self,
        display_name: String,
    ) -> Result<Option<StoredGroup>, ProviderStoreError> {
        unimplemented!();
    }

    async fn create_group_with_members(
        &self,
        group_request: CreateGroupRequest,
        members: Vec<StoredGroupMember>,
    ) -> Result<StoredGroup, ProviderStoreError> {
        let group_id = Uuid::new_v4();

        let new_group = SiloGroup::new(
            group_id,
            self.silo_id,
            group_request.external_id.unwrap_or(group_request.display_name), // XXX WRONG
        );

        let members = members
            .into_iter()
            .map(|member| {
                if member.resource_type != scim2_rs::ResourceType::User {
                    bail!("nested groups not supported");
                } else {
                    let user_id: Uuid = match member.value.parse() {
                        Ok(v) => v,
                        Err(_) => {
                            bail!("user id needs to be uuid");
                        }
                    };

                    Ok(SiloGroupMembership {
                        silo_group_id: group_id,
                        silo_user_id: user_id,
                    })
                }
            })
            .collect::<Result<Vec<SiloGroupMembership>, _>>()?;

        let conn = self
            .datastore
            .pool_connection_unauthorized()
            .await
            .map_err(|err| ProviderStoreError::StoreError(
            // XXX Error::unavail with ProviderStoreError type
                anyhow!("Failed to access DB connection: {err}")
            ))?;

        self
            .datastore
            .transaction_retry_wrapper("scim_create_group_with_members")
            .transaction(&conn, |conn| {
                let new_group = new_group.clone();
                let members = members.clone();
                async move {
                    {
                        use nexus_db_schema::schema::silo_group::dsl;
                        diesel::insert_into(dsl::silo_group)
                            .values(new_group)
                            .execute_async(&conn)
                            .await?;
                    }

                    {
                        use nexus_db_schema::schema::silo_group_membership::dsl;
                        diesel::insert_into(dsl::silo_group_membership)
                            .values(members)
                            .execute_async(&conn)
                            .await?;
                    }

                    Ok(())
                }
            })
            .await
            .map_err(|e| ProviderStoreError::StoreError(e.into()))?;

        Ok(convert_silo_to_stored_group(new_group, members))
    }

    async fn list_groups(
        &self,
        query_params: QueryParams,
    ) -> Result<Vec<StoredGroup>, ProviderStoreError> {
        let maybe_filter = query_params.filter();

        let silo_groups = {
            let conn = self
                .datastore
                .pool_connection_unauthorized()
                .await
                .map_err(|err| ProviderStoreError::StoreError(
                // XXX Error::unavail with ProviderStoreError type
                    anyhow!("Failed to access DB connection: {err}")
                ))?;

            use nexus_db_schema::schema::silo_group::dsl;
            let query = dsl::silo_group
                .filter(dsl::silo_id.eq(self.silo_id))
                .filter(dsl::time_deleted.is_null());

            if let Some(filter) = maybe_filter {
                match filter {
                    scim2_rs::FilterOp::DisplayNameEq(_display_name) => {
                        // XXX case sensitive?
                        // define_sql_function!(fn lower(a: diesel::sql_types::VarChar) -> diesel::sql_types::VarChar);

                        /* // XXX no display name
                        query = query
                            .filter(dsl::display_name.eq(display_name.clone()));
                        */
                    }

                    _ => {
                        return Err(scim2_rs::Error::invalid_filter(
                            "invalid or unsupported filter".to_string(),
                        ).into());
                    }
                }
            };

            query
                .select(SiloGroup::as_returning())
                .load_async(&*conn)
                .await
                .map_err(|e| ProviderStoreError::StoreError(e.into()))?
        };

        // XXX members

        let stored_groups = silo_groups
            .into_iter()
            .map(|group| convert_silo_to_stored_group(group, vec![]))
            .collect();

        Ok(stored_groups)
    }

    async fn replace_group_with_members(
        &self,
        group_id: String,
        group_request: CreateGroupRequest,
        members: Vec<StoredGroupMember>,
    ) -> Result<StoredGroup, ProviderStoreError> {
        let group_id: Uuid = match group_id.parse() {
            Ok(v) => v,
            Err(_) => {
                return Err(ProviderStoreError::StoreError(
                    anyhow!("group id must be uuid")
                ));
            }
        };

        // XXX provider store responsibility split is weird here
        unimplemented!();
    }

    // Delete a group, and all group memberships.
    //
    // A Some(StoredGroup) is returned if the Group existed prior to the delete,
    // otherwise None is returned.
    async fn delete_group_by_id(
        &self,
        group_id: String,
    ) -> Result<Option<StoredGroup>, ProviderStoreError> {
        let group_id: Uuid = match group_id.parse() {
            Ok(v) => v,
            Err(_) => {
                return Err(ProviderStoreError::StoreError(
                    anyhow!("group id must be uuid")
                ));
            }
        };

        let conn = self
            .datastore
            .pool_connection_unauthorized()
            .await
            .map_err(|err| ProviderStoreError::StoreError(
            // XXX Error::unavail with ProviderStoreError type
                anyhow!("Failed to access DB connection: {err}")
            ))?;

        let err = OptionalError::new();

        let maybe_existing = self
            .datastore
            .transaction_retry_wrapper("scim_delete_group_by_id")
            .transaction(&conn, |conn| {
                let err = err.clone();

                async move {
                    let group = {
                        use nexus_db_schema::schema::silo_group::dsl;

                        let maybe_group = dsl::silo_group
                            .filter(dsl::silo_id.eq(self.silo_id))
                            .filter(dsl::id.eq(group_id))
                            .filter(dsl::time_deleted.is_null())
                            .select(SiloGroup::as_select())
                            .first_async(&conn)
                            .await
                            .optional()?;

                        let Some(group) = maybe_group else {
                            return Ok(None);
                        };

                        group
                    };

                    let members: Vec<_> = {
                        use nexus_db_schema::schema::silo_group_membership::dsl;

                        dsl::silo_group_membership
                            .filter(dsl::silo_group_id.eq(group_id))
                            .load_async(&conn)
                            .await?
                    };

                    {
                        use nexus_db_schema::schema::silo_group::dsl;
                        let updated = diesel::update(dsl::silo_group)
                            .filter(dsl::silo_id.eq(self.silo_id))
                            .filter(dsl::id.eq(group_id))
                            .filter(dsl::time_deleted.is_null())
                            .set(dsl::time_deleted.eq(Utc::now()))
                            .execute_async(&conn)
                            .await?;

                        if updated != 1 {
                            return Err(err.bail(anyhow!(
                                "expected 1 row to be updated, not {updated}"
                            )));
                        }
                    }

                    {
                        use nexus_db_schema::schema::silo_group_membership::dsl;

                        diesel::delete(dsl::silo_group_membership)
                            .filter(dsl::silo_group_id.eq(group_id))
                            .execute_async(&conn)
                            .await?;
                    }

                    Ok(Some((group, members)))
                }
            })
            .await
            .map_err(|e| {
                if let Some(e) = err.take() {
                    ProviderStoreError::StoreError(e.into())
                } else {
                    ProviderStoreError::StoreError(e.into())
                }
            })?;

        Ok(maybe_existing
            .map(|(group, members)|
                convert_silo_to_stored_group(group, members)
        ))
    }

    async fn get_user_group_membership(
        &self,
        user_id: String,
    ) -> Result<Vec<UserGroup>, ProviderStoreError> {
        let user_id: Uuid = match user_id.parse() {
            Ok(v) => v,
            Err(_) => {
                return Err(ProviderStoreError::StoreError(
                    anyhow!("user id must be uuid")
                ));
            }
        };

        let conn = self
            .datastore
            .pool_connection_unauthorized()
            .await
            .map_err(|err| ProviderStoreError::StoreError(
            // XXX Error::unavail with ProviderStoreError type
                anyhow!("Failed to access DB connection: {err}")
            ))?;

        let members = {
            use nexus_db_schema::schema::silo_group_membership::dsl;

            dsl::silo_group_membership
                .filter(dsl::silo_user_id.eq(user_id))
                .select(SiloGroupMembership::as_select())
                .load_async(&*conn)
                .await
                .map_err(|e| ProviderStoreError::StoreError(e.into()))?
        };

        Ok(members
            .into_iter()
            .map(|silo_group_membership| UserGroup {
                member_type: Some(UserGroupType::Direct),
                value: Some(silo_group_membership.silo_group_id.to_string()),
                display: None, // XXX ?
            })
            .collect()
        )
    }

    async fn get_group_members(
        &self,
        group_id: String,
    ) -> Result<Vec<StoredGroupMember>, ProviderStoreError> {
        let group_id: Uuid = match group_id.parse() {
            Ok(v) => v,
            Err(_) => {
                return Err(ProviderStoreError::StoreError(
                    anyhow!("group id must be uuid")
                ));
            }
        };

        let conn = self
            .datastore
            .pool_connection_unauthorized()
            .await
            .map_err(|err| ProviderStoreError::StoreError(
            // XXX Error::unavail with ProviderStoreError type
                anyhow!("Failed to access DB connection: {err}")
            ))?;

        let members = {
            use nexus_db_schema::schema::silo_group_membership::dsl;

            dsl::silo_group_membership
                .filter(dsl::silo_group_id.eq(group_id))
                .select(SiloGroupMembership::as_select())
                .load_async(&*conn)
                .await
                .map_err(|e| ProviderStoreError::StoreError(e.into()))?
        };

        Ok(members
            .into_iter()
            .map(convert_silo_to_stored_group_membership)
            .collect()
        )
    }
}
