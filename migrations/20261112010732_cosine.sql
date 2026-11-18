CREATE TABLE recalls_temp AS
SELECT id, embedding, hidden, citation_text, citation_username, text
FROM recalls;

DROP TABLE recalls;

CREATE VIRTUAL TABLE recalls USING vec0(
    id                  INTEGER PRIMARY KEY,
    embedding           float[1024] distance_metric=cosine,
    hidden              BOOLEAN,
    +citation_text      TEXT,
    +citation_username  TEXT,
    +text               TEXT
);

INSERT INTO recalls (id, embedding, hidden, citation_text, citation_username, text)
SELECT id, embedding, hidden, citation_text, citation_username, text
FROM recalls_temp;

DROP TABLE recalls_temp;
