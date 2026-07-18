-- normalize legacy parked dispatch rows to awaiting
UPDATE {prefix}_dispatch SET status = 'awaiting' WHERE status = 'parked';
