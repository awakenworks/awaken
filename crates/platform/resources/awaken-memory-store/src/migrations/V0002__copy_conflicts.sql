-- Version 2 of the converged bundle; published legacy bundles remain unchanged.
CREATE TABLE {prefix}_copy_conflicts (
    store_id TEXT NOT NULL,
    id TEXT NOT NULL,
    candidate_json TEXT NOT NULL,
    PRIMARY KEY (store_id, id)
);
