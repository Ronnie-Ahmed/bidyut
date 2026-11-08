ALTER TABLE reports DROP COLUMN lat, DROP COLUMN lng;

CREATE INDEX idx_reports_created ON reports (created_at);
CREATE INDEX idx_notifications_done ON notifications (created_at) WHERE status <> 'pending';