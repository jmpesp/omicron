// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! [`DataStore`] methods related to SCIM

use iddqd::IdOrdMap;
use nexus_auth::authz::ApiResource;
use anyhow::anyhow;
use async_bb8_diesel::AsyncRunQueryDsl;
use chrono::Utc;
use crate::authz;
use crate::context::OpContext;
use crate::db;
use crate::db::model::Silo;
use crate::db::model::SiloGroup;
use crate::db::model::SiloGroupScimAttributes;
use crate::db::model::SiloScimClientBearerToken;
use crate::db::model::SiloGroupMembership;
use crate::db::model::SiloUser;
use crate::db::model::SiloUserScimAttributes;
use diesel::prelude::*;
use nexus_db_errors::ErrorHandler;
use nexus_db_errors::OptionalError;
use nexus_db_errors::public_error_from_diesel;
use nexus_db_lookup::DbConnection;
use omicron_common::api::external::CreateResult;
use omicron_common::api::external::DeleteResult;
use omicron_common::api::external::ListResultVec;
use omicron_common::api::external::LookupResult;
use rand::{RngCore, SeedableRng, rngs::StdRng};
use std::sync::Arc;
use std::str::FromStr;
use super::DataStore;
use uuid::Uuid;
use nexus_types::external_api::shared::SiloRole;
use omicron_common::api::external::LookupType;
use nexus_auth::authz::ApiResourceWithRoles;
use nexus_db_model::DatabaseString;

use scim2_rs::ProviderStoreDeleteResult;
use scim2_rs::CreateGroupRequest;
use scim2_rs::CreateUserRequest;
use scim2_rs::FilterOp;
use scim2_rs::Group;
use scim2_rs::GroupMember;
use scim2_rs::ProviderStore;
use scim2_rs::ProviderStoreError;
use scim2_rs::ResourceType;
use scim2_rs::StoredMeta;
use scim2_rs::StoredParts;
use scim2_rs::User;
use scim2_rs::UserGroup;
use scim2_rs::UserGroupType;

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
            .filter(dsl::time_deleted.is_null())
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
            .filter(dsl::time_deleted.is_null())
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
        diesel::update(dsl::silo_scim_client_bearer_token)
            .filter(dsl::silo_id.eq(authz_silo.id()))
            .filter(dsl::id.eq(id))
            .filter(dsl::time_deleted.is_null())
            .set(dsl::time_deleted.eq(Utc::now()))
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
        diesel::update(dsl::silo_scim_client_bearer_token)
            .filter(dsl::silo_id.eq(authz_silo.id()))
            .filter(dsl::time_deleted.is_null())
            .set(dsl::time_deleted.eq(Utc::now()))
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
            .filter(dsl::time_deleted.is_null())
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

// XXX separate file?

pub struct CrdbScimProviderStore {
    silo_id: Uuid,
    datastore: Arc<DataStore>,
}

impl CrdbScimProviderStore {
    /// Nuke sessions, tokens, etc, for deactivated users
    async fn on_user_active_to_false_in_txn(
        &self,
        conn: &async_bb8_diesel::Connection<DbConnection>,
        user_id: Uuid,
    ) -> Result<(), diesel::result::Error> {
        // Delete console sessions.
        {
            use nexus_db_schema::schema::console_session::dsl;
            diesel::delete(dsl::console_session)
                .filter(dsl::silo_user_id.eq(user_id))
                .execute_async(conn)
                .await?;
        }

        // Delete device authentication tokens.
        {
            use nexus_db_schema::schema::device_access_token::dsl;
            diesel::delete(dsl::device_access_token)
                .filter(dsl::silo_user_id.eq(user_id))
                .execute_async(conn)
                .await?;
        }
        Ok(())
    }

    async fn get_user_groups_for_user_in_txn(
        &self,
        conn: &async_bb8_diesel::Connection<DbConnection>,
        user_id: Uuid,
    ) -> Result<Option<Vec<UserGroup>>, diesel::result::Error> {
        use nexus_db_schema::schema::silo_group_membership::dsl;
        use nexus_db_schema::schema::silo_group::dsl as group_dsl;
        use nexus_db_schema::schema::silo_group_scim_attributes::dsl as
            attributes_dsl;

        struct Columns {
            group_id: Uuid,
            display_name: String,
        }

        let tuples: Vec<Columns> = dsl::silo_group_membership
            .inner_join(group_dsl::silo_group
                .on(dsl::silo_group_id.eq(group_dsl::id))
            )
            .inner_join(attributes_dsl::silo_group_scim_attributes
                .on(attributes_dsl::silo_group_id.eq(group_dsl::id))
            )
            .filter(group_dsl::silo_id.eq(self.silo_id))
            .filter(dsl::silo_user_id.eq(user_id))
            .filter(group_dsl::time_deleted.is_null())
            .select((
                group_dsl::id,
                attributes_dsl::display_name,
            ))
            .load_async(conn)
            .await?
            .into_iter()
            // XXX without the into_iter?
            .map(|(group_id, display_name)| Columns {
                group_id, display_name
            })
            .collect();

        if tuples.is_empty() {
            Ok(None)
        } else {
            let groups = tuples
                .into_iter()
                .map(|column|
                    UserGroup {
                        // Note this provider type does not support nested
                        // groups
                        member_type: Some(UserGroupType::Direct),
                        value: Some(column.group_id.to_string()),
                        display: Some(column.display_name),
                    }
                )
                .collect();

            Ok(Some(groups))
        }
    }

