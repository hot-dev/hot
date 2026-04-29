-- Hot 2.0 public SQLite schema baseline.

PRAGMA foreign_keys = ON;

CREATE TABLE access (
    access_id      text primary key,
    env_id         text not null,
    api_key_id     text,
    service_key_id text,
    session_id     text,
    source         text not null default 'api',
    ip_address     text,
    user_agent     text,
    host           text,
    method         text,
    path           text,
    query_params   text,
    created_at     text default (datetime('now'))
);

CREATE TABLE agent (
    agent_id blob primary key,
    build_id blob not null references build(build_id) on update restrict on delete restrict,
    env_id blob not null references env(env_id) on update restrict on delete restrict,

    type_name text not null,
    namespace text not null,

    name text,
    description text,
    tags text,              -- JSON array of strings
    config_fields text,     -- JSON object

    meta text,              -- JSON
    file text,
    line integer,
    "column" integer,
    position integer,

    created_at datetime default current_timestamp,

    unique (build_id, namespace, type_name)
);

CREATE TABLE workflow (
    workflow_id blob primary key,
    build_id blob not null references build(build_id) on update restrict on delete restrict,
    env_id blob not null references env(env_id) on update restrict on delete restrict,

    type_name text not null,
    namespace text not null,

    name text,
    description text,
    tags text,              -- JSON array of strings

    meta text,              -- JSON
    file text,
    line integer,
    "column" integer,
    position integer,

    created_at datetime default current_timestamp,

    unique (build_id, namespace, type_name)
);

CREATE TABLE alert (
    alert_id BLOB PRIMARY KEY,
    org_id BLOB NOT NULL REFERENCES org(org_id) ON UPDATE RESTRICT ON DELETE CASCADE,
    env_id BLOB NOT NULL REFERENCES env(env_id) ON UPDATE RESTRICT ON DELETE CASCADE,
    channel TEXT NOT NULL,
    data TEXT NOT NULL, -- JSON: Alert data (channel-specific details)
    created_at TEXT DEFAULT (datetime('now'))
);

