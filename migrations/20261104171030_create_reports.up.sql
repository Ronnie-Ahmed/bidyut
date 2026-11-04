CREATE TABLE reports (
    id         BIGSERIAL PRIMARY KEY,
    cell_id    BIGINT NOT NULL REFERENCES cells(id),
    device_id  UUID NOT NULL,
    kind       TEXT NOT NULL CHECK (kind IN ('outage', 'restored')),
    lat        DOUBLE PRECISION NOT NULL,
    lng        DOUBLE PRECISION NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_reports_cell_created ON reports (cell_id, created_at DESC);
CREATE INDEX idx_reports_device_created ON reports (device_id, created_at DESC);