    async fn get_group_members_for_group_in_txn(
        &self,
        conn: &async_bb8_diesel::Connection<DbConnection>,
        group_id: Uuid,
    ) -> Result<Option<IdOrdMap<GroupMember>>, diesel::result::Error> {
        use nexus_db_schema::schema::silo_group_membership::dsl;

        let users: Vec<Uuid> = dsl::silo_group_membership
            .filter(dsl::silo_group_id.eq(group_id))
            .select(dsl::silo_user_id)
            .load_async(conn)
            .await?;

        if users.is_empty() {
            Ok(None)
        } else {
            let members = users
                .into_iter()
                .map(|user_id|
                    GroupMember {
                        resource_type: Some(ResourceType::User.to_string()),
                        value: Some(user_id.to_string()),
                    }
                )
                .collect();

            Ok(Some(members))
        }
    }

    async fn get_user_by_id_in_txn(
        &self,
        conn: &async_bb8_diesel::Connection<DbConnection>,
        _err: OptionalError<ProviderStoreError>,
        user_id: Uuid,
    ) -> Result<Option<StoredParts<User>>, diesel::result::Error> {
        let maybe_user = {
            use nexus_db_schema::schema::silo_user::dsl;
            use nexus_db_schema::schema::silo_user_scim_attributes::dsl as
                attributes_dsl;

            dsl::silo_user
                .inner_join(attributes_dsl::silo_user_scim_attributes
                    .on(attributes_dsl::silo_user_id.eq(dsl::id))
                )
                .filter(dsl::silo_id.eq(self.silo_id))
                .filter(dsl::id.eq(user_id))
                .filter(dsl::time_deleted.is_null())
                .select((
                    SiloUser::as_returning(),
                    SiloUserScimAttributes::as_returning(),
                ))
                .first_async(conn)
                .await
                .optional()?
        };

        let Some((user, attributes)) = maybe_user else {
            return Ok(None);
        };

        let groups: Option<Vec<UserGroup>> = self.get_user_groups_for_user_in_txn(
            conn, user_id,
        ).await?;

        Ok(Some(convert_to_scim_user(user, attributes, groups)))
    }

    async fn create_user_in_txn(
        &self,
        conn: &async_bb8_diesel::Connection<DbConnection>,
        err: OptionalError<ProviderStoreError>,
        user_request: CreateUserRequest,
    ) -> Result<StoredParts<User>, diesel::result::Error> {
        use nexus_db_schema::schema::silo_user::dsl;
        use nexus_db_schema::schema::silo_user_scim_attributes::dsl as
            attributes_dsl;

        // userName is meant to be unique: If the user request is adding a
        // userName that already exists, reject it

        let maybe_other_user = dsl::silo_user
            .inner_join(attributes_dsl::silo_user_scim_attributes
                .on(attributes_dsl::silo_user_id.eq(dsl::id))
            )
            .filter(dsl::silo_id.eq(self.silo_id))
            .filter(attributes_dsl::user_name.eq(user_request.name.clone()))
            .filter(dsl::time_deleted.is_null())
            .select(SiloUser::as_returning())
            .first_async(conn)
            .await
            .optional()?;

        if maybe_other_user.is_some() {
            return Err(err.bail(
                scim2_rs::Error::conflict(format!("username {}", user_request.name)).into()
            ));
        }

        let user_id = Uuid::new_v4();
        let new_user = SiloUser::new(self.silo_id, user_id, user_id.to_string());

        let new_attributes = SiloUserScimAttributes {
            silo_user_id: user_id,

            user_name: user_request.name,
            external_id: user_request.external_id,
            active: user_request.active,
        };

        diesel::insert_into(dsl::silo_user)
            .values(new_user.clone())
            .execute_async(&*conn)
            .await?;

        diesel::insert_into(attributes_dsl::silo_user_scim_attributes)
            .values(new_attributes.clone())
            .execute_async(&*conn)
            .await?;

        Ok(convert_to_scim_user(new_user, new_attributes, None))
    }

    async fn list_users_in_txn(
        &self,
        conn: &async_bb8_diesel::Connection<DbConnection>,
        err: OptionalError<ProviderStoreError>,
        filter: Option<FilterOp>,
    ) -> Result<Vec<StoredParts<User>>, diesel::result::Error> {
        use nexus_db_schema::schema::silo_user::dsl;
        use nexus_db_schema::schema::silo_user_scim_attributes::dsl as
            attributes_dsl;

        let mut query = dsl::silo_user
            .inner_join(attributes_dsl::silo_user_scim_attributes
                .on(attributes_dsl::silo_user_id.eq(dsl::id))
            )
            .filter(dsl::silo_id.eq(self.silo_id))
            .filter(dsl::time_deleted.is_null())
            .into_boxed();

        match filter {
            Some(FilterOp::UserNameEq(username)) => {
                // XXX case sensitive?
                // define_sql_function!(fn lower(a: diesel::sql_types::VarChar) -> diesel::sql_types::VarChar);
                query = query
                    .filter(attributes_dsl::user_name.eq(username.clone()));
            }

            None => {
                // ok
            }

            Some(_) => {
                return Err(err.bail(scim2_rs::Error::invalid_filter(
                    "invalid or unsupported filter".to_string(),
                ).into()));
            }
        }

        let users = query
            .select((
                SiloUser::as_returning(),
                SiloUserScimAttributes::as_returning(),
            ))
            .load_async(conn)
            .await?;

        let mut returned_users = Vec::with_capacity(users.len());

        for (user, attributes) in users {
            let groups = self.get_user_groups_for_user_in_txn(
                conn,
                user.identity.id,
            ).await?;

            returned_users.push(convert_to_scim_user(user, attributes, groups));
        }

        Ok(returned_users)
    }