CREATE TABLE alert_channel (
    alert_channel_id BLOB PRIMARY KEY,
    org_id BLOB REFERENCES org(org_id) ON UPDATE RESTRICT ON DELETE CASCADE, -- NULL = system-wide
    env_id BLOB REFERENCES env(env_id) ON UPDATE RESTRICT ON DELETE CASCADE, -- NULL = org-wide or system
    name TEXT NOT NULL,
    pattern TEXT NOT NULL, -- Regex pattern
    enabled INTEGER NOT NULL DEFAULT 1,
    created_at TEXT DEFAULT (datetime('now')),
    updated_at TEXT DEFAULT (datetime('now')),
    created_by_user_id BLOB REFERENCES user(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT,
    updated_by_user_id BLOB REFERENCES user(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT,
    CHECK (
        (org_id IS NULL AND env_id IS NULL) OR
        (org_id IS NOT NULL)
    )
);

CREATE TABLE alert_delivery (
    alert_delivery_id BLOB PRIMARY KEY,
    alert_id BLOB NOT NULL REFERENCES alert(alert_id) ON UPDATE RESTRICT ON DELETE CASCADE,
    subscription_id BLOB NOT NULL REFERENCES alert_subscription(alert_subscription_id) ON UPDATE RESTRICT ON DELETE CASCADE,
    destination_id BLOB NOT NULL REFERENCES alert_destination(alert_destination_id) ON UPDATE RESTRICT ON DELETE CASCADE,
    resolved_user_id BLOB REFERENCES user(user_id) ON UPDATE RESTRICT ON DELETE SET NULL, -- For org/team/user email destinations
    status_id INTEGER NOT NULL DEFAULT 1 REFERENCES alert_delivery_status(status_id) ON UPDATE RESTRICT ON DELETE RESTRICT,
    attempts INTEGER NOT NULL DEFAULT 0,
    max_attempts INTEGER NOT NULL DEFAULT 5,
    next_retry_at TEXT, -- When to retry
    last_error TEXT,
    sent_at TEXT,
    created_at TEXT DEFAULT (datetime('now'))
);

CREATE TABLE alert_delivery_status (
    status_id INTEGER PRIMARY KEY,
    status_name TEXT NOT NULL UNIQUE,
    sort_order INTEGER NOT NULL
);

CREATE TABLE alert_destination (
    alert_destination_id BLOB PRIMARY KEY,
    org_id BLOB NOT NULL REFERENCES org(org_id) ON UPDATE RESTRICT ON DELETE CASCADE,
    name TEXT NOT NULL,
    destination_type_id INTEGER NOT NULL REFERENCES alert_destination_type(type_id) ON UPDATE RESTRICT ON DELETE RESTRICT,
    config TEXT NOT NULL, -- JSON: Type-specific config
    enabled INTEGER NOT NULL DEFAULT 1,
    created_at TEXT DEFAULT (datetime('now')),
    updated_at TEXT DEFAULT (datetime('now')),
    created_by_user_id BLOB NOT NULL REFERENCES user(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT,
    updated_by_user_id BLOB REFERENCES user(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT, verified INTEGER NOT NULL DEFAULT 1, verification_token TEXT, verification_expires_at TEXT, verification_attempts INTEGER NOT NULL DEFAULT 0,
    UNIQUE (org_id, name)
);

CREATE TABLE alert_destination_type (
    type_id INTEGER PRIMARY KEY,
    type_name TEXT NOT NULL UNIQUE,
    sort_order INTEGER NOT NULL
);

CREATE TABLE alert_subscription (
    alert_subscription_id BLOB PRIMARY KEY,
    org_id BLOB NOT NULL REFERENCES org(org_id) ON UPDATE RESTRICT ON DELETE CASCADE,
    env_id BLOB REFERENCES env(env_id) ON UPDATE RESTRICT ON DELETE CASCADE,
    team_id BLOB REFERENCES team(team_id) ON UPDATE RESTRICT ON DELETE CASCADE,
    user_id BLOB REFERENCES user(user_id) ON UPDATE RESTRICT ON DELETE CASCADE,
    enabled INTEGER NOT NULL DEFAULT 1,
    created_at TEXT DEFAULT (datetime('now')),
    updated_at TEXT DEFAULT (datetime('now')),
    created_by_user_id BLOB NOT NULL REFERENCES user(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT,
    updated_by_user_id BLOB REFERENCES user(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT,
    CHECK (NOT (team_id IS NOT NULL AND user_id IS NOT NULL))
);

CREATE TABLE alert_subscription_channel (
    subscription_id BLOB NOT NULL REFERENCES alert_subscription(alert_subscription_id) ON UPDATE RESTRICT ON DELETE CASCADE,
    channel_id BLOB NOT NULL REFERENCES alert_channel(alert_channel_id) ON UPDATE RESTRICT ON DELETE RESTRICT,
    enabled INTEGER NOT NULL DEFAULT 1,
    created_at TEXT DEFAULT (datetime('now')),
    PRIMARY KEY (subscription_id, channel_id)
);

CREATE TABLE alert_subscription_destination (
    subscription_id BLOB NOT NULL REFERENCES alert_subscription(alert_subscription_id) ON UPDATE RESTRICT ON DELETE CASCADE,
    destination_id BLOB NOT NULL REFERENCES alert_destination(alert_destination_id) ON UPDATE RESTRICT ON DELETE CASCADE,
    enabled INTEGER NOT NULL DEFAULT 1,
    created_at TEXT DEFAULT (datetime('now')),
    PRIMARY KEY (subscription_id, destination_id)
);

CREATE TABLE api_key (
    api_key_id blob primary key,
    env_id blob not null references env(env_id) on update restrict on delete restrict,
    description text not null,
    key_data text not null, -- json of api key hash and salt
    active integer default 1,
    created_by_user_id blob not null references user(user_id) on update restrict on delete restrict,
    created_at datetime default current_timestamp,
    updated_at datetime default current_timestamp,
    updated_by_user_id blob references user(user_id) on update restrict on delete restrict,
    active_toggle_at datetime,
    active_toggle_by_user_id blob references user(user_id) on update restrict on delete restrict
, permissions text DEFAULT '{}');

CREATE TABLE build (
    build_id blob primary key,
    project_id blob not null references project(project_id) on update restrict on delete restrict,
    hash text not null,
    size integer not null,
    build_type_id integer not null default 1 references build_type(build_type_id) on update restrict on delete restrict,
    deployed integer default 0,
    active integer default 1,
    created_by_user_id blob not null references user(user_id) on update restrict on delete restrict,
    created_at datetime default current_timestamp,
    updated_at datetime default current_timestamp,
    updated_by_user_id blob references user(user_id) on update restrict on delete restrict,
    active_toggle_at datetime,
    active_toggle_by_user_id blob references user(user_id) on update restrict on delete restrict,
    storage_path text,
    storage_backend text
);

CREATE TABLE build_type (
    build_type_id integer primary key,
    build_type text not null unique,
    sort_order integer not null
);

CREATE TABLE call (
    call_id blob not null primary key,
    run_id blob not null references run(run_id) on update restrict on delete cascade,
    parent_call_id blob,  -- No FK: avoids self-reference overhead and partition boundary issues (see DATABASE_PARTITIONING_ARCHIVING.md)
    function_name text not null,
    static_scope text not null,  -- For AST metadata lookup
    runtime_path text not null,  -- Unique execution instance
    call_depth integer not null,  -- 0 = top-level, 1 = first nested, etc.
    args text,  -- JSON array of function arguments
    return_value text,  -- JSON return value
    error text,  -- Error message if call failed
    flow text,  -- JSON: {type, modifier, flow_id, parent_flow_id, branch_index, pipe_position}
    start_time datetime not null,
    stop_time datetime,
    duration_us integer,  -- Duration in microseconds
    file text,
    line integer,
    "column" integer,
    position integer
, size INTEGER NOT NULL DEFAULT 0);

CREATE TABLE context (
    context_id blob primary key,
    env_id blob references env(env_id) on update restrict on delete cascade,
    project_id blob references project(project_id) on update restrict on delete cascade,
    key text not null,
    value text not null,  -- Always encrypted: base64(nonce || ciphertext || tag)
    description text,
    active integer default 1,
    created_at datetime default current_timestamp,
    created_by_user_id blob not null references user(user_id) on update restrict on delete restrict,
    updated_at datetime default current_timestamp,
    updated_by_user_id blob references user(user_id) on update restrict on delete restrict,
    -- Exactly one of env_id or project_id must be set (XOR constraint)
    check ((env_id is not null and project_id is null) or (env_id is null and project_id is not null))
);

CREATE TABLE domain (
    domain_id                  text primary key,
    env_id                     text not null references env(env_id) on update restrict on delete cascade,
    domain                     text not null unique,
    verified_at                text,
    tls_provisioned_at         text,
    certificate_ref        text,
    validation_cname_name  text,
    validation_cname_value text,
    routing_ref         text,
    routing_domain     text,
    created_at                 text default (datetime('now')),
    deleted_at                 text
, provisioning_error text);

CREATE TABLE email_queue (
    email_queue_id BLOB PRIMARY KEY,
    to_address TEXT NOT NULL,
    subject TEXT NOT NULL,
    html_body TEXT,
    text_body TEXT,
    from_address TEXT NOT NULL,
    status_id INTEGER NOT NULL DEFAULT 1,  -- 1=pending, 2=sent, 3=failed
    error_message TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    sent_at TEXT,
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE email_verification (
    verification_id blob primary key,
    email text not null,
    name text,
    password_hash text not null, -- Stores hashed password until verification
    verification_token text unique not null,
    status_id integer not null default 1 references email_verification_status(status_id) on update restrict on delete restrict,
    invite_code text, -- Optional invite code to process after verification
    -- Organization info for paid signups
    org_name text, -- Organization name (for paid signups)
    org_slug text, -- Organization slug (for paid signups)
    plan text, -- Plan ID (e.g., 'hot-cloud-starter')
    billing text, -- Billing period ('monthly' or 'annual')
    created_at datetime default current_timestamp,
    expires_at datetime not null,
    verified_at datetime,
    attempts integer default 0 -- Track resend attempts
, account_type TEXT DEFAULT 'individual');

CREATE TABLE email_verification_status (
    status_id integer primary key,
    status text not null unique,
    sort_order integer not null
);

CREATE TABLE env (
    env_id blob primary key,
    org_id blob not null references org(org_id) on update restrict on delete restrict,
    name text not null,
    active integer default 1,
    created_by_user_id blob not null references user(user_id) on update restrict on delete restrict,
    created_at datetime default current_timestamp,
    updated_at datetime default current_timestamp,
    updated_by_user_id blob references user(user_id) on update restrict on delete restrict,
    active_toggle_at datetime,
    active_toggle_by_user_id blob references user(user_id) on update restrict on delete restrict
);

CREATE TABLE event (
    event_id blob primary key,
    env_id blob not null references env(env_id) on update restrict on delete restrict,
    stream_id blob not null references stream(stream_id) on update restrict on delete restrict, -- execution stream identifier - groups related events/runs
    event_type text not null,
    event_data text not null, -- json
    event_time datetime not null,
    created_at datetime default current_timestamp,
    created_by_user_id blob not null references user(user_id) on update restrict on delete restrict,
    handled integer not null default 0 check (handled in (0, 1)) -- 0 = unhandled (no runs), 1 = handled (has runs)
, access_id text);

CREATE TABLE event_handler (
    event_handler_id blob primary key,
    build_id blob not null references build(build_id) on update restrict on delete restrict,
    event_type text not null,
    ns text not null,
    var text not null,
    meta text,
    value text,
    file text,
    line integer,
    "column" integer,
    position integer,
    -- Unique constraint: one handler per (build, ns, var, event_type)
    unique (build_id, ns, var, event_type)
);

CREATE TABLE file (
    file_id blob primary key,

    -- File identification
    path text not null,
    size integer not null,
    etag text,
    content_type text,

    -- Storage backend information
    storage_backend text not null,
    storage_path text,

    -- Ownership and environment isolation
    org_id blob not null references org(org_id) on update restrict on delete restrict,
    env_id blob references env(env_id) on update restrict on delete restrict,

    -- Run tracking (full execution context)
    created_by_run_id blob references run(run_id) on update restrict on delete restrict,
    updated_by_run_id blob references run(run_id) on update restrict on delete restrict,

    -- Standard audit fields
    active integer default 1,
    created_at datetime default current_timestamp,
    created_by_user_id blob not null references user(user_id) on update restrict on delete restrict,
    updated_at datetime default current_timestamp,
    updated_by_user_id blob references user(user_id) on update restrict on delete restrict,
    active_toggle_at datetime,
    active_toggle_by_user_id blob references user(user_id) on update restrict on delete restrict
);

CREATE TABLE file_upload (
    upload_id           blob PRIMARY KEY,
    path                text NOT NULL,
    org_id              blob NOT NULL REFERENCES org(org_id),
    env_id              blob REFERENCES env(env_id),
    created_by_user_id  blob NOT NULL,
    status              text NOT NULL DEFAULT 'pending',
    expected_size       integer,
    content_type        text,
    part_size           integer NOT NULL,
    parts_expected      integer,
    parts_received      integer NOT NULL DEFAULT 0,
    bytes_received      integer NOT NULL DEFAULT 0,
    backend_upload_id   text,
    parts_manifest      text NOT NULL DEFAULT '[]',
    storage_backend     text NOT NULL,
    created_at          datetime NOT NULL DEFAULT current_timestamp,
    expires_at          datetime NOT NULL
);

CREATE TABLE invite (
    invite_id blob primary key,
    invite_code text unique not null,
    email text not null,
    org_id blob not null references org(org_id) on update restrict on delete restrict,
    invite_status_id integer not null default 1 references invite_status(invite_status_id) on update restrict on delete restrict,
    intended_org_user_role_id integer not null default 1 references org_user_role(org_user_role_id) on update restrict on delete restrict,
    active integer default 1,
    created_at datetime default current_timestamp,
    created_by_user_id blob not null references user(user_id) on update restrict on delete restrict,
    updated_at datetime default current_timestamp,
    updated_by_user_id blob references user(user_id) on update restrict on delete restrict,
    expires_at datetime not null,
    used_at datetime
);

CREATE TABLE invite_status (
    invite_status_id integer primary key,
    status text not null unique,
    sort_order integer not null
);

CREATE TABLE mcp_tool (
    mcp_tool_id text primary key,
    build_id text not null references build(build_id) on update restrict on delete restrict,

    -- MCP service grouping (required)
    service text not null,

    -- Function location in Hot code
    ns text not null,
    var text not null,

    -- MCP tool metadata
    name text not null,              -- MCP tool name (auto-generated or override)
    description text,                -- Tool description
    input_schema text,               -- JSON Schema for input parameters (stored as JSON string)
    output_schema text,              -- JSON Schema for output (stored as JSON string)
    title text,                      -- Human-readable title
    icons text,                      -- MCP tool icons array (stored as JSON string)
    annotations text,                -- MCP annotations (stored as JSON string)

    -- Source location
    meta text,                       -- Original meta annotation (stored as JSON string)
    file text,
    line integer,
    "column" integer,
    position integer,

    created_at text default (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),

    -- Unique constraint: one tool per (build, service, name)
    unique (build_id, service, name)
);

CREATE TABLE org (
    org_id blob primary key,
    name text not null,
    slug text unique not null,
    is_personal integer default 0, -- 1 for auto-created personal orgs (cannot have paid billing)
    settings text default '{}',
    active integer default 1,
    created_at datetime default current_timestamp,
    created_by_user_id blob not null references user(user_id) on update restrict on delete restrict,
    updated_at datetime default current_timestamp,
    updated_by_user_id blob references user(user_id) on update restrict on delete restrict,
    active_toggle_at datetime,
    active_toggle_by_user_id blob references user(user_id) on update restrict on delete restrict
, features text, org_type TEXT NOT NULL DEFAULT 'organization' CHECK(org_type IN ('individual', 'organization')));

CREATE TABLE org_note (
    note_id         blob PRIMARY KEY,
    org_id          blob NOT NULL REFERENCES org(org_id),
    category        text NOT NULL,
    note_type       text NOT NULL,
    message         text NOT NULL,
    metadata        text,
    created_by      blob REFERENCES user(user_id),
    created_at      datetime NOT NULL DEFAULT current_timestamp
);

CREATE TABLE org_usage (
    usage_id blob primary key,
    org_id blob not null references org(org_id) on update restrict on delete restrict,
    usage_period_start datetime not null,
    usage_period_end datetime not null,
    runs_count integer default 0,
    team_members_count integer default 0,
    metrics text,
    created_at datetime default current_timestamp
);

CREATE TABLE org_user (
    org_user_id blob primary key,
    org_id blob not null references org(org_id) on update restrict on delete restrict,
    user_id blob not null references user(user_id) on update restrict on delete restrict,
    org_user_role_id integer not null default 1 references org_user_role(org_user_role_id) on update restrict on delete restrict,
    active integer default 1,
    created_at datetime default current_timestamp,
    created_by_user_id blob not null references user(user_id) on update restrict on delete restrict,
    updated_at datetime default current_timestamp,
    updated_by_user_id blob references user(user_id) on update restrict on delete restrict,
    active_toggle_at datetime,
    active_toggle_by_user_id blob references user(user_id) on update restrict on delete restrict,
    unique(org_id, user_id)
);

CREATE TABLE org_user_role (
    org_user_role_id integer primary key,
    role text not null unique,
    sort_order integer not null
);

CREATE TABLE project (
    project_id blob primary key,
    env_id blob not null references env(env_id) on update restrict on delete restrict,
    name text not null,
    active integer default 1,
    created_by_user_id blob not null references user(user_id) on update restrict on delete restrict,
    created_at datetime default current_timestamp,
    updated_at datetime default current_timestamp,
    updated_by_user_id blob references user(user_id) on update restrict on delete restrict,
    active_toggle_at datetime,
    active_toggle_by_user_id blob references user(user_id) on update restrict on delete restrict
);

CREATE TABLE run (
    run_id blob primary key,
    env_id blob not null references env(env_id) on update restrict on delete restrict,
    stream_id blob not null references stream(stream_id) on update restrict on delete restrict, -- execution stream identifier - groups related events/runs
    build_id blob not null references build(build_id) on update restrict on delete restrict,
    run_type_id integer not null references run_type(run_type_id) on update restrict on delete restrict,
    origin_run_id blob references run(run_id) on update restrict on delete restrict default null, -- null is the root run
    event_id blob references event(event_id) on update restrict on delete restrict default null, -- only set for event, call, and schedule runs
    start_time datetime default current_timestamp,
    stop_time datetime,
    status_id integer not null default 1 references run_status(status_id) on update restrict on delete restrict,
    by_user_id blob references user(user_id) on update restrict on delete restrict,
    result text, -- run result (JSON): Failure type for failed runs, return values for successful runs
    info text, -- execution info (JSON): warnings, routing decisions, diagnostics (null when empty)
    -- Retry fields for automatic retry of failed runs
    retry_attempt integer not null default 0, -- Current retry attempt (0 = first try, 1 = first retry, etc.)
    next_retry_at datetime -- When to retry next (null = no pending retry)
, access_id text, agent_type text);

CREATE TABLE run_status (
    status_id integer primary key,
    status text not null unique,
    sort_order integer not null
);

CREATE TABLE run_type (
    run_type_id integer primary key,
    run_type text not null unique,
    sort_order integer not null
);

CREATE TABLE schedule (
    schedule_id blob primary key,
    build_id blob not null references build(build_id) on update restrict on delete restrict,
    cron text not null,
    ns text not null,
    var text not null,
    meta text,
    value text,
    file text,
    line integer,
    "column" integer,
    position integer,
    active integer not null default 1,
    created_at text not null default (strftime('%Y-%m-%d %H:%M:%f', 'now')),
    deactivated_at text
);

CREATE TABLE schedule_log (
    log_id blob primary key,
    schedule_id blob not null references schedule(schedule_id) on delete cascade,
    event_id blob references event(event_id) on delete set null, -- the event created for this scheduled execution
    scheduled_time text not null, -- ISO 8601 timestamp: when it should have run
    executed_at text not null default (strftime('%Y-%m-%d %H:%M:%f', 'now')), -- when we actually queued it
    is_backfill integer not null default 0, -- 0=false, 1=true (SQLite uses integer for boolean)
    created_at text default (strftime('%Y-%m-%d %H:%M:%f', 'now'))
);

CREATE TABLE scheduler_state (
    scheduler_id text primary key default 'main',
    last_successful_sync_time text not null,
    updated_at text default (strftime('%Y-%m-%d %H:%M:%f', 'now'))
);

CREATE TABLE service_key (
    service_key_id text primary key,
    api_key_id     text not null references api_key(api_key_id) on update restrict on delete cascade,
    env_id         text not null references env(env_id) on update restrict on delete cascade,
    name           text,
    description    text,
    secret_hash    blob not null,
    permissions    text not null,
    metadata       text,
    expires_at     text,
    revoked_at     text,
    created_at     text default (datetime('now')),
    last_used_at   text
);

CREATE TABLE session (
    session_id    text primary key,
    api_key_id    text not null references api_key(api_key_id) on update restrict on delete cascade,
    env_id        text not null references env(env_id) on update restrict on delete cascade,
    secret_hash   blob not null,
    permissions   text not null,
    metadata      text,
    expires_at    text not null,
    revoked_at    text,
    created_at    text default (datetime('now')),
    last_used_at  text
);

CREATE TABLE stream (
    stream_id blob primary key,
    env_id blob not null references env(env_id) on update restrict on delete restrict,

    -- Stream metadata
    name text, -- Optional human-readable name
    description text, -- Optional description
    tags text, -- Optional tags for categorization (JSON)

    -- Lifecycle tracking
    created_at datetime default current_timestamp,
    started_at datetime, -- When first run started
    last_activity_at datetime default current_timestamp, -- Most recent run/event

    -- Performance metrics (cached for fast queries)
    total_runs integer default 0,
    total_events integer default 0,
    total_duration_ms integer default 0
);

CREATE TABLE org_plan (
    org_plan_id blob primary key,
    org_id blob not null unique references org(org_id) on update restrict on delete restrict,
    plan_uuid blob not null references plan(plan_uuid) on update restrict on delete restrict,
    status_id integer not null references org_plan_status(status_id) on update restrict on delete restrict,
    billing_period text not null,

    -- Usage/accounting period dates
    current_period_start datetime,
    current_period_end datetime,

    -- Audit fields
    created_at datetime default current_timestamp,
    created_by_user_id blob not null references user(user_id) on update restrict on delete restrict,
    updated_at datetime default current_timestamp,
    updated_by_user_id blob references user(user_id) on update restrict on delete restrict
);

CREATE TABLE plan (
    plan_uuid blob primary key,
    plan_id text, -- Internal plan identifier (e.g., 'hot-cloud-starter'), used for lookups and URLs
    plan_name text not null unique, -- Customer-facing display name
    base_price_monthly_cents integer not null,
    base_price_annual_cents integer not null,
    sort_order integer not null,
    active integer default 1,
    created_at datetime default current_timestamp,
    updated_at datetime default current_timestamp,
    features text
);

CREATE TABLE org_plan_status (
    status_id integer primary key,
    status text not null unique,
    sort_order integer not null
);

CREATE TABLE task (
    task_id blob primary key,
    env_id blob not null references env(env_id) on update restrict on delete restrict,
    stream_id blob not null references stream(stream_id) on update restrict on delete restrict,
    build_id blob not null references build(build_id) on update restrict on delete restrict,
    origin_run_id blob references run(run_id) on update restrict on delete restrict,
    task_status_id integer not null default 1 references task_status(task_status_id) on update restrict on delete restrict,
    function_name text not null,
    args text,
    options text,
    task_type text not null default 'code',
    start_time datetime,
    stop_time datetime,
    duration_ms integer,
    result text,
    info text,
    timeout_ms integer not null default 1800000,
    retry_attempt integer not null default 0,
    next_retry_at datetime,
    created_at datetime default current_timestamp,
    by_user_id blob references "user"(user_id) on update restrict on delete restrict
, run_id BLOB, worker_id text, last_heartbeat_at datetime, container_id text);

CREATE TABLE task_status (
    task_status_id integer primary key,
    name text not null unique
);

CREATE TABLE team (
    team_id blob primary key,
    org_id blob not null references org(org_id) on update restrict on delete restrict,
    name text not null,
    active integer default 1,
    created_at datetime default current_timestamp,
    created_by_user_id blob not null references user(user_id) on update restrict on delete restrict,
    updated_at datetime default current_timestamp,
    updated_by_user_id blob references user(user_id) on update restrict on delete restrict,
    active_toggle_at datetime,
    active_toggle_by_user_id blob references user(user_id) on update restrict on delete restrict
);

CREATE TABLE team_user (
    team_user_id blob primary key,
    team_id blob not null references team(team_id) on update restrict on delete restrict,
    user_id blob not null references user(user_id) on update restrict on delete restrict,
    team_user_role_id integer not null default 1 references team_user_role(team_user_role_id) on update restrict on delete restrict,
    active integer default 1,
    created_at datetime default current_timestamp,
    created_by_user_id blob not null references user(user_id) on update restrict on delete restrict,
    updated_at datetime default current_timestamp,
    updated_by_user_id blob references user(user_id) on update restrict on delete restrict,
    active_toggle_at datetime,
    active_toggle_by_user_id blob references user(user_id) on update restrict on delete restrict,
    unique(team_id, user_id)
);

CREATE TABLE team_user_role (
    team_user_role_id integer primary key,
    role text not null unique,
    sort_order integer not null
);

CREATE TABLE user (
    user_id blob primary key,
    email text unique not null,
    name text,
    settings text default '{"notifications": {"newsletter": true, "product_updates": true}}',
    active integer default 1,
    created_at datetime default current_timestamp,
    created_by_user_id blob not null references user(user_id) on update restrict on delete restrict,
    updated_at datetime default current_timestamp,
    updated_by_user_id blob references user(user_id) on update restrict on delete restrict,
    active_toggle_at datetime,
    active_toggle_by_user_id blob references user(user_id) on update restrict on delete restrict
);

CREATE TABLE user_auth (
    user_auth_id blob primary key,
    user_id blob not null references user(user_id) on update restrict on delete restrict,
    auth_type text not null, -- 'email_password', 'google', 'github', etc.
    auth_identifier text not null, -- email for email_password, provider id for oauth
    auth_data text, -- json string: password hash for email_password, tokens/profile for oauth
    active integer default 1, -- sqlite uses integer for boolean (1 = true, 0 = false)
    created_at datetime default current_timestamp,
    created_by_user_id blob not null references user(user_id) on update restrict on delete restrict,
    updated_at datetime default current_timestamp,
    updated_by_user_id blob references user(user_id) on update restrict on delete restrict,
    active_toggle_at datetime,
    active_toggle_by_user_id blob references user(user_id) on update restrict on delete restrict,
    unique(auth_type, auth_identifier)
);

CREATE TABLE webhook (
    webhook_id text primary key,
    build_id text not null references build(build_id) on update restrict on delete restrict,

    -- Webhook service grouping (required)
    service text not null,

    -- Endpoint path and method
    path text not null,
    method text not null default 'POST',

    -- Function location in Hot code
    ns text not null,
    var text not null,

    -- Webhook metadata
    name text not null,
    description text,
    meta text,
    file text,
    line integer,
    "column" integer,
    position integer,

    created_at text default (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),

    -- Unique constraint: one webhook per (build, service, path, method)
    unique (build_id, service, path, method)
);

CREATE INDEX idx_access_api_key on access(api_key_id) where api_key_id is not null;

CREATE INDEX idx_access_created on access(created_at);

CREATE INDEX idx_access_env on access(env_id);

CREATE INDEX idx_access_service_key on access(service_key_id) where service_key_id is not null;

CREATE INDEX idx_access_session on access(session_id) where session_id is not null;

CREATE INDEX idx_agent_build_id on agent(build_id);

CREATE INDEX idx_agent_env_id on agent(env_id);

CREATE INDEX idx_agent_env_namespace_type on agent(env_id, namespace, type_name);

CREATE INDEX idx_agent_type_name on agent(type_name);

CREATE INDEX idx_workflow_build_id on workflow(build_id);

CREATE INDEX idx_workflow_env_id on workflow(env_id);

CREATE INDEX idx_workflow_env_namespace_type on workflow(env_id, namespace, type_name);

CREATE INDEX idx_workflow_type_name on workflow(type_name);

CREATE INDEX idx_alert_channel ON alert(channel);

CREATE INDEX idx_alert_channel_enabled ON alert_channel(enabled) WHERE enabled = 1;

CREATE INDEX idx_alert_channel_env_id ON alert_channel(env_id);

CREATE UNIQUE INDEX idx_alert_channel_name_env ON alert_channel(org_id, env_id, name) WHERE env_id IS NOT NULL;

CREATE UNIQUE INDEX idx_alert_channel_name_org ON alert_channel(org_id, name) WHERE org_id IS NOT NULL AND env_id IS NULL;

CREATE UNIQUE INDEX idx_alert_channel_name_system ON alert_channel(name) WHERE org_id IS NULL AND env_id IS NULL;

CREATE INDEX idx_alert_channel_org_id ON alert_channel(org_id);

CREATE INDEX idx_alert_created_at ON alert(created_at);

CREATE INDEX idx_alert_delivery_alert_id ON alert_delivery(alert_id);

CREATE INDEX idx_alert_delivery_created_at ON alert_delivery(created_at);

CREATE INDEX idx_alert_delivery_destination_id ON alert_delivery(destination_id);

CREATE INDEX idx_alert_delivery_pending_retry ON alert_delivery(next_retry_at)
    WHERE status_id IN (1, 4) AND next_retry_at IS NOT NULL;

CREATE INDEX idx_alert_delivery_status ON alert_delivery(status_id);

CREATE INDEX idx_alert_delivery_subscription_id ON alert_delivery(subscription_id);

CREATE INDEX idx_alert_destination_enabled ON alert_destination(enabled) WHERE enabled = 1;

CREATE INDEX idx_alert_destination_org_id ON alert_destination(org_id);

CREATE INDEX idx_alert_destination_type ON alert_destination(destination_type_id);

CREATE INDEX idx_alert_destination_verification_token
    ON alert_destination(verification_token) WHERE verification_token IS NOT NULL;

CREATE INDEX idx_alert_env_id ON alert(env_id);

CREATE INDEX idx_alert_org_env_created ON alert(org_id, env_id, created_at DESC);

CREATE INDEX idx_alert_org_id ON alert(org_id);

CREATE INDEX idx_alert_subscription_channel_channel ON alert_subscription_channel(channel_id);

CREATE INDEX idx_alert_subscription_channel_enabled ON alert_subscription_channel(enabled) WHERE enabled = 1;

CREATE INDEX idx_alert_subscription_destination_destination ON alert_subscription_destination(destination_id);

CREATE INDEX idx_alert_subscription_destination_enabled ON alert_subscription_destination(enabled) WHERE enabled = 1;

CREATE INDEX idx_alert_subscription_enabled ON alert_subscription(enabled) WHERE enabled = 1;

CREATE INDEX idx_alert_subscription_env_id ON alert_subscription(env_id);

CREATE INDEX idx_alert_subscription_org_id ON alert_subscription(org_id);

CREATE INDEX idx_alert_subscription_team_id ON alert_subscription(team_id);

CREATE INDEX idx_alert_subscription_user_id ON alert_subscription(user_id);

CREATE INDEX idx_api_key_active on api_key(active) where active = 1;

CREATE INDEX idx_api_key_env_id on api_key(env_id);

CREATE INDEX idx_build_active on build(active) where active = 1;

CREATE UNIQUE INDEX idx_build_project_deployed_unique on build(project_id) where deployed = 1;

CREATE INDEX idx_build_project_id on build(project_id);

CREATE UNIQUE INDEX idx_build_project_live_unique on build(project_id) where build_type_id = 2;

CREATE INDEX idx_build_storage_backend on build(storage_backend);

CREATE INDEX idx_build_type_id on build(build_type_id);

CREATE INDEX idx_call_duration_us on call(duration_us) where duration_us is not null;

CREATE INDEX idx_call_function_name on call(function_name);

CREATE INDEX idx_call_parent_call_id on call(parent_call_id);

CREATE INDEX idx_call_root_by_run
    ON call(run_id) WHERE parent_call_id IS NULL;

CREATE INDEX idx_call_run_id on call(run_id);

CREATE INDEX idx_call_runtime_path on call(runtime_path);

CREATE INDEX idx_call_start_time on call(start_time);

CREATE INDEX idx_call_static_scope on call(static_scope);

CREATE INDEX idx_context_active on context(active) where active = 1;

CREATE INDEX idx_context_env_id on context(env_id);

CREATE UNIQUE INDEX idx_context_env_key on context(env_id, key) where env_id is not null and active = 1;

CREATE INDEX idx_context_project_id on context(project_id);

CREATE UNIQUE INDEX idx_context_project_key on context(project_id, key) where project_id is not null and active = 1;

CREATE UNIQUE INDEX idx_domain_domain on domain(domain);

CREATE INDEX idx_domain_env on domain(env_id);

CREATE INDEX idx_email_queue_status ON email_queue (status_id, created_at);

CREATE INDEX idx_email_verification_created_at on email_verification(created_at);

CREATE INDEX idx_email_verification_email on email_verification(email);

CREATE INDEX idx_email_verification_expires_at on email_verification(expires_at);

CREATE INDEX idx_email_verification_status on email_verification(status_id);

CREATE INDEX idx_email_verification_token on email_verification(verification_token);

CREATE INDEX idx_env_active on env(active) where active = 1;

CREATE INDEX idx_env_org_id on env(org_id);

CREATE INDEX idx_event_access on event(access_id) where access_id is not null;

CREATE INDEX idx_event_created_at on event(created_at);

CREATE INDEX idx_event_env_created_at on event(env_id, created_at desc);

CREATE INDEX idx_event_env_handled_created_at on event(env_id, handled, created_at desc);

CREATE INDEX idx_event_env_id on event(env_id);

CREATE INDEX idx_event_event_type on event(event_type);

CREATE INDEX idx_event_handled on event(handled);

CREATE INDEX idx_event_handler_build_id on event_handler (build_id);

CREATE INDEX idx_event_handler_event_type on event_handler (event_type);

CREATE INDEX idx_event_stream_id on event(stream_id);

CREATE INDEX idx_file_active on file(active) where active = 1;

CREATE INDEX idx_file_created_at on file(created_at);

CREATE INDEX idx_file_created_by_run_id on file(created_by_run_id);

CREATE INDEX idx_file_env_id on file(env_id);

CREATE INDEX idx_file_org_env on file(org_id, env_id);

CREATE UNIQUE INDEX idx_file_org_env_path_unique on file(org_id, env_id, path) where active = 1;

CREATE INDEX idx_file_org_id on file(org_id);

CREATE INDEX idx_file_org_size on file(org_id, size) where active = 1;

CREATE INDEX idx_file_path on file(path);

CREATE INDEX idx_file_storage_backend on file(storage_backend);

CREATE INDEX idx_file_upload_expires ON file_upload(expires_at);

CREATE INDEX idx_file_upload_org ON file_upload(org_id);

CREATE INDEX idx_file_upload_status ON file_upload(status);

CREATE INDEX idx_invite_active on invite(active) where active = 1;

CREATE INDEX idx_invite_code on invite(invite_code);

CREATE INDEX idx_invite_created_at on invite(created_at);

CREATE INDEX idx_invite_email on invite(email);

CREATE INDEX idx_invite_expires_at on invite(expires_at);

CREATE INDEX idx_invite_org_id on invite(org_id);

CREATE INDEX idx_invite_status_id on invite(invite_status_id);

CREATE INDEX idx_mcp_tool_build_id on mcp_tool(build_id);

CREATE INDEX idx_mcp_tool_build_service on mcp_tool(build_id, service);

CREATE INDEX idx_mcp_tool_name on mcp_tool(name);

CREATE INDEX idx_mcp_tool_service on mcp_tool(service);

CREATE INDEX idx_org_active on org(active) where active = 1;

CREATE INDEX idx_org_is_personal on org(is_personal) where is_personal = 1;

CREATE INDEX idx_org_name on org(name);

CREATE INDEX idx_org_note_category ON org_note(org_id, category);

CREATE INDEX idx_org_note_created_at ON org_note(org_id, created_at DESC);

CREATE INDEX idx_org_note_org_id ON org_note(org_id);

CREATE INDEX idx_org_type ON org(org_type);

CREATE INDEX idx_org_usage_org_id on org_usage(org_id);

CREATE INDEX idx_org_usage_period_start on org_usage(usage_period_start);

CREATE INDEX idx_org_user_active on org_user(active) where active = 1;

CREATE INDEX idx_org_user_org_id on org_user(org_id);

CREATE INDEX idx_org_user_user_id on org_user(user_id);

CREATE INDEX idx_project_active on project(active) where active = 1;

CREATE INDEX idx_project_env_id on project(env_id);

CREATE UNIQUE INDEX idx_project_env_name on project(env_id, name);

CREATE INDEX idx_run_access on run(access_id) where access_id is not null;

CREATE INDEX idx_run_agent_type on run(agent_type) where agent_type is not null;

CREATE INDEX idx_run_build_id on run(build_id);

CREATE INDEX idx_run_by_user_id on run(by_user_id);

CREATE INDEX idx_run_env_id on run(env_id);

CREATE INDEX idx_run_env_run_type_start_time on run(env_id, run_type_id, start_time desc);

CREATE INDEX idx_run_env_start_non_task
    ON run(env_id, start_time DESC) WHERE run_type_id != 7;

CREATE INDEX idx_run_env_start_time on run(env_id, start_time desc);

CREATE INDEX idx_run_env_status_start_non_task
    ON run(env_id, status_id, start_time DESC) WHERE run_type_id != 7;

CREATE INDEX idx_run_env_status_start_time on run(env_id, status_id, start_time desc);

CREATE INDEX idx_run_event_id on run(event_id);

CREATE INDEX idx_run_info_warning on run(env_id, start_time desc) where json_extract(info, '$.warning') is not null;

CREATE INDEX idx_run_origin_run_id on run(origin_run_id);

CREATE INDEX idx_run_pending_retry on run(next_retry_at) where status_id = 5 and next_retry_at is not null;

CREATE INDEX idx_run_run_type_id on run(run_type_id);

CREATE INDEX idx_run_start_time on run(start_time);

CREATE INDEX idx_run_stop_time on run(stop_time);

CREATE INDEX idx_run_stream_id on run(stream_id);

CREATE INDEX idx_schedule_active on schedule(active) where active = 1;

CREATE INDEX idx_schedule_build_active on schedule(build_id, active) where active = 1;

CREATE INDEX idx_schedule_build_id on schedule(build_id);

CREATE INDEX idx_schedule_log_event_id on schedule_log(event_id);

CREATE INDEX idx_schedule_log_schedule_time on schedule_log(schedule_id, scheduled_time desc);

CREATE INDEX idx_schedule_log_time on schedule_log(scheduled_time);

CREATE UNIQUE INDEX idx_schedule_unique_function on schedule(build_id, ns, var, cron);

CREATE INDEX idx_service_key_active on service_key(env_id)
    where revoked_at is null;

CREATE INDEX idx_service_key_api_key on service_key(api_key_id);

CREATE INDEX idx_service_key_env on service_key(env_id);

CREATE INDEX idx_session_active_expires on session(expires_at)
    where revoked_at is null;

CREATE INDEX idx_session_api_key on session(api_key_id);

CREATE INDEX idx_session_env on session(env_id);

CREATE INDEX idx_stream_created_at on stream(created_at);

CREATE INDEX idx_stream_env_id on stream(env_id);

CREATE INDEX idx_stream_env_last_activity_at on stream(env_id, last_activity_at desc);

CREATE INDEX idx_stream_env_started_at on stream(env_id, started_at desc);

CREATE INDEX idx_stream_last_activity_at on stream(last_activity_at);

CREATE INDEX idx_stream_started_at on stream(started_at);

CREATE INDEX idx_org_plan_org_id on org_plan(org_id);

CREATE INDEX idx_plan_uuid on org_plan(plan_uuid);

CREATE UNIQUE INDEX idx_plan_plan_id_unique on plan(plan_id) where plan_id is not null;

CREATE INDEX idx_org_plan_status_id on org_plan(status_id);

CREATE INDEX idx_task_created_at on task(created_at);

CREATE INDEX idx_task_env_created_at on task(env_id, created_at desc);

CREATE INDEX idx_task_env_id on task(env_id);

CREATE INDEX idx_task_env_status
    ON task(env_id, task_status_id);

CREATE INDEX idx_task_heartbeat_stale
    ON task (last_heartbeat_at)
    WHERE task_status_id = 2 AND last_heartbeat_at IS NOT NULL;

CREATE INDEX idx_task_origin_run on task(origin_run_id);

CREATE INDEX idx_task_pending_retry on task(next_retry_at) where task_status_id = 4 and next_retry_at is not null;

CREATE INDEX idx_task_status on task(task_status_id);

CREATE INDEX idx_task_stream_id on task(stream_id);

CREATE INDEX idx_team_active on team(active) where active = 1;

CREATE INDEX idx_team_name on team(name);

CREATE INDEX idx_team_org_id on team(org_id);

CREATE INDEX idx_team_user_active on team_user(active) where active = 1;

CREATE INDEX idx_team_user_team_id on team_user(team_id);

CREATE INDEX idx_team_user_user_id on team_user(user_id);

CREATE INDEX idx_user_active on user(active) where active = 1;

CREATE INDEX idx_user_auth_active on user_auth(active) where active = 1;

CREATE INDEX idx_user_auth_type_identifier on user_auth(auth_type, auth_identifier);

CREATE INDEX idx_user_auth_user_id on user_auth(user_id);

CREATE INDEX idx_user_email on user(email);

CREATE INDEX idx_webhook_build_id on webhook(build_id);

CREATE INDEX idx_webhook_build_service_path on webhook(build_id, service, path, method);

CREATE INDEX idx_webhook_path on webhook(service, path);

CREATE INDEX idx_webhook_service on webhook(service);

-- Generic reference data.

insert into org_user_role (org_user_role_id, role, sort_order) values
    (1, 'member', 1),
    (2, 'admin', 2);

insert into team_user_role (team_user_role_id, role, sort_order) values
    (1, 'member', 1),
    (2, 'admin', 2);

insert into invite_status (invite_status_id, status, sort_order) values
    (1, 'invited', 1),
    (2, 'joined', 2),
    (3, 'declined', 3);

insert into build_type (build_type_id, build_type, sort_order) values
    (1, 'bundle', 1),
    (2, 'live', 2);

insert into run_status (status_id, status, sort_order) values
    (1, 'running', 1),
    (2, 'succeeded', 2),
    (3, 'failed', 3),
    (4, 'cancelled', 4),
    (5, 'pending_retry', 5);

insert into run_type (run_type_id, run_type, sort_order) values
    (1, 'call', 1),
    (2, 'event', 2),
    (3, 'schedule', 3),
    (4, 'run', 4),
    (5, 'eval', 5),
    (6, 'repl', 6),
    (7, 'task', 7);

insert into scheduler_state (scheduler_id, last_successful_sync_time, updated_at) values
    ('main', '2026-04-25 00:20:28.099', '2026-04-25 00:20:28.099');

insert into alert_destination_type (type_id, type_name, sort_order) values
    (1, 'email', 1),
    (2, 'slack', 2),
    (3, 'pagerduty', 3),
    (4, 'webhook', 4);

insert into alert_channel (alert_channel_id, org_id, env_id, name, pattern, enabled, created_at, updated_at, created_by_user_id, updated_by_user_id) values
    (X'cd59d2883d5431dbbf337d12044f5fc8', NULL, NULL, 'run:failed', 'run:failed', 1, '2026-04-25 00:20:28', '2026-04-25 00:20:28', NULL, NULL),
    (X'0f813a935ed4526c4f485369481f7ccd', NULL, NULL, 'run:cancelled', 'run:cancelled', 1, '2026-04-25 00:20:28', '2026-04-25 00:20:28', NULL, NULL),
    (X'5c27371d5813251209ee0fb67bc1affe', NULL, NULL, 'deploy:failed', 'deploy:failed', 1, '2026-04-25 00:20:28', '2026-04-25 00:20:28', NULL, NULL),
    (X'ac006e9cbd9a2158c89dbf5892d44352', NULL, NULL, 'deploy:succeeded', 'deploy:succeeded', 1, '2026-04-25 00:20:28', '2026-04-25 00:20:28', NULL, NULL);

insert into alert_delivery_status (status_id, status_name, sort_order) values
    (1, 'pending', 1),
    (2, 'sent', 2),
    (3, 'failed', 3),
    (4, 'retrying', 4);

insert into org_plan_status (status_id, status, sort_order) values
    (1, 'active', 1),
    (2, 'inactive', 2),
    (3, 'pending', 3);

insert into email_verification_status (status_id, status, sort_order) values
    (1, 'pending', 1),
    (2, 'verified', 2),
    (3, 'expired', 3);

insert into task_status (task_status_id, name) values
    (1, 'queued'),
    (2, 'running'),
    (3, 'completed'),
    (4, 'failed'),
    (5, 'cancelled'),
    (6, 'timed_out');
