-- durable ordered outbox for dispatch authority transitions
CREATE TABLE {prefix}_dispatch_operation (
    sequence {pk_autoinc},
    run_id TEXT NOT NULL,
    operation {json} NOT NULL
)
