-- `vec0` is a virtual table, so the columns live alongside the vector: the
-- citation/text fields are auxiliary (`+`, stored separately, no WHERE use) and
-- `hidden` is a metadata column so it can filter a KNN query.
CREATE VIRTUAL TABLE recalls USING vec0(
    id          INTEGER PRIMARY KEY,
    embedding   float[1024],
    hidden      BOOLEAN,
    +citation_text     TEXT,
    +citation_username TEXT,
    +text              TEXT
);