ALTER TABLE base_prompts RENAME COLUMN active_base_prompt    TO active_instructions;
ALTER TABLE base_prompts RENAME COLUMN practical_base_prompt TO practical_instructions;

ALTER TABLE base_prompts RENAME COLUMN active_llm_tools     TO active_tool_definitions;
ALTER TABLE base_prompts RENAME COLUMN practical_llm_tools  TO practical_tool_definitions;
