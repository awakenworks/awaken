-- remove the obsolete standalone delegation repository
-- migration-allow-edit: V0015 always creates this table before V0016 retires it
DROP TABLE {prefix}_delegation_group