    async fn replace_user_in_txn(
        &self,
        conn: &async_bb8_diesel::Connection<DbConnection>,
        err: OptionalError<ProviderStoreError>,
        user_id: Uuid,
        user_request: CreateUserRequest,
    ) -> Result<StoredParts<User>, diesel::result::Error> {
        use nexus_db_schema::schema::silo_user::dsl;
        use nexus_db_schema::schema::silo_user_scim_attributes::dsl as
            attributes_dsl;

        let maybe_user = dsl::silo_user
            .filter(dsl::silo_id.eq(self.silo_id))
            .filter(dsl::id.eq(user_id))
            .filter(dsl::time_deleted.is_null())
            .select(SiloUser::as_returning())
            .first_async(conn)
            .await
            .optional()?;

        if maybe_user.is_none() {
            return Err(err.bail(scim2_rs::Error::not_found(
                user_id.to_string()
            ).into()));
        }

        let CreateUserRequest {
            name,
            active,
            external_id,
            groups: _,
        } = user_request;

        // userName is meant to be unique: If the user request is changing the
        // userName to one that already exists, reject it

        let maybe_other_user = dsl::silo_user
            .inner_join(attributes_dsl::silo_user_scim_attributes
                .on(attributes_dsl::silo_user_id.eq(dsl::id))
            )
            .filter(dsl::silo_id.eq(self.silo_id))
            .filter(attributes_dsl::user_name.eq(name.clone()))
            .filter(dsl::id.ne(user_id))
            .filter(dsl::time_deleted.is_null())
            .select(SiloUser::as_returning())
            .first_async(conn)
            .await
            .optional()?;

        if maybe_other_user.is_some() {
            return Err(err.bail(
                scim2_rs::Error::conflict(format!("username {}", name)).into()
            ));
        }

        // Overwrite all fields based on CreateUserRequest, except groups: it's
        // invalid to change group memberships in a Users PUT.

        if let Some(active) = active {
            if !active {
                self.on_user_active_to_false_in_txn(conn, user_id).await?;
            }
        }

        let updated = diesel::update(dsl::silo_user)
            .filter(dsl::silo_id.eq(self.silo_id))
            .filter(dsl::id.eq(user_id))
            .filter(dsl::time_deleted.is_null())
            .set((
                dsl::time_modified.eq(Utc::now()),
            ))
            .execute_async(conn)
            .await?;

        if updated != 1 {
            return Err(err.bail(ProviderStoreError::StoreError(
            anyhow!(
                "expected 1 row to be updated, not {updated}"
            ))));
        }

        let updated = diesel::update(attributes_dsl::silo_user_scim_attributes)
            .filter(attributes_dsl::silo_user_id.eq(user_id))
            .set((
                attributes_dsl::user_name.eq(name),
                attributes_dsl::active.eq(active),
                attributes_dsl::external_id.eq(external_id),
            ))
            .execute_async(conn)
            .await?;

        if updated != 1 {
            return Err(err.bail(ProviderStoreError::StoreError(
            anyhow!(
                "expected 1 row to be updated, not {updated}"
            ))));
        }

        let (user, attributes) = dsl::silo_user
            .inner_join(attributes_dsl::silo_user_scim_attributes
                .on(attributes_dsl::silo_user_id.eq(dsl::id))
            )
            .filter(dsl::silo_id.eq(self.silo_id))
            .filter(dsl::id.eq(user_id))
            .filter(dsl::time_deleted.is_null())
            .select((
                SiloUser::as_select(),
                SiloUserScimAttributes::as_returning(),
            ))
            .first_async(conn)
            .await?;

        // Groups don't change, so query for what was previously there

        let groups = self.get_user_groups_for_user_in_txn(
            conn,
            user.identity.id,
        ).await?;

        Ok(convert_to_scim_user(user, attributes, groups))
    }

    async fn delete_user_by_id_in_txn(
        &self,
        conn: &async_bb8_diesel::Connection<DbConnection>,
        err: OptionalError<ProviderStoreError>,
        user_id: Uuid,
    ) -> Result<bool, diesel::result::Error> {
        use nexus_db_schema::schema::silo_user::dsl;

        let maybe_user = dsl::silo_user
            .filter(dsl::silo_id.eq(self.silo_id))
            .filter(dsl::id.eq(user_id))
            .filter(dsl::time_deleted.is_null())
            .select(SiloUser::as_select())
            .first_async(conn)
            .await
            .optional()?;

        if maybe_user.is_none() {
            return Ok(false);
        }

        let updated = diesel::update(dsl::silo_user)
            .filter(dsl::silo_id.eq(self.silo_id))
            .filter(dsl::id.eq(user_id))
            .filter(dsl::time_deleted.is_null())
            .set(dsl::time_deleted.eq(Utc::now()))
            .execute_async(conn)
            .await?;

        if updated != 1 {
            return Err(err.bail(ProviderStoreError::StoreError(
            anyhow!(
                "expected 1 row to be updated, not {updated}"
            ))));
        }

        {
            use nexus_db_schema::schema::silo_user_scim_attributes::dsl;

            diesel::delete(dsl::silo_user_scim_attributes)
                .filter(dsl::silo_user_id.eq(user_id))
                .execute_async(conn)
                .await?;
        }

        {
            use nexus_db_schema::schema::silo_group_membership::dsl;

            diesel::delete(dsl::silo_group_membership)
                .filter(dsl::silo_user_id.eq(user_id))
                .execute_async(conn)
                .await?;
        }

        Ok(true)
    }

