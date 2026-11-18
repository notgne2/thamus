CREATE TABLE base_prompts (
    "instance_id" TEXT NOT NULL,

    "dream" INTEGER NOT NULL PRIMARY KEY,

    "active_base_prompt" TEXT NOT NULL,

    "practical_base_prompt" TEXT NOT NULL,

    -- JSON-encoded `Vec<LlmTool>`.
    "active_llm_tools" TEXT NOT NULL,

    "practical_llm_tools" TEXT NOT NULL,

    "created_at" TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
