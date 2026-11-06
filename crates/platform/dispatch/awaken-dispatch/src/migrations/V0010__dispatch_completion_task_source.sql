-- retain the original Task-control request in its existing completion transaction
ALTER TABLE {prefix}_dispatch_completion ADD COLUMN retained_request {json}