    async fn get_group_by_id_in_txn(
        &self,
        conn: &async_bb8_diesel::Connection<DbConnection>,
        _err: OptionalError<ProviderStoreError>,
        group_id: Uuid,
    ) -> Result<Option<StoredParts<Group>>, diesel::result::Error> {
        use nexus_db_schema::schema::silo_group::dsl;
        use nexus_db_schema::schema::silo_group_scim_attributes::dsl as
            attributes_dsl;

        let maybe_group = dsl::silo_group
            .inner_join(attributes_dsl::silo_group_scim_attributes
                .on(attributes_dsl::silo_group_id.eq(dsl::id))
            )
            .filter(dsl::silo_id.eq(self.silo_id))
            .filter(dsl::id.eq(group_id))
            .filter(dsl::time_deleted.is_null())
            .select((
                SiloGroup::as_returning(),
                SiloGroupScimAttributes::as_returning(),
            ))
            .first_async(conn)
            .await
            .optional()?;

        let Some((group, attributes)) = maybe_group else {
            return Ok(None);
        };

        let members: Option<IdOrdMap<GroupMember>> =
            self.get_group_members_for_group_in_txn(conn, group_id).await?;

        Ok(Some(convert_to_scim_group(group, attributes, members)))
    }

    /// Returns User id, and the GroupMember object, if this member is valid
    async fn validate_group_member_in_txn(
        &self,
        conn: &async_bb8_diesel::Connection<DbConnection>,
        err: OptionalError<ProviderStoreError>,
        member: &GroupMember,
    ) -> Result<(Uuid, GroupMember), diesel::result::Error> {
        let GroupMember { resource_type, value } = member;

        let Some(value) = value else {
            // The minimum that this code needs is the value field so complain
            // about that.
            return Err(err.bail(scim2_rs::Error::invalid_syntax(String::from(
                "group member missing value field",
            )).into()));
        };

        let id: Uuid = match value.parse() {
            Ok(v) => v,

            Err(_) => {
                return Err(err.bail(ProviderStoreError::StoreError(
                    anyhow!("user id must be uuid")
                )));
            }
        };

        // Find the ID that this request is talking about, or 404
        let resource_type = if let Some(resource_type) = resource_type {
            let resource_type = match ResourceType::from_str(resource_type) {
                Ok(v) => v,
                Err(e) => Err(err.bail(
                    scim2_rs::Error::invalid_syntax(e.to_string()).into()
                ))?,
            };

            match resource_type {
                ResourceType::User => {
                    use nexus_db_schema::schema::silo_user::dsl;

                    let maybe_user: Option<Uuid> = dsl::silo_user
                        .filter(dsl::silo_id.eq(self.silo_id))
                        .filter(dsl::id.eq(id))
                        .filter(dsl::time_deleted.is_null())
                        .select(dsl::id)
                        .first_async(conn)
                        .await
                        .optional()?;

                    if maybe_user.is_none() {
                        return Err(err.bail(scim2_rs::Error::not_found(
                            value.to_string()
                        ).into()));
                    }
                }

                ResourceType::Group => {
                    // don't support nested groups for now.
                    return Err(err.bail(scim2_rs::Error::internal_error(
                        "nested groups not supported".to_string(),
                    ).into()));
                }
            }

            resource_type
        } else {
            let maybe_user: Option<Uuid> = {
                use nexus_db_schema::schema::silo_user::dsl;

                dsl::silo_user
                    .filter(dsl::silo_id.eq(self.silo_id))
                    .filter(dsl::id.eq(id))
                    .filter(dsl::time_deleted.is_null())
                    .select(dsl::id)
                    .first_async(conn)
                    .await
                    .optional()?
            };

            let maybe_group: Option<Uuid> = {
                use nexus_db_schema::schema::silo_group::dsl;

                dsl::silo_group
                    .filter(dsl::silo_id.eq(self.silo_id))
                    .filter(dsl::id.eq(id))
                    .filter(dsl::time_deleted.is_null())
                    .select(dsl::id)
                    .first_async(conn)
                    .await
                    .optional()?
            };

            match (maybe_user, maybe_group) {
                (None, None) => {
                    // 404
                    return Err(err.bail(scim2_rs::Error::not_found(
                        value.clone()
                    ).into()));
                }

                (Some(_), None) => ResourceType::User,

                (None, Some(_)) => {
                    return Err(err.bail(scim2_rs::Error::internal_error(
                        "nested groups not supported".to_string(),
                    ).into()));
                }

                (Some(_), Some(_)) => {
                    return Err(err.bail(scim2_rs::Error::internal_error(format!(
                        "{value} returned a user and group!"
                    )).into()));
                }
            }
        };

        Ok((id, GroupMember {
            resource_type: Some(resource_type.to_string()),
            value: Some(value.to_string()),
        }))
    }

