ALTER TABLE outage_events
    ADD COLUMN closed_by TEXT CHECK (closed_by IN ('reports', 'timeout'));

CREATE TABLE subscriptions (
    id         BIGSERIAL PRIMARY KEY,
    device_id  UUID NOT NULL,
    cell_id    BIGINT NOT NULL REFERENCES cells(id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (device_id, cell_id)
);

CREATE INDEX idx_subscriptions_cell ON subscriptions (cell_id);

CREATE TABLE notifications (
    id              BIGSERIAL PRIMARY KEY,
    subscription_id BIGINT NOT NULL REFERENCES subscriptions(id) ON DELETE CASCADE,
    cell_id         BIGINT NOT NULL REFERENCES cells(id),
    kind            TEXT NOT NULL CHECK (kind IN ('down', 'restored', 'expired')),
    status          TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'sent', 'failed')),
    attempts        INTEGER NOT NULL DEFAULT 0,
    next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_error      TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    sent_at         TIMESTAMPTZ
);

CREATE INDEX idx_notifications_pending
    ON notifications (next_attempt_at) WHERE status = 'pending';