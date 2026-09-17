-- At most one queued/running operation may own an instance. This database
-- invariant backs the per-instance coordinator lock across processes and
-- daemon restarts; terminal history remains unrestricted.
CREATE UNIQUE INDEX ux_operations_one_active_per_instance
    ON operations (instance_id)
    WHERE instance_id IS NOT NULL AND state IN ('queued', 'running');
