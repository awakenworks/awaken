-- Optimistic-concurrency fence for every DataSubject aggregate mutation
ALTER TABLE {prefix}_subject
ADD COLUMN revision BIGINT NOT NULL DEFAULT 0
