CREATE TABLE telegram_chats (
    device_id  UUID PRIMARY KEY,
    chat_id    BIGINT NOT NULL UNIQUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);