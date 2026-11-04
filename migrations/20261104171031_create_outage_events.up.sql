CREATE TABLE outage_events (
    id         BIGSERIAL PRIMARY KEY,
    cell_id    BIGINT NOT NULL REFERENCES cells(id),
    started_at TIMESTAMPTZ NOT NULL,
    ended_at   TIMESTAMPTZ,
    CHECK (ended_at IS NULL OR ended_at > started_at)
);

CREATE UNIQUE INDEX uniq_open_event_per_cell
    ON outage_events (cell_id) WHERE ended_at IS NULL;

CREATE INDEX idx_events_cell_started ON outage_events (cell_id, started_at DESC);