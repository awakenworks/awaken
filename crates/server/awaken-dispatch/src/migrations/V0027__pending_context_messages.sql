-- retain stable context Messages accepted with a durable pending input
ALTER TABLE {prefix}_pending ADD COLUMN context_messages {json}
