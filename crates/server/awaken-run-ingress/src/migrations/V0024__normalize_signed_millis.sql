-- normalize legacy negative millisecond values written by unchecked u64-to-i64 casts
UPDATE {prefix}_dispatch
SET lease_until = 9223372036854775807
WHERE lease_until < 0;
UPDATE {prefix}_dispatch
SET dead_lettered_at = 9223372036854775807
WHERE dead_lettered_at < 0;
UPDATE {prefix}_pending
SET available_at = 9223372036854775807
WHERE available_at < 0;
UPDATE {prefix}_dispatch_operation
SET recorded_at_ms = 9223372036854775807
WHERE recorded_at_ms < 0
