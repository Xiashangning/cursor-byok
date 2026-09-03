-- Persist the selectable Cursor model parameter values configured in the desktop UI.
ALTER TABLE model_configs ADD COLUMN effort_options_json TEXT NOT NULL DEFAULT '["low","medium","high","xhigh","max"]';
ALTER TABLE model_configs ADD COLUMN context_options_json TEXT NOT NULL DEFAULT '["200k","356k","800k","1m"]';