    async fn create_group_in_txn(
        &self,
        conn: &async_bb8_diesel::Connection<DbConnection>,
        err: OptionalError<ProviderStoreError>,
        group_request: CreateGroupRequest,
    ) -> Result<StoredParts<Group>, diesel::result::Error> {
        let CreateGroupRequest { display_name, external_id, members } =
            group_request;

        // displayName is meant to be unique: If the group request is changing
        // the displayName to one that already exists, reject it.

        let maybe_group = dsl::silo_group
            .inner_join(attributes_dsl::silo_group_scim_attributes
                .on(attributes_dsl::silo_group_id.eq(dsl::id))
            )
            .filter(dsl::silo_id.eq(self.silo_id))
            .filter(attributes_dsl::display_name.eq(display_name.clone()))
            .filter(dsl::time_deleted.is_null())
            .select(SiloGroup::as_returning())
            .first_async(conn)
            .await
            .optional()?;

        if maybe_group.is_some() {
            return Err(err.bail(scim2_rs::Error::conflict(
                format!("displayName {}", display_name)
            ).into()));
        }

        let group_id = Uuid::new_v4();

        let new_group = SiloGroup::new(group_id, self.silo_id, group_id.to_string());

        let new_attributes = SiloGroupScimAttributes {
            silo_group_id: group_id,
            display_name: display_name.clone(),
            external_id,
        };

        use nexus_db_schema::schema::silo_group::dsl;
        use nexus_db_schema::schema::silo_group_scim_attributes::dsl as
            attributes_dsl;

        diesel::insert_into(dsl::silo_group)
            .values(new_group.clone())
            .execute_async(conn)
            .await?;

        diesel::insert_into(attributes_dsl::silo_group_scim_attributes)
            .values(new_attributes.clone())
            .execute_async(conn)
            .await?;

        let members = if let Some(members) = &members {
            let mut returned_members = IdOrdMap::with_capacity(members.len());
            let mut memberships = Vec::with_capacity(members.len());

            // Validate the members arg, and insert silo group membership
            // records.
            for member in members {
                let (user_id, returned_member) = self
                    .validate_group_member_in_txn(conn, err.clone(), member)
                    .await?;

                match returned_members.insert_unique(returned_member) {
                    Ok(_) => {},

                    Err(e) => {
                        return Err(err.bail(ProviderStoreError::Scim(
                            scim2_rs::Error::conflict(format!("{:?}", e.new_item()))
                        )));
                    }
                }

                memberships.push(SiloGroupMembership {
                    silo_group_id: group_id,
                    silo_user_id: user_id,
                });
            }

            use nexus_db_schema::schema::silo_group_membership::dsl;
            diesel::insert_into(dsl::silo_group_membership)
                .values(memberships)
                .execute_async(conn)
                .await?;

            Some(returned_members)
        } else {
            None
        };

        // If this group's name matches the silo's admin group name, then create
        // the appropriate policy granting members of that group the silo admin
        // role.

        {
            use nexus_db_schema::schema::silo::dsl;
            let silo = dsl::silo
                .filter(dsl::id.eq(self.silo_id))
                .select(Silo::as_select())
                .first_async(conn)
                .await?;

            // XXX this means that the admin group can't be deleted!

            // XXX this means that the SCIM provisioning client won't know about
            // this group!

            if let Some(admin_group_name) = silo.admin_group_name {
                if admin_group_name == display_name {
                    // XXX code copied from silo create

                    let authz_silo = authz::Silo::new(
                        authz::FLEET,
                        self.silo_id,
                        LookupType::ById(self.silo_id),
                    );

                    use nexus_db_schema::schema::role_assignment::dsl;

                    let new_assignment = db::model::RoleAssignment::new(
                        db::model::IdentityType::SiloGroup,
                        group_id,
                        authz_silo.resource_type(),
                        authz_silo.resource_id(),
                        &SiloRole::Admin.to_database_string(),
                    );

                    diesel::insert_into(dsl::role_assignment)
                        .values(new_assignment)
                        .execute_async(conn)
                        .await?;
                }
            }
        }

        Ok(convert_to_scim_group(new_group, new_attributes, members))
    }

    async fn list_groups_in_txn(
        &self,
        conn: &async_bb8_diesel::Connection<DbConnection>,
        err: OptionalError<ProviderStoreError>,
        filter: Option<FilterOp>,
    ) -> Result<Vec<StoredParts<Group>>, diesel::result::Error> {
        use nexus_db_schema::schema::silo_group::dsl;
        use nexus_db_schema::schema::silo_group_scim_attributes::dsl as
            attributes_dsl;

        let mut query = dsl::silo_group
            .inner_join(attributes_dsl::silo_group_scim_attributes
                .on(attributes_dsl::silo_group_id.eq(dsl::id))
            )
            .filter(dsl::silo_id.eq(self.silo_id))
            .filter(dsl::time_deleted.is_null())
            .into_boxed();

        match filter {
            Some(FilterOp::DisplayNameEq(display_name)) => {
                // XXX case sensitive?
                // define_sql_function!(fn lower(a: diesel::sql_types::VarChar) -> diesel::sql_types::VarChar);

                query = query
                    .filter(attributes_dsl::display_name.eq(display_name.clone()));
            }

            None => {
                // ok
            }

            Some(_) => {
                return Err(err.bail(scim2_rs::Error::invalid_filter(
                    "invalid or unsupported filter".to_string(),
                ).into()));
            }
        }

        let groups = query
            .select((
                SiloGroup::as_returning(),
                SiloGroupScimAttributes::as_returning(),
            ))
            .load_async(&*conn)
            .await?;

        let mut returned_groups = Vec::with_capacity(groups.len());

        for (group, attributes) in groups {
            let members = self.get_group_members_for_group_in_txn(
                conn,
                group.identity.id,
            ).await?;

            returned_groups.push(convert_to_scim_group(group, attributes, members));
        }

        Ok(returned_groups)
    }

