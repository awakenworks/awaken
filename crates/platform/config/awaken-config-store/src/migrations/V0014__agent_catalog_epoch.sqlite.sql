-- Monotonic Config-owned catalog observation; never permission authority.
-- Publication observation follows its execution Workspace, preserving independent publication locks.
CREATE TABLE {prefix}_agent_catalog_epoch (scope_id TEXT PRIMARY KEY, epoch INTEGER NOT NULL CHECK (typeof(epoch) = 'integer' AND epoch > 0));
INSERT INTO {prefix}_agent_catalog_epoch (scope_id, epoch) SELECT scope_id, 1 FROM {prefix}_agent UNION SELECT scope_id, 1 FROM {prefix}_agent_revision UNION SELECT scope_id, 1 FROM {prefix}_personal_agent_selection UNION SELECT COALESCE(json_extract(record, '$.execution_workspace'), scope_id), 1 FROM {prefix}_publication;
CREATE TRIGGER {prefix}_catalog_agent_insert AFTER INSERT ON {prefix}_agent BEGIN
INSERT INTO {prefix}_agent_catalog_epoch (scope_id, epoch) VALUES (NEW.scope_id, 1) ON CONFLICT (scope_id) DO UPDATE SET epoch = epoch + 1;
END;
CREATE TRIGGER {prefix}_catalog_agent_update AFTER UPDATE ON {prefix}_agent BEGIN
INSERT INTO {prefix}_agent_catalog_epoch (scope_id, epoch) VALUES (NEW.scope_id, 1) ON CONFLICT (scope_id) DO UPDATE SET epoch = epoch + 1;
INSERT INTO {prefix}_agent_catalog_epoch (scope_id, epoch) SELECT OLD.scope_id, 1 WHERE OLD.scope_id <> NEW.scope_id ON CONFLICT (scope_id) DO UPDATE SET epoch = epoch + 1;
END;
CREATE TRIGGER {prefix}_catalog_agent_delete AFTER DELETE ON {prefix}_agent BEGIN
INSERT INTO {prefix}_agent_catalog_epoch (scope_id, epoch) VALUES (OLD.scope_id, 1) ON CONFLICT (scope_id) DO UPDATE SET epoch = epoch + 1;
END;
CREATE TRIGGER {prefix}_catalog_revision_insert AFTER INSERT ON {prefix}_agent_revision BEGIN
INSERT INTO {prefix}_agent_catalog_epoch (scope_id, epoch) VALUES (NEW.scope_id, 1) ON CONFLICT (scope_id) DO UPDATE SET epoch = epoch + 1;
END;
CREATE TRIGGER {prefix}_catalog_revision_update AFTER UPDATE ON {prefix}_agent_revision BEGIN
INSERT INTO {prefix}_agent_catalog_epoch (scope_id, epoch) VALUES (NEW.scope_id, 1) ON CONFLICT (scope_id) DO UPDATE SET epoch = epoch + 1;
INSERT INTO {prefix}_agent_catalog_epoch (scope_id, epoch) SELECT OLD.scope_id, 1 WHERE OLD.scope_id <> NEW.scope_id ON CONFLICT (scope_id) DO UPDATE SET epoch = epoch + 1;
END;
CREATE TRIGGER {prefix}_catalog_revision_delete AFTER DELETE ON {prefix}_agent_revision BEGIN
INSERT INTO {prefix}_agent_catalog_epoch (scope_id, epoch) VALUES (OLD.scope_id, 1) ON CONFLICT (scope_id) DO UPDATE SET epoch = epoch + 1;
END;
CREATE TRIGGER {prefix}_catalog_selection_insert AFTER INSERT ON {prefix}_personal_agent_selection BEGIN
INSERT INTO {prefix}_agent_catalog_epoch (scope_id, epoch) VALUES (NEW.scope_id, 1) ON CONFLICT (scope_id) DO UPDATE SET epoch = epoch + 1;
END;
CREATE TRIGGER {prefix}_catalog_selection_update AFTER UPDATE ON {prefix}_personal_agent_selection BEGIN
INSERT INTO {prefix}_agent_catalog_epoch (scope_id, epoch) VALUES (NEW.scope_id, 1) ON CONFLICT (scope_id) DO UPDATE SET epoch = epoch + 1;
INSERT INTO {prefix}_agent_catalog_epoch (scope_id, epoch) SELECT OLD.scope_id, 1 WHERE OLD.scope_id <> NEW.scope_id ON CONFLICT (scope_id) DO UPDATE SET epoch = epoch + 1;
END;
CREATE TRIGGER {prefix}_catalog_selection_delete AFTER DELETE ON {prefix}_personal_agent_selection BEGIN
INSERT INTO {prefix}_agent_catalog_epoch (scope_id, epoch) VALUES (OLD.scope_id, 1) ON CONFLICT (scope_id) DO UPDATE SET epoch = epoch + 1;
END;
CREATE TRIGGER {prefix}_catalog_publication_insert AFTER INSERT ON {prefix}_publication BEGIN
INSERT INTO {prefix}_agent_catalog_epoch (scope_id, epoch) VALUES (COALESCE(json_extract(NEW.record, '$.execution_workspace'), NEW.scope_id), 1) ON CONFLICT (scope_id) DO UPDATE SET epoch = epoch + 1;
END;
CREATE TRIGGER {prefix}_catalog_publication_update AFTER UPDATE ON {prefix}_publication BEGIN
INSERT INTO {prefix}_agent_catalog_epoch (scope_id, epoch) VALUES (COALESCE(json_extract(NEW.record, '$.execution_workspace'), NEW.scope_id), 1) ON CONFLICT (scope_id) DO UPDATE SET epoch = epoch + 1;
INSERT INTO {prefix}_agent_catalog_epoch (scope_id, epoch) SELECT COALESCE(json_extract(OLD.record, '$.execution_workspace'), OLD.scope_id), 1 WHERE COALESCE(json_extract(OLD.record, '$.execution_workspace'), OLD.scope_id) <> COALESCE(json_extract(NEW.record, '$.execution_workspace'), NEW.scope_id) ON CONFLICT (scope_id) DO UPDATE SET epoch = epoch + 1;
END;
CREATE TRIGGER {prefix}_catalog_publication_delete AFTER DELETE ON {prefix}_publication BEGIN
INSERT INTO {prefix}_agent_catalog_epoch (scope_id, epoch) VALUES (COALESCE(json_extract(OLD.record, '$.execution_workspace'), OLD.scope_id), 1) ON CONFLICT (scope_id) DO UPDATE SET epoch = epoch + 1;
END;
