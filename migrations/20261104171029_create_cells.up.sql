CREATE TABLE cells (
    id           BIGSERIAL PRIMARY KEY,
    lat_idx      INTEGER NOT NULL,
    lng_idx      INTEGER NOT NULL,
    status       TEXT NOT NULL DEFAULT 'up' CHECK (status IN ('up', 'down')),
    status_since TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (lat_idx, lng_idx)
);