    async fn replace_group_in_txn(
        &self,
        conn: &async_bb8_diesel::Connection<DbConnection>,
        err: OptionalError<ProviderStoreError>,
        group_id: Uuid,
        group_request: CreateGroupRequest,
    ) -> Result<StoredParts<Group>, diesel::result::Error> {
        use nexus_db_schema::schema::silo_group::dsl;
        use nexus_db_schema::schema::silo_group_scim_attributes::dsl as
            attributes_dsl;

        let maybe_group = dsl::silo_group
            .filter(dsl::silo_id.eq(self.silo_id))
            .filter(dsl::id.eq(group_id))
            .filter(dsl::time_deleted.is_null())
            .select(SiloGroup::as_returning())
            .first_async(conn)
            .await
            .optional()?;

        if maybe_group.is_none() {
            return Err(err.bail(scim2_rs::Error::not_found(group_id.to_string()).into()));
        };

        let CreateGroupRequest {
            display_name,
            external_id,
            members,
        } = group_request;

        // displayName is meant to be unique: If the group request is changing
        // the displayName to one that already exists, reject it.

        let maybe_group = dsl::silo_group
            .inner_join(attributes_dsl::silo_group_scim_attributes
                .on(attributes_dsl::silo_group_id.eq(dsl::id))
            )
            .filter(dsl::silo_id.eq(self.silo_id))
            .filter(attributes_dsl::display_name.eq(display_name.clone()))
            .filter(dsl::id.ne(group_id))
            .filter(dsl::time_deleted.is_null())
            .select(SiloGroup::as_returning())
            .first_async(conn)
            .await
            .optional()?;

        if maybe_group.is_some() {
            return Err(err.bail(scim2_rs::Error::conflict(
                format!("displayName {}", display_name)
            ).into()));
        }

        // Overwrite all fields based on CreateGroupRequest.

        let updated = diesel::update(dsl::silo_group)
            .filter(dsl::silo_id.eq(self.silo_id))
            .filter(dsl::id.eq(group_id))
            .filter(dsl::time_deleted.is_null())
            .set((
                dsl::time_modified.eq(Utc::now()),
            ))
            .execute_async(conn)
            .await?;

        if updated != 1 {
            return Err(err.bail(ProviderStoreError::StoreError(
            anyhow!(
                "expected 1 row to be updated, not {updated}"
            ))));
        }

        let updated = diesel::update(attributes_dsl::silo_group_scim_attributes)
            .filter(attributes_dsl::silo_group_id.eq(group_id))
            .set((
                attributes_dsl::display_name.eq(display_name),
                attributes_dsl::external_id.eq(external_id),
            ))
            .execute_async(conn)
            .await?;

        if updated != 1 {
            return Err(err.bail(ProviderStoreError::StoreError(
            anyhow!(
                "expected 1 row to be updated, not {updated}"
            ))));
        }

        let (group, attributes) = dsl::silo_group
            .inner_join(attributes_dsl::silo_group_scim_attributes
                .on(attributes_dsl::silo_group_id.eq(dsl::id))
            )
            .filter(dsl::silo_id.eq(self.silo_id))
            .filter(dsl::id.eq(group_id))
            .filter(dsl::time_deleted.is_null())
            .select((
                SiloGroup::as_select(),
                SiloGroupScimAttributes::as_select(),
            ))
            .first_async(conn)
            .await?;

        // Delete all existing group memberships for this group id

        {
            use nexus_db_schema::schema::silo_group_membership::dsl;

            diesel::delete(dsl::silo_group_membership)
                .filter(dsl::silo_group_id.eq(group_id))
                .execute_async(conn)
                .await?;
        }

        // Validate the members arg, and insert silo group membership records.

        let members = if let Some(members) = &members {
            let mut returned_members = IdOrdMap::with_capacity(members.len());
            let mut memberships = Vec::with_capacity(members.len());

            // Validate the members arg, and insert silo group membership
            // records.
            for member in members {
                let (user_id, returned_member) = self
                    .validate_group_member_in_txn(conn, err.clone(), member)
                    .await?;

                match returned_members.insert_unique(returned_member) {
                    Ok(_) => {},

                    Err(e) => {
                        return Err(err.bail(ProviderStoreError::Scim(
                            scim2_rs::Error::conflict(format!("{:?}", e.new_item()))
                        )));
                    }
                }

                memberships.push(SiloGroupMembership {
                    silo_group_id: group_id,
                    silo_user_id: user_id,
                });
            }

            use nexus_db_schema::schema::silo_group_membership::dsl;
            diesel::insert_into(dsl::silo_group_membership)
                .values(memberships)
                .execute_async(conn)
                .await?;

            Some(returned_members)
        } else {
            None
        };

        // XXX if displayName changes, invalidate role assignments, or delete
        // them all then re-check name?

        Ok(convert_to_scim_group(group, attributes, members))
    }

    async fn delete_group_by_id_in_txn(
        &self,
        conn: &async_bb8_diesel::Connection<DbConnection>,
        err: OptionalError<ProviderStoreError>,
        group_id: Uuid,
    ) -> Result<bool, diesel::result::Error> {
        use nexus_db_schema::schema::silo_group::dsl;

        let maybe_group = dsl::silo_group
            .filter(dsl::silo_id.eq(self.silo_id))
            .filter(dsl::id.eq(group_id))
            .filter(dsl::time_deleted.is_null())
            .select(SiloGroup::as_select())
            .first_async(conn)
            .await
            .optional()?;

        if maybe_group.is_none() {
            return Ok(false);
        }

        let updated = diesel::update(dsl::silo_group)
            .filter(dsl::silo_id.eq(self.silo_id))
            .filter(dsl::id.eq(group_id))
            .filter(dsl::time_deleted.is_null())
            .set(dsl::time_deleted.eq(Utc::now()))
            .execute_async(conn)
            .await?;

        if updated != 1 {
            return Err(err.bail(ProviderStoreError::StoreError(
            anyhow!(
                "expected 1 row to be updated, not {updated}"
            ))));
        }

        {
            use nexus_db_schema::schema::silo_group_scim_attributes::dsl;

            diesel::delete(dsl::silo_group_scim_attributes)
                .filter(dsl::silo_group_id.eq(group_id))
                .execute_async(conn)
                .await?;
        }

        {
            use nexus_db_schema::schema::silo_group_membership::dsl;

            diesel::delete(dsl::silo_group_membership)
                .filter(dsl::silo_group_id.eq(group_id))
                .execute_async(conn)
                .await?;
        }

        Ok(true)
    }
}

