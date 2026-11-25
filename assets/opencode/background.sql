-- A synthetic OpenCode database in the shape 2.3.x writes when subagents run
-- in the background, which demo.sql (a call that waits for its child) does not
-- have: each `subagent` call returns at once with `metadata.status: "running"`
-- and the child's end arrives later as a `synthetic` report in the parent.
-- Four children: one reports `completed` after an interim report whose header
-- still says `running`, one `error`, one `cancelled` (a state taken from
-- OpenCode's source; no database we hold has it), and one is resumed after
-- reporting and has not reported again. Nothing here is a capture.

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

INSERT INTO session_v2 (id, project_id, parent_id, slug, directory, title, version, agent, time_created, time_updated) VALUES ('ses_bg_root', 'prj_bg', NULL, 'root', '/home/dev/jobs', 'Audit the job queue', '2.3.2', 'general', 1793714400000, 1793714431000);
INSERT INTO session_v2 (id, project_id, parent_id, slug, directory, title, version, agent, time_created, time_updated) VALUES ('ses_bg_audit', 'prj_bg', 'ses_bg_root', 'audit', '/home/dev/jobs', 'Audit retries', '2.3.2', 'general', 1793714402000, 1793714425000);
INSERT INTO session_v2 (id, project_id, parent_id, slug, directory, title, version, agent, time_created, time_updated) VALUES ('ses_bg_build', 'prj_bg', 'ses_bg_root', 'build', '/home/dev/jobs', 'Rebuild the index', '2.3.2', 'general', 1793714402000, 1793714416000);
INSERT INTO session_v2 (id, project_id, parent_id, slug, directory, title, version, agent, time_created, time_updated) VALUES ('ses_bg_cancel', 'prj_bg', 'ses_bg_root', 'cancel', '/home/dev/jobs', 'Trace slow jobs', '2.3.2', 'general', 1793714402000, 1793714418000);
INSERT INTO session_v2 (id, project_id, parent_id, slug, directory, title, version, agent, time_created, time_updated) VALUES ('ses_bg_epilogue', 'prj_bg', 'ses_bg_root', 'epilogue', '/home/dev/jobs', 'Summarise findings', '2.3.2', 'general', 1793714402000, 1793714440000);

INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES ('msg_b01', 'ses_bg_root', 'user', 1, 1793714400000, 1793714400000, '{"time":{"created":1793714400000},"text":"Audit the job queue."}');
INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES ('msg_b02', 'ses_bg_root', 'assistant', 2, 1793714401000, 1793714406000, '{"time":{"created":1793714401000,"completed":1793714406000},"model":{"id":"demo-model","providerID":"demo"},"content":[{"type":"tool","id":"call_a","name":"subagent","state":{"status":"completed","input":{"agent":"general","description":"Audit retries","prompt":"Do it."},"metadata":{"sessionID":"ses_bg_audit","status":"running"}},"time":{"created":1793714402000,"ran":1793714402000,"completed":1793714402020}},{"type":"tool","id":"call_b","name":"subagent","state":{"status":"completed","input":{"agent":"general","description":"Rebuild the index","prompt":"Do it."},"metadata":{"sessionID":"ses_bg_build","status":"running"}},"time":{"created":1793714403000,"ran":1793714403000,"completed":1793714403020}},{"type":"tool","id":"call_c","name":"subagent","state":{"status":"completed","input":{"agent":"general","description":"Trace slow jobs","prompt":"Do it."},"metadata":{"sessionID":"ses_bg_cancel","status":"running"}},"time":{"created":1793714404000,"ran":1793714404000,"completed":1793714404019}},{"type":"tool","id":"call_e","name":"subagent","state":{"status":"completed","input":{"agent":"general","description":"Summarise findings","prompt":"Do it."},"metadata":{"sessionID":"ses_bg_epilogue","status":"running"}},"time":{"created":1793714405000,"ran":1793714405000,"completed":1793714405020}}],"tokens":{"output":10,"reasoning":0},"finish":"stop"}');
INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES ('msg_ses_bg_audit_1', 'ses_bg_audit', 'user', 1, 1793714402100, 1793714402100, '{"time":{"created":1793714402100},"text":"Do it."}');
INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES ('msg_ses_bg_build_1', 'ses_bg_build', 'user', 1, 1793714403100, 1793714403100, '{"time":{"created":1793714403100},"text":"Do it."}');
INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES ('msg_ses_bg_cancel_1', 'ses_bg_cancel', 'user', 1, 1793714404100, 1793714404100, '{"time":{"created":1793714404100},"text":"Do it."}');
INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES ('msg_ses_bg_epilogue_1', 'ses_bg_epilogue', 'user', 1, 1793714405100, 1793714405100, '{"time":{"created":1793714405100},"text":"Do it."}');
INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES ('msg_ses_bg_audit_2', 'ses_bg_audit', 'assistant', 2, 1793714408000, 1793714412000, '{"time":{"created":1793714408000,"completed":1793714412000},"model":{"id":"demo-model","providerID":"demo"},"content":[{"type":"tool","id":"call_a_r1","name":"read","state":{"status":"completed","input":{"path":"/home/dev/jobs/src/queue.rs"},"metadata":{}},"time":{"created":1793714409000,"ran":1793714409000,"completed":1793714410000}}],"tokens":{"output":10,"reasoning":0},"finish":"stop"}');
INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES ('msg_b03', 'ses_bg_root', 'synthetic', 3, 1793714413000, 1793714413000, '{"time":{"created":1793714413000},"text":"<subagent sessionID=\"ses_bg_audit\" state=\"running\" description=\"job\">\nStill waiting on a scan.\n</subagent>","metadata":{"source":"subagent","childID":"ses_bg_audit","agent":"general","state":"completed"}}');
INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES ('msg_ses_bg_audit_3', 'ses_bg_audit', 'assistant', 3, 1793714420000, 1793714424000, '{"time":{"created":1793714420000,"completed":1793714424000},"model":{"id":"demo-model","providerID":"demo"},"content":[{"type":"tool","id":"call_a_r2","name":"read","state":{"status":"completed","input":{"path":"/home/dev/jobs/src/queue.rs"},"metadata":{}},"time":{"created":1793714421000,"ran":1793714421000,"completed":1793714422000}}],"tokens":{"output":10,"reasoning":0},"finish":"stop"}');
INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES ('msg_b04', 'ses_bg_root', 'synthetic', 4, 1793714425000, 1793714425000, '{"time":{"created":1793714425000},"text":"<subagent sessionID=\"ses_bg_audit\" state=\"completed\" description=\"job\">\nDone.\n</subagent>","metadata":{"source":"subagent","childID":"ses_bg_audit","agent":"general","state":"completed"}}');
INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES ('msg_ses_bg_build_2', 'ses_bg_build', 'assistant', 2, 1793714408000, 1793714415000, '{"time":{"created":1793714408000,"completed":1793714415000},"model":{"id":"demo-model","providerID":"demo"},"content":[{"type":"tool","id":"call_b_r1","name":"read","state":{"status":"completed","input":{"path":"/home/dev/jobs/src/queue.rs"},"metadata":{}},"time":{"created":1793714409000,"ran":1793714409000,"completed":1793714410000}}],"tokens":{"output":10,"reasoning":0},"finish":"stop"}');
INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES ('msg_b05', 'ses_bg_root', 'synthetic', 5, 1793714416000, 1793714416000, '{"time":{"created":1793714416000},"text":"<subagent sessionID=\"ses_bg_build\" state=\"error\" description=\"job\">\nSubagent failed\n</subagent>","metadata":{"source":"subagent","childID":"ses_bg_build","agent":"general","state":"error"}}');
INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES ('msg_ses_bg_cancel_2', 'ses_bg_cancel', 'assistant', 2, 1793714408000, 1793714417000, '{"time":{"created":1793714408000,"completed":1793714417000},"model":{"id":"demo-model","providerID":"demo"},"content":[{"type":"tool","id":"call_c_r1","name":"read","state":{"status":"completed","input":{"path":"/home/dev/jobs/src/queue.rs"},"metadata":{}},"time":{"created":1793714409000,"ran":1793714409000,"completed":1793714410000}}],"tokens":{"output":10,"reasoning":0},"finish":"stop"}');
INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES ('msg_b06', 'ses_bg_root', 'synthetic', 6, 1793714418000, 1793714418000, '{"time":{"created":1793714418000},"text":"<subagent sessionID=\"ses_bg_cancel\" state=\"cancelled\" description=\"job\">\nSubagent cancelled\n</subagent>","metadata":{"source":"subagent","childID":"ses_bg_cancel","agent":"general","state":"cancelled"}}');
INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES ('msg_ses_bg_epilogue_2', 'ses_bg_epilogue', 'assistant', 2, 1793714408000, 1793714419000, '{"time":{"created":1793714408000,"completed":1793714419000},"model":{"id":"demo-model","providerID":"demo"},"content":[{"type":"tool","id":"call_e_r1","name":"read","state":{"status":"completed","input":{"path":"/home/dev/jobs/src/queue.rs"},"metadata":{}},"time":{"created":1793714409000,"ran":1793714409000,"completed":1793714410000}}],"tokens":{"output":10,"reasoning":0},"finish":"stop"}');
INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES ('msg_b07', 'ses_bg_root', 'synthetic', 7, 1793714420000, 1793714420000, '{"time":{"created":1793714420000},"text":"<subagent sessionID=\"ses_bg_epilogue\" state=\"completed\" description=\"job\">\nDone.\n</subagent>","metadata":{"source":"subagent","childID":"ses_bg_epilogue","agent":"general","state":"completed"}}');
INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES ('msg_b08', 'ses_bg_root', 'assistant', 8, 1793714430000, 1793714431000, '{"time":{"created":1793714430000,"completed":1793714431000},"model":{"id":"demo-model","providerID":"demo"},"content":[{"type":"tool","id":"call_e2","name":"subagent","state":{"status":"completed","input":{"agent":"general","description":"Add the open questions","prompt":"Do it.","sessionID":"ses_bg_epilogue"},"metadata":{"sessionID":"ses_bg_epilogue","status":"running"}},"time":{"created":1793714430000,"ran":1793714430000,"completed":1793714430020}}],"tokens":{"output":10,"reasoning":0},"finish":"stop"}');
INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES ('msg_ses_bg_epilogue_3', 'ses_bg_epilogue', 'user', 3, 1793714430100, 1793714430100, '{"time":{"created":1793714430100},"text":"Add the open questions."}');
INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES ('msg_ses_bg_epilogue_4', 'ses_bg_epilogue', 'assistant', 4, 1793714432000, 1793714440000, '{"time":{"created":1793714432000,"completed":1793714440000},"model":{"id":"demo-model","providerID":"demo"},"content":[{"type":"tool","id":"call_e_r2","name":"read","state":{"status":"completed","input":{"path":"/home/dev/jobs/src/queue.rs"},"metadata":{}},"time":{"created":1793714433000,"ran":1793714433000,"completed":1793714434000}}],"tokens":{"output":10,"reasoning":0},"finish":"stop"}');
