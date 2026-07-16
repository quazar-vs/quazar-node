CREATE TABLE IF NOT EXISTS block_state (
    id INTEGER PRIMARY KEY DEFAULT 1,
    next_block_number INTEGER NOT NULL,
    next_required_events INTEGER NOT NULL,
    last_block_size INTEGER NOT NULL,
    last_block_closed_by_timeout BOOLEAN DEFAULT FALSE,
    updated_at TIMESTAMPTZ DEFAULT NOW()
);

INSERT INTO block_state (id, next_block_number, next_required_events, last_block_size, last_block_closed_by_timeout)
VALUES (1, 1, 1, 0, FALSE)
ON CONFLICT (id) DO NOTHING;
