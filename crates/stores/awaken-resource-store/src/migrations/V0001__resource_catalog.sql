-- create the Resources-owned Resource Catalog aggregate
CREATE TABLE {prefix}_entry (
    kind TEXT NOT NULL,
    id TEXT NOT NULL,
    data {json} NOT NULL,
    PRIMARY KEY (kind, id)
)
