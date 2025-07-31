// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! [`DataStore`] methods related to SCIM

use anyhow::anyhow;
use async_bb8_diesel::AsyncRunQueryDsl;
use chrono::Utc;
use crate::authz;
use crate::context::OpContext;
use crate::db::model::SiloGroup;
use crate::db::model::SiloScimClientBearerToken;
use crate::db::model::SiloGroupMembership;
use crate::db::model::SiloUser;
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

        let tuples: Vec<(Uuid, String)> = dsl::silo_group_membership
            .inner_join(group_dsl::silo_group
                .on(dsl::silo_group_id.eq(group_dsl::id))
            )
            .filter(group_dsl::silo_id.eq(self.silo_id))
            .filter(dsl::silo_user_id.eq(user_id))
            .select((
                group_dsl::id,
                group_dsl::external_id, // XXX external id is not display name
            ))
            .load_async(conn)
            .await?;

        if tuples.is_empty() {
            Ok(None)
        } else {
            let groups = tuples
                .into_iter()
                .map(|(group_id, display_name)|
                    UserGroup {
                        member_type: Some(UserGroupType::Direct),
                        value: Some(group_id.to_string()),
                        display: Some(display_name),
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
    ) -> Result<Option<Vec<GroupMember>>, diesel::result::Error> {
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

            // XXX wrap up as query function?
            dsl::silo_user
                .filter(dsl::silo_id.eq(self.silo_id))
                .filter(dsl::id.eq(user_id))
                .filter(dsl::time_deleted.is_null())
                .select(SiloUser::as_returning())
                .first_async(conn)
                .await
                .optional()?
        };

        let Some(user) = maybe_user else {
            return Ok(None);
        };

        let groups: Option<Vec<UserGroup>> = self.get_user_groups_for_user_in_txn(
            conn, user_id,
        ).await?;

        Ok(Some(convert_to_scim_user(user, groups)))
    }

    async fn list_users_in_txn(
        &self,
        conn: &async_bb8_diesel::Connection<DbConnection>,
        err: OptionalError<ProviderStoreError>,
        filter: Option<FilterOp>,
    ) -> Result<Vec<StoredParts<User>>, diesel::result::Error> {
        use nexus_db_schema::schema::silo_user::dsl;

        let query = dsl::silo_user
            .filter(dsl::silo_id.eq(self.silo_id))
            .filter(dsl::time_deleted.is_null());

        match filter {
            Some(FilterOp::UserNameEq(_username)) => {
                // XXX case sensitive?
                // define_sql_function!(fn lower(a: diesel::sql_types::VarChar) -> diesel::sql_types::VarChar);

                /* // XXX no username
                query = query
                    .filter(dsl::user_name.eq(username.clone()));
                */
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
            .select(SiloUser::as_returning())
            .load_async(conn)
            .await?;

        let mut returned_users = Vec::with_capacity(users.len());

        for user in users {
            let groups = self.get_user_groups_for_user_in_txn(
                conn,
                user.identity.id,
            ).await?;

            returned_users.push(convert_to_scim_user(user, groups));
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

        // XXX wrap up as query function?
        let maybe_user = dsl::silo_user
            .filter(dsl::silo_id.eq(self.silo_id))
            .filter(dsl::id.eq(user_id))
            .filter(dsl::time_deleted.is_null())
            .select(SiloUser::as_returning())
            .first_async(conn)
            .await
            .optional()?;

        if maybe_user.is_none() {
            return Err(err.bail(scim2_rs::Error::not_found(user_id.to_string()).into()));
        };

        // userName is meant to be unique: If the user request is changing the
        // userName to one that already exists, reject it

        /*
        // XXX no username
        let maybe_user = dsl::silo_user
            .filter(dsl::silo_id.eq(self.silo_id))
            //.filter(dsl::user_name.eq(user_request.name))
            .filter(dsl::id.ne(user_id))
            .filter(dsl::time_deleted.is_null())
            .select(SiloUser::as_returning())
            .first_async(conn)
            .await
            .optional()?;

        let Some(user) = maybe_user else {
            return Err(err.bail(
                scim2_rs::Error::conflict(format!("username {}", user_request.name)).into()
            ));
        };
        */

        // Overwrite all fields based on CreateUserRequest, except groups: it's
        // invalid to change group memberships in a Users PUT.

        let CreateUserRequest {
            name,
            active,
            external_id,
            groups: _,
        } = user_request;

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
                //dsl::name.eq(name), // XXX
                //dsl::active.eq(active), // XXX
                //dsl::external_id.eq(external_id),// XXX Option
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

        let user = dsl::silo_user
            .filter(dsl::silo_id.eq(self.silo_id))
            .filter(dsl::id.eq(user_id))
            .filter(dsl::time_deleted.is_null())
            .select(SiloUser::as_select())
            .first_async(conn)
            .await?;

        // Groups don't change, so query for what was previously there

        let groups = self.get_user_groups_for_user_in_txn(
            conn,
            user.identity.id,
        ).await?;

        Ok(convert_to_scim_user(user, groups))
    }

    async fn delete_user_by_id_in_txn(
        &self,
        conn: &async_bb8_diesel::Connection<DbConnection>,
        err: OptionalError<ProviderStoreError>,
        user_id: Uuid,
    ) -> Result<Option<()>, diesel::result::Error> {
        use nexus_db_schema::schema::silo_user::dsl;

        let maybe_user = dsl::silo_user
            .filter(dsl::silo_id.eq(self.silo_id))
            .filter(dsl::id.eq(user_id))
            .filter(dsl::time_deleted.is_null())
            .select(SiloUser::as_select())
            .first_async(conn)
            .await
            .optional()?;

        let Some(_user) = maybe_user else {
            return Ok(None);
        };

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
            use nexus_db_schema::schema::silo_group_membership::dsl;

            diesel::delete(dsl::silo_group_membership)
                .filter(dsl::silo_user_id.eq(user_id))
                .execute_async(conn)
                .await?;
        }

        Ok(Some(()))
    }

    async fn get_group_by_id_in_txn(
        &self,
        conn: &async_bb8_diesel::Connection<DbConnection>,
        _err: OptionalError<ProviderStoreError>,
        group_id: Uuid,
    ) -> Result<Option<StoredParts<Group>>, diesel::result::Error> {
        use nexus_db_schema::schema::silo_group::dsl;
        let maybe_group = dsl::silo_group
            .filter(dsl::silo_id.eq(self.silo_id))
            .filter(dsl::id.eq(group_id))
            .filter(dsl::time_deleted.is_null())
            .select(SiloGroup::as_returning())
            .first_async(conn)
            .await
            .optional()?;

        let Some(group) = maybe_group else {
            return Ok(None);
        };

        let members: Option<Vec<GroupMember>> = self.get_group_members_for_group_in_txn(
            conn, group_id,
        ).await?;

        Ok(Some(convert_to_scim_group(group, members)))
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
                        return Err(err.bail(scim2_rs::Error::not_found(value.to_string()).into()));
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
                    return Err(err.bail(scim2_rs::Error::not_found(value.clone()).into()));
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
        let group_id = Uuid::new_v4();

        let CreateGroupRequest { display_name, external_id, members } =
            group_request;

        let new_group = SiloGroup::new(
            group_id,
            self.silo_id,
            external_id.unwrap_or(display_name), // XXX group needs display name
        );

        use nexus_db_schema::schema::silo_group::dsl;
        diesel::insert_into(dsl::silo_group)
            .values(new_group.clone())
            .execute_async(conn)
            .await?;

        let members = if let Some(members) = &members {
            let mut returned_members = Vec::with_capacity(members.len());
            let mut memberships = Vec::with_capacity(members.len());

            // Validate the members arg, and insert silo group membership
            // records.
            for member in members {
                let err = err.clone();
                let (user_id, returned_member) = self
                    .validate_group_member_in_txn(conn, err, member)
                    .await?;

                returned_members.push(returned_member);

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

        Ok(convert_to_scim_group(new_group, members))
    }

    async fn list_groups_in_txn(
        &self,
        conn: &async_bb8_diesel::Connection<DbConnection>,
        err: OptionalError<ProviderStoreError>,
        filter: Option<FilterOp>,
    ) -> Result<Vec<StoredParts<Group>>, diesel::result::Error> {
        use nexus_db_schema::schema::silo_group::dsl;
        let query = dsl::silo_group
            .filter(dsl::silo_id.eq(self.silo_id))
            .filter(dsl::time_deleted.is_null());

        match filter {
            Some(FilterOp::DisplayNameEq(_display_name)) => {
                // XXX case sensitive?
                // define_sql_function!(fn lower(a: diesel::sql_types::VarChar) -> diesel::sql_types::VarChar);

                /* // XXX no display name
                query = query
                    .filter(dsl::display_name.eq(display_name.clone()));
                */
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
            .select(SiloGroup::as_returning())
            .load_async(&*conn)
            .await?;

        let mut returned_groups = Vec::with_capacity(groups.len());

        for group in groups {
            let members = self.get_group_members_for_group_in_txn(
                conn,
                group.identity.id,
            ).await?;

            returned_groups.push(convert_to_scim_group(group, members));
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

        // displayName is meant to be unique: If the group request is changing
        // the displayName to one that already exists, reject it.

        /*
        // XXX no displayName
        let maybe_group = dsl::silo_group
            .filter(dsl::silo_id.eq(self.silo_id))
            //.filter(dsl::display_name.eq(group_request.name))
            .filter(dsl::id.ne(group_id))
            .filter(dsl::time_deleted.is_null())
            .select(SiloGroup::as_returning())
            .first_async(conn)
            .await
            .optional()?;

        let Some(group) = maybe_group else {
            return Err(err.bail(
                scim2_rs::Error::conflict(format!("displayName {}", group_request.display_name)).into()
            ));
        };
        */

        // Overwrite all fields based on CreateGroupRequest.

        let CreateGroupRequest {
            display_name,
            external_id,
            members,
        } = group_request;

        let updated = diesel::update(dsl::silo_group)
            .filter(dsl::silo_id.eq(self.silo_id))
            .filter(dsl::id.eq(group_id))
            .filter(dsl::time_deleted.is_null())
            .set((
                //dsl::display_name.eq(display_name), // XXX
                //dsl::external_id.eq(external_id),// XXX Option
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

        let group = dsl::silo_group
            .filter(dsl::silo_id.eq(self.silo_id))
            .filter(dsl::id.eq(group_id))
            .filter(dsl::time_deleted.is_null())
            .select(SiloGroup::as_select())
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
            let mut returned_members = Vec::with_capacity(members.len());
            let mut memberships = Vec::with_capacity(members.len());

            // Validate the members arg, and insert silo group membership
            // records.
            for member in members {
                let err = err.clone();
                let (user_id, returned_member) = self
                    .validate_group_member_in_txn(conn, err, member)
                    .await?;

                returned_members.push(returned_member);

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

        Ok(convert_to_scim_group(group, members))
    }

    async fn delete_group_by_id_in_txn(
        &self,
        conn: &async_bb8_diesel::Connection<DbConnection>,
        err: OptionalError<ProviderStoreError>,
        group_id: Uuid,
    ) -> Result<Option<()>, diesel::result::Error> {
        use nexus_db_schema::schema::silo_group::dsl;

        let maybe_group = dsl::silo_group
            .filter(dsl::silo_id.eq(self.silo_id))
            .filter(dsl::id.eq(group_id))
            .filter(dsl::time_deleted.is_null())
            .select(SiloGroup::as_select())
            .first_async(conn)
            .await
            .optional()?;

        let Some(_group) = maybe_group else {
            return Ok(None);
        };

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
            use nexus_db_schema::schema::silo_group_membership::dsl;

            diesel::delete(dsl::silo_group_membership)
                .filter(dsl::silo_group_id.eq(group_id))
                .execute_async(conn)
                .await?;
        }

        Ok(Some(()))
    }
}

fn convert_to_scim_user(silo_user: SiloUser, groups: Option<Vec<UserGroup>>) -> StoredParts<User> {
    StoredParts {
        resource: User {
            id: silo_user.identity.id.to_string(),
            name: silo_user.identity.id.to_string(), // XXX
            active: Some(true), // XXX
            external_id: Some(silo_user.external_id), // XXX
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
    members: Option<Vec<GroupMember>>,
) -> StoredParts<Group> {
    StoredParts {
        resource: Group {
            id: silo_group.identity.id.to_string(),
            display_name: silo_group.identity.id.to_string(), // XXX
            external_id: Some(silo_group.external_id), // XXX
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
        user_id: String,
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

        Ok(convert_to_scim_user(new_user, None))
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
        user_id: String,
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
        user_id: String,
    ) -> Result<Option<()>, ProviderStoreError> {
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

        let maybe_deleted_user =
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

        Ok(maybe_deleted_user)
    }

    async fn get_group_by_id(
        &self,
        group_id: String,
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
        group_id: String,
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
    // A Some(StoredParts<Group>) is returned if the Group existed prior to the
    // delete, otherwise None is returned.
    async fn delete_group_by_id(
        &self,
        group_id: String,
    ) -> Result<Option<()>, ProviderStoreError> {
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

        let maybe_existing = self
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

        Ok(maybe_existing)
    }
}
