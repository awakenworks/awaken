-- Monotonic Config-owned catalog observation; never permission authority.
-- Publication observation follows its execution Workspace, preserving independent publication locks.
CREATE TABLE {prefix}_agent_catalog_epoch (scope_id TEXT PRIMARY KEY, epoch BIGINT NOT NULL CHECK (epoch > 0));
INSERT INTO {prefix}_agent_catalog_epoch (scope_id, epoch) SELECT scope_id, 1 FROM {prefix}_agent UNION SELECT scope_id, 1 FROM {prefix}_agent_revision UNION SELECT scope_id, 1 FROM {prefix}_personal_agent_selection UNION SELECT COALESCE(record->>'execution_workspace', scope_id), 1 FROM {prefix}_publication;
CREATE FUNCTION {prefix}_advance_agent_catalog_epoch() RETURNS TRIGGER AS $$
BEGIN
IF TG_OP = 'DELETE' THEN
INSERT INTO {prefix}_agent_catalog_epoch (scope_id, epoch) VALUES (OLD.scope_id, 1) ON CONFLICT (scope_id) DO UPDATE SET epoch = {prefix}_agent_catalog_epoch.epoch + 1;
RETURN OLD;
END IF;
INSERT INTO {prefix}_agent_catalog_epoch (scope_id, epoch) VALUES (NEW.scope_id, 1) ON CONFLICT (scope_id) DO UPDATE SET epoch = {prefix}_agent_catalog_epoch.epoch + 1;
IF TG_OP = 'UPDATE' AND OLD.scope_id IS DISTINCT FROM NEW.scope_id THEN
INSERT INTO {prefix}_agent_catalog_epoch (scope_id, epoch) VALUES (OLD.scope_id, 1) ON CONFLICT (scope_id) DO UPDATE SET epoch = {prefix}_agent_catalog_epoch.epoch + 1;
END IF;
RETURN NEW;
END;
$$ LANGUAGE plpgsql;
CREATE FUNCTION {prefix}_advance_agent_catalog_epoch_publication() RETURNS TRIGGER AS $$
BEGIN
IF TG_OP = 'DELETE' THEN
INSERT INTO {prefix}_agent_catalog_epoch (scope_id, epoch) VALUES (COALESCE(OLD.record->>'execution_workspace', OLD.scope_id), 1) ON CONFLICT (scope_id) DO UPDATE SET epoch = {prefix}_agent_catalog_epoch.epoch + 1;
RETURN OLD;
END IF;
INSERT INTO {prefix}_agent_catalog_epoch (scope_id, epoch) VALUES (COALESCE(NEW.record->>'execution_workspace', NEW.scope_id), 1) ON CONFLICT (scope_id) DO UPDATE SET epoch = {prefix}_agent_catalog_epoch.epoch + 1;
IF TG_OP = 'UPDATE' AND COALESCE(OLD.record->>'execution_workspace', OLD.scope_id) IS DISTINCT FROM COALESCE(NEW.record->>'execution_workspace', NEW.scope_id) THEN
INSERT INTO {prefix}_agent_catalog_epoch (scope_id, epoch) VALUES (COALESCE(OLD.record->>'execution_workspace', OLD.scope_id), 1) ON CONFLICT (scope_id) DO UPDATE SET epoch = {prefix}_agent_catalog_epoch.epoch + 1;
END IF;
RETURN NEW;
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER {prefix}_catalog_agent_change AFTER INSERT OR UPDATE OR DELETE ON {prefix}_agent FOR EACH ROW EXECUTE FUNCTION {prefix}_advance_agent_catalog_epoch();
CREATE TRIGGER {prefix}_catalog_revision_change AFTER INSERT OR UPDATE OR DELETE ON {prefix}_agent_revision FOR EACH ROW EXECUTE FUNCTION {prefix}_advance_agent_catalog_epoch();
CREATE TRIGGER {prefix}_catalog_selection_change AFTER INSERT OR UPDATE OR DELETE ON {prefix}_personal_agent_selection FOR EACH ROW EXECUTE FUNCTION {prefix}_advance_agent_catalog_epoch();
CREATE TRIGGER {prefix}_catalog_publication_change AFTER INSERT OR UPDATE OR DELETE ON {prefix}_publication FOR EACH ROW EXECUTE FUNCTION {prefix}_advance_agent_catalog_epoch_publication();
