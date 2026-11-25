-- A synthetic OpenCode v2 database written with `experimental.part_rows` on,
-- in the shape OpenCode 2.3.2's own projector writes (its source:
-- packages/core/src/session/projector.ts and sql.ts): an assistant message's
-- parts are rows of `session_part`, whose `data` is the document an inline
-- `content` element would be, and the message's own `content` is `[]`. One
-- message of the session is still inline, because a database can hold both,
-- and one call is still `streaming`. Nothing here is a capture; the project,
-- the prompts and the paths are invented.
--
-- Beside demo.sql, which has no `session_part` table at all, as a database
-- written only by an older OpenCode may not.

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
CREATE TABLE `session_part` (
  `message_id` text NOT NULL,
  `session_id` text NOT NULL,
  `ordinal` integer NOT NULL,
  `type` text NOT NULL,
  `call_id` text,
  `time_created` integer NOT NULL,
  `time_updated` integer NOT NULL,
  `data` text NOT NULL,
  CONSTRAINT `session_part_pk` PRIMARY KEY(`message_id`, `ordinal`)
);
CREATE INDEX `session_part_session_message_idx` ON `session_part` (`session_id`,`message_id`,`ordinal`);

INSERT INTO session_v2 (id, project_id, slug, directory, title, version, agent, time_created, time_updated) VALUES ('ses_parts_root', 'prj_parts', 'parts', '/home/dev/parts', 'Tidy the config loader', '2.3.2', 'build', 1793610000000, 1793610051000);

INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES ('msg_p01', 'ses_parts_root', 'user', 1, 1793610000000, 1793610000000, '{"time":{"created":1793610000000},"text":"Tidy the config loader.","files":[],"agents":[]}');
INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES ('msg_p02', 'ses_parts_root', 'assistant', 2, 1793610001000, 1793610020000, '{"time":{"created":1793610001000,"firstToken":1793610002000,"completed":1793610020000},"agent":"build","model":{"id":"demo-model","providerID":"demo"},"content":[],"finish":"stop","cost":0,"tokens":{"input":500,"output":70,"reasoning":0,"cache":{"read":0,"write":0}}}');
INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES ('msg_p03', 'ses_parts_root', 'assistant', 3, 1793610030000, 1793610045000, '{"time":{"created":1793610030000,"firstToken":1793610031000,"completed":1793610045000},"agent":"build","model":{"id":"demo-model","providerID":"demo"},"content":[{"type":"tool","id":"call_grep","name":"grep","executed":true,"state":{"status":"completed","input":{"pattern":"parse","path":"/home/dev/parts"},"metadata":{}},"time":{"created":1793610032000,"ran":1793610032000,"completed":1793610036000}},{"type":"text","text":"Two call sites; both go through load()."}],"finish":"stop","cost":0,"tokens":{"input":500,"output":40,"reasoning":0,"cache":{"read":0,"write":0}}}');
INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES ('msg_p04', 'ses_parts_root', 'assistant', 4, 1793610050000, 1793610051000, '{"time":{"created":1793610050000,"firstToken":1793610051000},"agent":"build","model":{"id":"demo-model","providerID":"demo"},"content":[]}');

INSERT INTO session_part (message_id, session_id, ordinal, type, call_id, time_created, time_updated, data) VALUES ('msg_p02', 'ses_parts_root', 0, 'reasoning', NULL, 1793610002000, 1793610003000, '{"type":"reasoning","text":"Read the loader before changing it.","time":{"created":1793610002000,"completed":1793610003000}}');
INSERT INTO session_part (message_id, session_id, ordinal, type, call_id, time_created, time_updated, data) VALUES ('msg_p02', 'ses_parts_root', 1, 'tool', 'call_read', 1793610004000, 1793610006000, '{"type":"tool","id":"call_read","name":"read","executed":true,"state":{"status":"completed","input":{"path":"/home/dev/parts/src/config.rs"},"metadata":{}},"time":{"created":1793610004000,"ran":1793610004000,"completed":1793610006000}}');
INSERT INTO session_part (message_id, session_id, ordinal, type, call_id, time_created, time_updated, data) VALUES ('msg_p02', 'ses_parts_root', 2, 'text', NULL, 1793610007000, 1793610007000, '{"type":"text","text":"The loader parses twice; I will fold it into one pass."}');
INSERT INTO session_part (message_id, session_id, ordinal, type, call_id, time_created, time_updated, data) VALUES ('msg_p04', 'ses_parts_root', 0, 'tool', 'call_patch', 1793610051000, 1793610051000, '{"type":"tool","id":"call_patch","name":"patch","executed":true,"state":{"status":"streaming","input":"{\"patchText\":\"*** Begin Patch\\n*** Upd"},"time":{"created":1793610051000}}');
