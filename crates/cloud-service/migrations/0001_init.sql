-- LOT-017: managed Cloud Service schema. Every table that holds tenant data carries tenant_id
-- and every query in src/db.rs filters by it explicitly (no "current tenant" session global) —
-- see src/tenant_mw.rs and tests/lot017_tenant_isolation.rs.

CREATE TABLE tenants (
    id UUID PRIMARY KEY,
    name TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE users (
    id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL REFERENCES tenants (id),
    -- OIDC `sub` claim of the configured issuer, unique per issuer.
    oidc_subject TEXT NOT NULL,
    oidc_issuer TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (oidc_issuer, oidc_subject)
);

CREATE TABLE sessions (
    id UUID PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES users (id),
    tenant_id UUID NOT NULL REFERENCES tenants (id),
    -- Opaque bearer token handed to the client; only its BLAKE3 hash is stored.
    token_blake3 TEXT NOT NULL UNIQUE,
    expires_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX sessions_tenant_id_idx ON sessions (tenant_id);

-- One PKCE authorization-code flow in flight. Rows older than a few minutes are garbage; nothing
-- here is sensitive beyond the verifier (not a credential, useless without the matching code).
CREATE TABLE oidc_flows (
    state TEXT PRIMARY KEY,
    pkce_verifier TEXT NOT NULL,
    nonce TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE tasks (
    id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL REFERENCES tenants (id),
    created_by UUID NOT NULL REFERENCES users (id),
    -- Monotonic per-task counter for SSE cursor replay (`events.rs`), independent of task_events'
    -- own primary key so replaying never depends on global row ordering.
    next_event_cursor BIGINT NOT NULL DEFAULT 1,
    status TEXT NOT NULL DEFAULT 'created',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX tasks_tenant_id_idx ON tasks (tenant_id);

-- Append-only. `cursor` is per-task and monotonic (assigned from tasks.next_event_cursor), never
-- reused, so a client reconnecting with `Last-Event-ID: <cursor>` always gets an unambiguous
-- "everything after this" replay (tests/lot017_sse_cursor.rs).
CREATE TABLE task_events (
    task_id UUID NOT NULL REFERENCES tasks (id),
    tenant_id UUID NOT NULL REFERENCES tasks (id),
    cursor BIGINT NOT NULL,
    event_type TEXT NOT NULL,
    payload JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (task_id, cursor)
);
CREATE INDEX task_events_tenant_id_idx ON task_events (tenant_id);

-- One row per promoted (or superseded/rolled-back) digest. `status = 'active'` on at most one row
-- per tenant at a time; publish/rollback both go through src/publish_service.rs, which enforces
-- that invariant inside a transaction.
CREATE TABLE publish_deployments (
    id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL REFERENCES tenants (id),
    task_id UUID NOT NULL REFERENCES tasks (id),
    digest_blake3 TEXT NOT NULL,
    object_key TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('active', 'superseded', 'rolled_back')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, digest_blake3)
);
CREATE INDEX publish_deployments_tenant_active_idx
    ON publish_deployments (tenant_id)
    WHERE status = 'active';

-- Backs `sandbox_runner::reconcile::RunDeadlineStore` (src/fargate_runs.rs): the Fargate
-- reconciliation watchdog's documented mitigation for orphaned tasks (LOT-017 risk 4.8).
CREATE TABLE fargate_runs (
    ecs_task_arn TEXT PRIMARY KEY,
    tenant_id UUID NOT NULL REFERENCES tenants (id),
    task_id UUID NOT NULL REFERENCES tasks (id),
    stop_deadline TIMESTAMPTZ NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('running', 'stopped')) DEFAULT 'running',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX fargate_runs_overdue_idx ON fargate_runs (stop_deadline) WHERE status = 'running';
