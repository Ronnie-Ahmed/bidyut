DROP INDEX idx_notifications_done;
DROP INDEX idx_reports_created;
ALTER TABLE reports ADD COLUMN lat DOUBLE PRECISION, ADD COLUMN lng DOUBLE PRECISION;