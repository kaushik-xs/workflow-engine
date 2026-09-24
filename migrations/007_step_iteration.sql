-- Loop nodes run the nodes inside them once per item, so the same node can have one step
-- per iteration. `iteration` identifies which: the loop indices from the outermost loop
-- inwards, dot-joined ("0", "2.1"). Steps outside any loop keep the empty string.
ALTER TABLE workflow_steps ADD COLUMN iteration TEXT NOT NULL DEFAULT '';
ALTER TABLE workflow_steps DROP CONSTRAINT workflow_steps_execution_id_node_id_key;
ALTER TABLE workflow_steps
    ADD CONSTRAINT workflow_steps_execution_node_iteration_key
    UNIQUE (execution_id, node_id, iteration);
