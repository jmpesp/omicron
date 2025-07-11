CREATE UNIQUE INDEX IF NOT EXISTS lookup_scim_client_by_silo_id
ON omicron.public.silo_scim_client (
    silo_id,
    id
) WHERE
    time_deleted IS NULL;
