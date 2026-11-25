-- A synthetic OpenCode v2 database: the two tables a session is made of
-- (`session_v2`, `session_message`), with the columns and indexes OpenCode
-- writes, and one hand-authored session in the shape OpenCode v2.3.x produces.
-- Nothing here is a capture of a real session: the project, the prompts and
-- every path are invented. The foreign keys of the real tables (to `project`,
-- which the provider never reads) are left out so the file stands alone.
--
-- A root session `ses_demo_root` spawns two agents. The explorer finishes and
-- is later resumed; the reviewer's call errors. One tool exits non-zero, one
-- call is still running, and the `idle` and `synthetic` rows are there because
-- the provider is meant to state nothing for them.
--
-- The tests load this into a temporary database; regenerate the goldens next
-- to it with `UPDATE_GOLDEN=1 cargo test`.

CREATE TABLE `session_v2` (
  `id` text PRIMARY KEY,
  `project_id` text NOT NULL,
  `workspace_id` text,
  `parent_id` text,
  `fork_session_id` text,
  `fork_boundary` text,
  `slug` text NOT NULL,
  `directory` text NOT NULL,
  `path` text,
  `title` text,
  `version` text NOT NULL,
  `share_url` text,
  `summary_additions` integer,
  `summary_deletions` integer,
  `summary_files` integer,
  `summary_diffs` text,
  `metadata` text,
  `cost` real DEFAULT 0 NOT NULL,
  `tokens_input` integer DEFAULT 0 NOT NULL,
  `tokens_output` integer DEFAULT 0 NOT NULL,
  `tokens_reasoning` integer DEFAULT 0 NOT NULL,
  `tokens_cache_read` integer DEFAULT 0 NOT NULL,
  `tokens_cache_write` integer DEFAULT 0 NOT NULL,
  `revert` text,
  `permission` text,
  `agent` text,
  `model` text,
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL,
  `time_idle` integer,
  `time_viewed` integer,
  `idle_outcome` text,
  `time_compacting` integer,
  `time_archived` integer,
  `time_suspended` integer,
  `resume_attempts` integer DEFAULT 0 NOT NULL,
  `time_busy` integer
);
CREATE INDEX `session_v2_parent_idx` ON `session_v2` (`parent_id`);
CREATE INDEX `session_v2_project_idx` ON `session_v2` (`project_id`);

CREATE TABLE `session_message` (
  `id` text PRIMARY KEY,
  `session_id` text NOT NULL,
  `type` text NOT NULL,
  `seq` integer NOT NULL,
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL,
  `data` text NOT NULL
);
CREATE UNIQUE INDEX `session_message_session_seq_idx` ON `session_message` (`session_id`,`seq`);
CREATE INDEX `session_message_session_time_created_id_idx` ON `session_message` (`session_id`,`time_created`,`id`);

INSERT INTO session_v2 (id, project_id, parent_id, slug, directory, title, version, agent, time_created, time_updated) VALUES ('ses_demo_root', 'prj_demo', NULL, 'root', '/home/dev/shop', 'Add a dark-mode toggle', '2.3.2', 'build', 1793527200000, 1793527286000);
INSERT INTO session_v2 (id, project_id, parent_id, slug, directory, title, version, agent, time_created, time_updated) VALUES ('ses_demo_explore', 'prj_demo', 'ses_demo_root', 'explore', '/home/dev/shop', 'Map the theme code', '2.3.2', 'explore', 1793527205000, 1793527296000);
INSERT INTO session_v2 (id, project_id, parent_id, slug, directory, title, version, agent, time_created, time_updated) VALUES ('ses_demo_review', 'prj_demo', 'ses_demo_root', 'review', '/home/dev/shop', 'Review the diff', '2.3.2', 'general', 1793527273000, 1793527278000);

INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES ('msg_r01', 'ses_demo_root', 'user', 1, 1793527200000, 1793527200000, '{"time":{"created":1793527200000},"text":"Add a dark-mode toggle to the settings page.","files":[],"agents":[]}');
INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES ('msg_r02', 'ses_demo_root', 'assistant', 2, 1793527201000, 1793527240000, '{"time":{"created":1793527201000,"firstToken":1793527202000,"completed":1793527240000},"agent":"build","model":{"id":"demo-model","providerID":"demo"},"content":[{"type":"reasoning","text":"The settings page owns theme state. Read it, then ask an explorer to map the theme code.","time":{"created":1793527202000}},{"type":"tool","id":"call_read1","name":"read","executed":true,"state":{"status":"completed","input":{"path":"/home/dev/shop/src/settings.tsx"},"metadata":{"truncated":false}},"time":{"created":1793527203000,"ran":1793527203000,"completed":1793527204000}},{"type":"tool","id":"call_explore","name":"subagent","executed":true,"state":{"status":"completed","input":{"agent":"explore","description":"Map the theme code","prompt":"Find where theme state lives and how it is applied."},"metadata":{"sessionID":"ses_demo_explore","status":"completed","truncated":false}},"time":{"created":1793527205000,"ran":1793527205000,"completed":1793527230000}},{"type":"text","text":"The explorer found the theme in theme.ts; wiring a toggle there."}],"finish":"tool-calls","cost":0,"tokens":{"input":900,"output":120,"reasoning":30,"cache":{"read":0,"write":0}}}');
INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES ('msg_r03', 'ses_demo_root', 'idle', 3, 1793527241000, 1793527241000, '{"time":{"created":1793527241000},"outcome":"succeeded"}');
INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES ('msg_r04', 'ses_demo_root', 'assistant', 4, 1793527242000, 1793527280000, '{"time":{"created":1793527242000,"firstToken":1793527243000,"completed":1793527280000},"agent":"build","model":{"id":"demo-model","providerID":"demo"},"content":[{"type":"reasoning","text":"Patch the theme module, add the toggle, then run the tests.","time":{"created":1793527243000}},{"type":"tool","id":"call_patch1","name":"patch","executed":true,"state":{"status":"completed","input":{"patchText":"*** Begin Patch\n*** Update File: /home/dev/shop/src/theme.ts\n@@\n-export const mode = ''light''\n+export const mode = readMode()\n*** Add File: /home/dev/shop/src/toggle.tsx\n+export const Toggle = () => null\n*** End Patch"},"metadata":{"files":[{"path":"/home/dev/shop/src/theme.ts"}],"truncated":false}},"time":{"created":1793527245000,"ran":1793527245000,"completed":1793527250000}},{"type":"tool","id":"call_test1","name":"shell","executed":true,"state":{"status":"completed","input":{"command":"npm test"},"metadata":{"status":"completed","exit":1,"truncated":false}},"time":{"created":1793527252000,"ran":1793527252000,"completed":1793527270000}},{"type":"tool","id":"call_review","name":"subagent","executed":true,"state":{"status":"error","input":{"agent":"general","description":"Review the diff","prompt":"Review the change for regressions."},"metadata":{"sessionID":"ses_demo_review","status":"error"},"error":{"type":"tool.execution","message":"reviewer stopped before finishing"}},"time":{"created":1793527272000,"ran":1793527272000,"completed":1793527278000}},{"type":"text","text":"Tests fail on the toggle; the reviewer stopped early."}],"finish":"stop","cost":0,"tokens":{"input":900,"output":90,"reasoning":10,"cache":{"read":0,"write":0}}}');
INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES ('msg_r05', 'ses_demo_root', 'synthetic', 5, 1793527281000, 1793527281000, '{"time":{"created":1793527281000},"text":"subagent finished","metadata":{"source":"subagent","childID":"ses_demo_review"}}');
INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES ('msg_r06', 'ses_demo_root', 'assistant', 6, 1793527285000, 1793527286000, '{"time":{"created":1793527285000,"firstToken":1793527286000},"agent":"build","model":{"id":"demo-model","providerID":"demo"},"content":[{"type":"tool","id":"call_resume","name":"subagent","executed":true,"state":{"status":"running","input":{"sessionID":"ses_demo_explore","description":"Check the dark palette contrast","prompt":"Look at the contrast of the dark palette."},"metadata":{"sessionID":"ses_demo_explore","status":"running"}},"time":{"created":1793527286000,"ran":1793527286000}}]}');
INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES ('msg_e01', 'ses_demo_explore', 'user', 1, 1793527205000, 1793527205000, '{"time":{"created":1793527205000},"text":"Find where theme state lives and how it is applied.","files":[],"agents":[]}');
INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES ('msg_e02', 'ses_demo_explore', 'assistant', 2, 1793527206000, 1793527228000, '{"time":{"created":1793527206000,"firstToken":1793527207000,"completed":1793527228000},"agent":"build","model":{"id":"demo-model","providerID":"demo"},"content":[{"type":"reasoning","text":"Search for the theme, then read the module that holds it.","time":{"created":1793527207000}},{"type":"tool","id":"call_g1","name":"grep","executed":true,"state":{"status":"completed","input":{"pattern":"theme","path":"/home/dev/shop"},"metadata":{"matches":4,"truncated":false}},"time":{"created":1793527208000,"ran":1793527208000,"completed":1793527212000}},{"type":"tool","id":"call_e_read1","name":"read","executed":true,"state":{"status":"completed","input":{"path":"/home/dev/shop/src/theme.ts"},"metadata":{"truncated":false}},"time":{"created":1793527214000,"ran":1793527214000,"completed":1793527215000}},{"type":"text","text":"Theme state lives in src/theme.ts and is applied by the root layout."}],"finish":"stop","cost":0,"tokens":{"input":900,"output":60,"reasoning":0,"cache":{"read":0,"write":0}}}');
INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES ('msg_e03', 'ses_demo_explore', 'idle', 3, 1793527229000, 1793527229000, '{"time":{"created":1793527229000},"outcome":"succeeded"}');
INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES ('msg_e04', 'ses_demo_explore', 'user', 4, 1793527287000, 1793527287000, '{"time":{"created":1793527287000},"text":"Look at the contrast of the dark palette.","files":[],"agents":[]}');
INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES ('msg_e05', 'ses_demo_explore', 'assistant', 5, 1793527288000, 1793527295000, '{"time":{"created":1793527288000,"firstToken":1793527289000,"completed":1793527295000},"agent":"build","model":{"id":"demo-model","providerID":"demo"},"content":[{"type":"tool","id":"call_e_read2","name":"read","executed":true,"state":{"status":"completed","input":{"path":"/home/dev/shop/src/palette.ts"},"metadata":{"truncated":false}},"time":{"created":1793527290000,"ran":1793527290000,"completed":1793527291000}},{"type":"text","text":"The dark palette passes contrast for body text."}],"finish":"stop","cost":0,"tokens":{"input":900,"output":40,"reasoning":0,"cache":{"read":0,"write":0}}}');
INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES ('msg_e06', 'ses_demo_explore', 'idle', 6, 1793527296000, 1793527296000, '{"time":{"created":1793527296000},"outcome":"succeeded"}');
INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES ('msg_v01', 'ses_demo_review', 'user', 1, 1793527273000, 1793527273000, '{"time":{"created":1793527273000},"text":"Review the change for regressions.","files":[],"agents":[]}');
INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES ('msg_v02', 'ses_demo_review', 'assistant', 2, 1793527274000, 1793527277000, '{"time":{"created":1793527274000,"firstToken":1793527275000,"completed":1793527277000},"agent":"build","model":{"id":"demo-model","providerID":"demo"},"content":[{"type":"tool","id":"call_v_read1","name":"read","executed":true,"state":{"status":"error","input":{"path":"/home/dev/shop/src/missing.ts"},"error":{"type":"tool.execution","message":"file not found"}},"time":{"created":1793527275000,"ran":1793527275000,"completed":1793527276000}}],"finish":"error","cost":0,"tokens":{"input":900,"output":8,"reasoning":0,"cache":{"read":0,"write":0}}}');
INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES ('msg_v03', 'ses_demo_review', 'idle', 3, 1793527278000, 1793527278000, '{"time":{"created":1793527278000},"outcome":"failed","error":{"type":"unknown","message":"stopped"}}');