fn convert_to_scim_user(
    silo_user: SiloUser,
    attributes: SiloUserScimAttributes,
    groups: Option<Vec<UserGroup>>,
) -> StoredParts<User> {
    StoredParts {
        resource: User {
            id: silo_user.identity.id.to_string(),
            name: attributes.user_name,
            active: attributes.active,
            external_id: attributes.external_id,
            groups,
        },

        meta: StoredMeta {
            created: silo_user.identity.time_created,
            last_modified: silo_user.identity.time_modified,
            version: "W/unimplemented".to_string(),
        },
    }
}

fn convert_to_scim_group(
    silo_group: SiloGroup,
    attributes: SiloGroupScimAttributes,
    members: Option<IdOrdMap<GroupMember>>,
) -> StoredParts<Group> {
    StoredParts {
        resource: Group {
            id: silo_group.identity.id.to_string(),
            display_name: attributes.display_name,
            external_id: attributes.external_id,
            members,
        },

        meta: StoredMeta {
            created: silo_group.identity.time_created,
            last_modified: silo_group.identity.time_modified,
            version: "W/unimplemented".to_string(),
        },
    }
}

#[async_trait::async_trait]
impl ProviderStore for CrdbScimProviderStore {
    async fn get_user_by_id(
        &self,
        user_id: &str,
    ) -> Result<Option<StoredParts<User>>, ProviderStoreError> {
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

        let err: OptionalError<ProviderStoreError> = OptionalError::new();

        let maybe_user =
            self.datastore.transaction_retry_wrapper("scim_get_user_by_id")
                .transaction(&conn, |conn| {
                    let user_id = user_id.clone();
                    let err = err.clone();

                    async move {
                        self.get_user_by_id_in_txn(
                            &conn,
                            err,
                            user_id,
                        )
                        .await
                    }
                })
                .await
                .map_err(|e| {
                    if let Some(e) = err.take() {
                        e
                    } else {
                        ProviderStoreError::StoreError(e.into())
                    }
                })?;

        Ok(maybe_user)
    }

    async fn create_user(
        &self,
        user_request: CreateUserRequest,
    ) -> Result<StoredParts<User>, ProviderStoreError> {
        let conn = self
            .datastore
            .pool_connection_unauthorized()
            .await
            .map_err(|err| ProviderStoreError::StoreError(
            // XXX Error::unavail with ProviderStoreError type
                anyhow!("Failed to access DB connection: {err}")
            ))?;

        let err: OptionalError<ProviderStoreError> = OptionalError::new();

        let user =
            self.datastore.transaction_retry_wrapper("scim_create_user")
                .transaction(&conn, |conn| {
                    let user_request = user_request.clone();
                    let err = err.clone();

                    async move {
                        self.create_user_in_txn(
                            &conn,
                            err,
                            user_request,
                        )
                        .await
                    }
                })
                .await
                .map_err(|e| {
                    if let Some(e) = err.take() {
                        e
                    } else {
                        ProviderStoreError::StoreError(e.into())
                    }
                })?;

        Ok(user)
    }

    async fn list_users(
        &self,
        filter: Option<FilterOp>,
    ) -> Result<Vec<StoredParts<User>>, ProviderStoreError> {
        let conn = self
            .datastore
            .pool_connection_unauthorized()
            .await
            .map_err(|err| ProviderStoreError::StoreError(
            // XXX Error::unavail with ProviderStoreError type
                anyhow!("Failed to access DB connection: {err}")
            ))?;

        let err: OptionalError<ProviderStoreError> = OptionalError::new();

        let users =
            self.datastore.transaction_retry_wrapper("scim_list_users")
                .transaction(&conn, |conn| {
                    let err = err.clone();
                    let filter = filter.clone();

                    async move {
                        self.list_users_in_txn(
                            &conn,
                            err,
                            filter,
                        )
                        .await
                    }
                })
                .await
                .map_err(|e| {
                    if let Some(e) = err.take() {
                        e
                    } else {
                        ProviderStoreError::StoreError(e.into())
                    }
                })?;

        Ok(users)
    }

    async fn replace_user(
        &self,
        user_id: &str,
        user_request: CreateUserRequest,
    ) -> Result<StoredParts<User>, ProviderStoreError> {
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

        let err: OptionalError<ProviderStoreError> = OptionalError::new();

        let user =
            self.datastore.transaction_retry_wrapper("scim_replace_user")
                .transaction(&conn, |conn| {
                    let err = err.clone();
                    let user_request = user_request.clone();

                    async move {
                        self.replace_user_in_txn(
                            &conn,
                            err,
                            user_id,
                            user_request,
                        )
                        .await
                    }
                })
                .await
                .map_err(|e| {
                    if let Some(e) = err.take() {
                        e
                    } else {
                        ProviderStoreError::StoreError(e.into())
                    }
                })?;

        Ok(user)
    }

