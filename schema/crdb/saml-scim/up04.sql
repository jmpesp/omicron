CREATE UNIQUE INDEX IF NOT EXISTS bearer_token_unique_for_scim_client
ON omicron.public.silo_scim_client (
    bearer_token
) WHERE
    time_deleted IS NULL;
