-- A generation number on each Claude link, so a refresh that started before
-- a new login can neither overwrite the new login's tokens nor mark them
-- broken.
--
-- claude_link_generations holds the last generation handed out. It only
-- grows, so a link deleted by a logout and stored again by a new login gets
-- a generation no earlier refresh can hold.

ALTER TABLE claude_links ADD COLUMN generation INTEGER NOT NULL DEFAULT 0;

CREATE TABLE claude_link_generations (
    id INTEGER PRIMARY KEY NOT NULL CHECK (id = 1),
    last INTEGER NOT NULL
) STRICT;

INSERT INTO claude_link_generations (id, last) VALUES (1, 0);