    async fn delete_user_by_id(
        &self,
        user_id: &str,
    ) -> Result<ProviderStoreDeleteResult, ProviderStoreError> {
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

        let err: OptionalError<ProviderStoreError> = OptionalError::new();

        let deleted_user =
            self.datastore.transaction_retry_wrapper("scim_delete_user_by_id")
                .transaction(&conn, |conn| {
                    let user_id = user_id.clone();
                    let err = err.clone();

                    async move {
                        self.delete_user_by_id_in_txn(
                            &conn,
                            err,
                            user_id,
                        )
                        .await
                    }
                })
                .await
                .map_err(|e| {
                    if let Some(e) = err.take() {
                        e
                    } else {
                        ProviderStoreError::StoreError(e.into())
                    }
                })?;

        if deleted_user {
            Ok(ProviderStoreDeleteResult::Deleted)
        } else {
            Ok(ProviderStoreDeleteResult::NotFound)
        }
    }

    async fn get_group_by_id(
        &self,
        group_id: &str,
    ) -> Result<Option<StoredParts<Group>>, ProviderStoreError> {
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

        let err: OptionalError<ProviderStoreError> = OptionalError::new();

        let maybe_group =
            self.datastore.transaction_retry_wrapper("scim_get_group_by_id")
                .transaction(&conn, |conn| {
                    let group_id = group_id.clone();
                    let err = err.clone();

                    async move {
                        self.get_group_by_id_in_txn(
                            &conn,
                            err,
                            group_id,
                        )
                        .await
                    }
                })
                .await
                .map_err(|e| {
                    if let Some(e) = err.take() {
                        e
                    } else {
                        ProviderStoreError::StoreError(e.into())
                    }
                })?;

        Ok(maybe_group)
    }

    async fn create_group(
        &self,
        group_request: CreateGroupRequest,
    ) -> Result<StoredParts<Group>, ProviderStoreError> {
        let conn = self
            .datastore
            .pool_connection_unauthorized()
            .await
            .map_err(|err| ProviderStoreError::StoreError(
            // XXX Error::unavail with ProviderStoreError type
                anyhow!("Failed to access DB connection: {err}")
            ))?;

        let err: OptionalError<ProviderStoreError> = OptionalError::new();

        let group = self
            .datastore
            .transaction_retry_wrapper("scim_create_group")
            .transaction(&conn, |conn| {
                let group_request = group_request.clone();
                let err = err.clone();

                async move {
                    self.create_group_in_txn(
                        &conn,
                        err,
                        group_request,
                    )
                    .await
                }
            })
            .await
            .map_err(|e| {
                if let Some(e) = err.take() {
                    e
                } else {
                    ProviderStoreError::StoreError(e.into())
                }
            })?;

        Ok(group)
    }

    async fn list_groups(
        &self,
        filter: Option<FilterOp>,
    ) -> Result<Vec<StoredParts<Group>>, ProviderStoreError> {
        let conn = self
            .datastore
            .pool_connection_unauthorized()
            .await
            .map_err(|err| ProviderStoreError::StoreError(
            // XXX Error::unavail with ProviderStoreError type
                anyhow!("Failed to access DB connection: {err}")
            ))?;

        let err: OptionalError<ProviderStoreError> = OptionalError::new();

        let groups =
            self.datastore.transaction_retry_wrapper("scim_list_groups")
                .transaction(&conn, |conn| {
                    let err = err.clone();
                    let filter = filter.clone();

                    async move {
                        self.list_groups_in_txn(
                            &conn,
                            err,
                            filter,
                        )
                        .await
                    }
                })
                .await
                .map_err(|e| {
                    if let Some(e) = err.take() {
                        e
                    } else {
                        ProviderStoreError::StoreError(e.into())
                    }
                })?;

        Ok(groups)
    }

    async fn replace_group(
        &self,
        group_id: &str,
        group_request: CreateGroupRequest,
    ) -> Result<StoredParts<Group>, ProviderStoreError> {
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

        let err: OptionalError<ProviderStoreError> = OptionalError::new();

        let group =
            self.datastore.transaction_retry_wrapper("scim_replace_group")
                .transaction(&conn, |conn| {
                    let err = err.clone();
                    let group_request = group_request.clone();

                    async move {
                        self.replace_group_in_txn(
                            &conn,
                            err,
                            group_id,
                            group_request,
                        )
                        .await
                    }
                })
                .await
                .map_err(|e| {
                    if let Some(e) = err.take() {
                        e
                    } else {
                        ProviderStoreError::StoreError(e.into())
                    }
                })?;

        Ok(group)
    }

    // Delete a group, and all group memberships.
    //
    // true is returned if the Group existed prior to the delete, otherwise
    // false is returned.
    async fn delete_group_by_id(
        &self,
        group_id: &str,
    ) -> Result<ProviderStoreDeleteResult, ProviderStoreError> {
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

        let err: OptionalError<ProviderStoreError> = OptionalError::new();

        let deleted_group = self
            .datastore
            .transaction_retry_wrapper("scim_delete_group_by_id")
            .transaction(&conn, |conn| {
                let err = err.clone();

                async move {
                    self.delete_group_by_id_in_txn(
                        &conn,
                        err,
                        group_id,
                    )
                    .await
                }
            })
            .await
            .map_err(|e| {
                if let Some(e) = err.take() {
                    e
                } else {
                    ProviderStoreError::StoreError(e.into())
                }
            })?;

        if deleted_group {
            Ok(ProviderStoreDeleteResult::Deleted)
        } else {
            Ok(ProviderStoreDeleteResult::NotFound)
        }
    }
}
