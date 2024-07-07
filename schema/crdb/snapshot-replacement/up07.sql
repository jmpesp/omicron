CREATE UNIQUE INDEX IF NOT EXISTS unique_snapshot_replacement_per_volume
    on omicron.public.snapshot_replacement_step (volume_id)
    WHERE replacement_state != 'complete';
