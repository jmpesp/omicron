CREATE UNIQUE INDEX IF NOT EXISTS lookup_silo_user_by_silo_only ON omicron.public.silo_user (
    silo_id
) WHERE
    time_deleted IS NULL;
