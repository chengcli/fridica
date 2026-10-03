-- Interim progress notes a worker wrote while its job ran (#105), one row per
-- posted note; the thread inbox carries each to the thread.
CREATE TABLE job_progress (
    job_id TEXT NOT NULL REFERENCES jobs(id), attempt INTEGER NOT NULL, seq INTEGER NOT NULL,
    text TEXT NOT NULL, created REAL NOT NULL,
    PRIMARY KEY(job_id, attempt, seq)
);
-- The channel ledger (#108): which threads of a channel talk about which pull
-- request or issue (`#<number>`; `repo` is the repository when a message named
-- it), and which threads point at which other threads.
CREATE TABLE item_links (
    workspace TEXT NOT NULL, channel TEXT NOT NULL, item TEXT NOT NULL,
    session_id TEXT NOT NULL REFERENCES threads(id), repo TEXT NOT NULL DEFAULT '',
    first_seen REAL NOT NULL, last_seen REAL NOT NULL,
    PRIMARY KEY(workspace, channel, item, session_id)
);
CREATE INDEX item_links_session ON item_links(session_id);
CREATE TABLE thread_links (
    session_id TEXT NOT NULL REFERENCES threads(id), target TEXT NOT NULL REFERENCES threads(id),
    created REAL NOT NULL,
    PRIMARY KEY(session_id, target)
);
CREATE INDEX thread_links_target ON thread_links(target);
-- When a job stopped by the backend's usage limit may run again (#107).
ALTER TABLE jobs ADD COLUMN retry_at REAL;
