-- Hot 2.0 public Postgres schema baseline.

SET statement_timeout = 0;
SET lock_timeout = 0;
SET idle_in_transaction_session_timeout = 0;
SET transaction_timeout = 0;
SET client_encoding = 'UTF8';
SET standard_conforming_strings = on;
SET search_path = hot, public;
SET check_function_bodies = false;
SET xmloption = content;
SET client_min_messages = warning;
SET row_security = off;

CREATE EXTENSION IF NOT EXISTS vector WITH SCHEMA hot;
CREATE EXTENSION IF NOT EXISTS pg_trgm WITH SCHEMA hot;




CREATE TYPE hot.org_type AS ENUM (
    'individual',
    'organization'
);


SET default_tablespace = '';

SET default_table_access_method = heap;


CREATE TABLE hot.access (
    access_id uuid DEFAULT uuidv7() NOT NULL,
    env_id uuid NOT NULL,
    api_key_id uuid,
    service_key_id uuid,
    session_id uuid,
    source text DEFAULT 'api'::text NOT NULL,
    ip_address text,
    user_agent text,
    host text,
    method text,
    path text,
    query_params text,
    created_at timestamp with time zone DEFAULT now()
);



CREATE TABLE hot.agent (
    agent_id uuid DEFAULT uuidv7() NOT NULL,
    build_id uuid NOT NULL,
    env_id uuid NOT NULL,
    type_name text NOT NULL,
    namespace text NOT NULL,
    name text,
    description text,
    tags jsonb,
    config_fields jsonb,
    meta jsonb,
    file text,
    line integer,
    "column" integer,
    "position" integer,
    created_at timestamp with time zone DEFAULT now()
);
ALTER TABLE ONLY hot.agent ALTER COLUMN tags SET COMPRESSION lz4;
ALTER TABLE ONLY hot.agent ALTER COLUMN config_fields SET COMPRESSION lz4;
ALTER TABLE ONLY hot.agent ALTER COLUMN meta SET COMPRESSION lz4;



CREATE TABLE hot.workflow (
    workflow_id uuid DEFAULT uuidv7() NOT NULL,
    build_id uuid NOT NULL,
    env_id uuid NOT NULL,
    type_name text NOT NULL,
    namespace text NOT NULL,
    name text,
    description text,
    tags jsonb,
    meta jsonb,
    file text,
    line integer,
    "column" integer,
    "position" integer,
    created_at timestamp with time zone DEFAULT now()
);
ALTER TABLE ONLY hot.workflow ALTER COLUMN tags SET COMPRESSION lz4;
ALTER TABLE ONLY hot.workflow ALTER COLUMN meta SET COMPRESSION lz4;



CREATE TABLE hot.alert (
    alert_id uuid DEFAULT uuidv7() NOT NULL,
    org_id uuid NOT NULL,
    env_id uuid NOT NULL,
    channel text NOT NULL,
    data jsonb NOT NULL,
    created_at timestamp with time zone DEFAULT now()
);
ALTER TABLE ONLY hot.alert ALTER COLUMN data SET COMPRESSION lz4;



CREATE TABLE hot.alert_channel (
    alert_channel_id uuid DEFAULT uuidv7() NOT NULL,
    org_id uuid,
    env_id uuid,
    name text NOT NULL,
    pattern text NOT NULL,
    enabled boolean DEFAULT true NOT NULL,
    created_at timestamp with time zone DEFAULT now(),
    updated_at timestamp with time zone DEFAULT now(),
    created_by_user_id uuid,
    updated_by_user_id uuid,
    CONSTRAINT chk_channel_scope CHECK ((((org_id IS NULL) AND (env_id IS NULL)) OR (org_id IS NOT NULL)))
);



CREATE TABLE hot.alert_delivery (
    alert_delivery_id uuid DEFAULT uuidv7() NOT NULL,
    alert_id uuid NOT NULL,
    subscription_id uuid NOT NULL,
    destination_id uuid NOT NULL,
    resolved_user_id uuid,
    status_id smallint DEFAULT 1 NOT NULL,
    attempts integer DEFAULT 0 NOT NULL,
    max_attempts integer DEFAULT 5 NOT NULL,
    next_retry_at timestamp with time zone,
    last_error text,
    sent_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now()
);



CREATE TABLE hot.alert_delivery_status (
    status_id smallint NOT NULL,
    status_name text NOT NULL,
    sort_order smallint NOT NULL
);



CREATE TABLE hot.alert_destination (
    alert_destination_id uuid DEFAULT uuidv7() NOT NULL,
    org_id uuid NOT NULL,
    name text NOT NULL,
    destination_type_id smallint NOT NULL,
    config jsonb NOT NULL,
    enabled boolean DEFAULT true NOT NULL,
    created_at timestamp with time zone DEFAULT now(),
    updated_at timestamp with time zone DEFAULT now(),
    created_by_user_id uuid NOT NULL,
    updated_by_user_id uuid,
    verified boolean DEFAULT true NOT NULL,
    verification_token text,
    verification_expires_at timestamp with time zone,
    verification_attempts integer DEFAULT 0 NOT NULL
);



CREATE TABLE hot.alert_destination_type (
    type_id smallint NOT NULL,
    type_name text NOT NULL,
    sort_order smallint NOT NULL
);



CREATE TABLE hot.alert_subscription (
    alert_subscription_id uuid DEFAULT uuidv7() NOT NULL,
    org_id uuid NOT NULL,
    env_id uuid,
    team_id uuid,
    user_id uuid,
    enabled boolean DEFAULT true NOT NULL,
    created_at timestamp with time zone DEFAULT now(),
    updated_at timestamp with time zone DEFAULT now(),
    created_by_user_id uuid NOT NULL,
    updated_by_user_id uuid,
    CONSTRAINT chk_subscriber_type CHECK ((NOT ((team_id IS NOT NULL) AND (user_id IS NOT NULL))))
);



CREATE TABLE hot.alert_subscription_channel (
    subscription_id uuid NOT NULL,
    channel_id uuid NOT NULL,
    enabled boolean DEFAULT true NOT NULL,
    created_at timestamp with time zone DEFAULT now()
);



CREATE TABLE hot.alert_subscription_destination (
    subscription_id uuid NOT NULL,
    destination_id uuid NOT NULL,
    enabled boolean DEFAULT true NOT NULL,
    created_at timestamp with time zone DEFAULT now()
);



CREATE TABLE hot.api_key (
    api_key_id uuid DEFAULT uuidv7() NOT NULL,
    env_id uuid NOT NULL,
    description text NOT NULL,
    key_data jsonb NOT NULL,
    active boolean DEFAULT true,
    created_by_user_id uuid NOT NULL,
    created_at timestamp with time zone DEFAULT now(),
    updated_at timestamp with time zone DEFAULT now(),
    updated_by_user_id uuid,
    active_toggle_at timestamp with time zone,
    active_toggle_by_user_id uuid,
    permissions jsonb DEFAULT '{}'::jsonb
);
ALTER TABLE ONLY hot.api_key ALTER COLUMN key_data SET COMPRESSION lz4;



CREATE TABLE hot.build (
    build_id uuid DEFAULT uuidv7() NOT NULL,
    project_id uuid NOT NULL,
    hash text NOT NULL,
    size integer NOT NULL,
    build_type_id smallint DEFAULT 1 NOT NULL,
    deployed boolean DEFAULT false,
    active boolean DEFAULT true,
    created_by_user_id uuid NOT NULL,
    created_at timestamp with time zone DEFAULT now(),
    updated_at timestamp with time zone DEFAULT now(),
    updated_by_user_id uuid,
    active_toggle_at timestamp with time zone,
    active_toggle_by_user_id uuid,
    storage_path text,
    storage_backend text
);



CREATE TABLE hot.build_type (
    build_type_id smallint NOT NULL,
    build_type text NOT NULL,
    sort_order smallint NOT NULL
);



CREATE TABLE hot.call (
    call_id uuid NOT NULL,
    run_id uuid NOT NULL,
    parent_call_id uuid,
    function_name text NOT NULL,
    static_scope text NOT NULL,
    runtime_path text NOT NULL,
    call_depth integer NOT NULL,
    args jsonb,
    return_value jsonb,
    error text,
    flow jsonb,
    start_time timestamp with time zone NOT NULL,
    stop_time timestamp with time zone,
    duration_us bigint,
    file text,
    line integer,
    "column" integer,
    "position" integer,
    size bigint DEFAULT 0 NOT NULL
);
ALTER TABLE ONLY hot.call ALTER COLUMN args SET COMPRESSION lz4;
ALTER TABLE ONLY hot.call ALTER COLUMN return_value SET COMPRESSION lz4;
ALTER TABLE ONLY hot.call ALTER COLUMN flow SET COMPRESSION lz4;



CREATE TABLE hot.context (
    context_id uuid DEFAULT uuidv7() NOT NULL,
    env_id uuid,
    project_id uuid,
    key text NOT NULL,
    value text NOT NULL,
    description text,
    active boolean DEFAULT true,
    created_at timestamp with time zone DEFAULT now(),
    created_by_user_id uuid NOT NULL,
    updated_at timestamp with time zone DEFAULT now(),
    updated_by_user_id uuid,
    CONSTRAINT context_check CHECK ((((env_id IS NOT NULL) AND (project_id IS NULL)) OR ((env_id IS NULL) AND (project_id IS NOT NULL))))
);



CREATE TABLE hot.domain (
    domain_id uuid DEFAULT uuidv7() NOT NULL,
    env_id uuid NOT NULL,
    domain text NOT NULL,
    verified_at timestamp with time zone,
    tls_provisioned_at timestamp with time zone,
    certificate_ref text,
    validation_cname_name text,
    validation_cname_value text,
    routing_ref text,
    routing_domain text,
    created_at timestamp with time zone DEFAULT now(),
    deleted_at timestamp with time zone,
    provisioning_error text
);



CREATE TABLE hot.email_queue (
    email_queue_id uuid DEFAULT gen_random_uuid() NOT NULL,
    to_address text NOT NULL,
    subject text NOT NULL,
    html_body text,
    text_body text,
    from_address text NOT NULL,
    status_id smallint DEFAULT 1 NOT NULL,
    error_message text,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    sent_at timestamp with time zone,
    updated_at timestamp with time zone DEFAULT now() NOT NULL
);



CREATE TABLE hot.email_verification (
    verification_id uuid DEFAULT uuidv7() NOT NULL,
    email text NOT NULL,
    name text,
    password_hash text NOT NULL,
    verification_token text NOT NULL,
    status_id smallint DEFAULT 1 NOT NULL,
    invite_code text,
    org_name text,
    org_slug text,
    plan text,
    billing text,
    created_at timestamp with time zone DEFAULT now(),
    expires_at timestamp with time zone NOT NULL,
    verified_at timestamp with time zone,
    attempts integer DEFAULT 0,
    account_type text DEFAULT 'individual'::text
);



CREATE TABLE hot.email_verification_status (
    status_id smallint NOT NULL,
    status text NOT NULL,
    sort_order smallint NOT NULL
);



CREATE TABLE hot.env (
    env_id uuid DEFAULT uuidv7() NOT NULL,
    org_id uuid NOT NULL,
    name text NOT NULL,
    active boolean DEFAULT true,
    created_by_user_id uuid NOT NULL,
    created_at timestamp with time zone DEFAULT now(),
    updated_at timestamp with time zone DEFAULT now(),
    updated_by_user_id uuid,
    active_toggle_at timestamp with time zone,
    active_toggle_by_user_id uuid
);



CREATE TABLE hot.event (
    event_id uuid DEFAULT uuidv7() NOT NULL,
    env_id uuid NOT NULL,
    stream_id uuid NOT NULL,
    event_type text NOT NULL,
    event_data jsonb NOT NULL,
    event_time timestamp with time zone NOT NULL,
    created_at timestamp with time zone DEFAULT now(),
    created_by_user_id uuid NOT NULL,
    handled boolean DEFAULT false NOT NULL,
    access_id uuid
);
ALTER TABLE ONLY hot.event ALTER COLUMN event_data SET COMPRESSION lz4;



CREATE TABLE hot.event_handler (
    event_handler_id uuid DEFAULT uuidv7() NOT NULL,
    build_id uuid NOT NULL,
    event_type text NOT NULL,
    ns text NOT NULL,
    var text NOT NULL,
    meta jsonb,
    value jsonb,
    file text,
    line integer,
    "column" integer,
    "position" integer
);
ALTER TABLE ONLY hot.event_handler ALTER COLUMN meta SET COMPRESSION lz4;
ALTER TABLE ONLY hot.event_handler ALTER COLUMN value SET COMPRESSION lz4;



CREATE TABLE hot.file (
    file_id uuid DEFAULT uuidv7() NOT NULL,
    path text NOT NULL,
    size bigint NOT NULL,
    etag text,
    content_type text,
    storage_backend text NOT NULL,
    storage_path text,
    org_id uuid NOT NULL,
    env_id uuid,
    created_by_run_id uuid,
    updated_by_run_id uuid,
    active boolean DEFAULT true,
    created_at timestamp with time zone DEFAULT now(),
    created_by_user_id uuid NOT NULL,
    updated_at timestamp with time zone DEFAULT now(),
    updated_by_user_id uuid,
    active_toggle_at timestamp with time zone,
    active_toggle_by_user_id uuid
);



CREATE TABLE hot.file_upload (
    upload_id uuid DEFAULT uuidv7() NOT NULL,
    path text NOT NULL,
    org_id uuid NOT NULL,
    env_id uuid,
    created_by_user_id uuid NOT NULL,
    status text DEFAULT 'pending'::text NOT NULL,
    expected_size bigint,
    content_type text,
    part_size bigint NOT NULL,
    parts_expected integer,
    parts_received integer DEFAULT 0 NOT NULL,
    bytes_received bigint DEFAULT 0 NOT NULL,
    backend_upload_id text,
    parts_manifest jsonb DEFAULT '[]'::jsonb NOT NULL,
    storage_backend text NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    expires_at timestamp with time zone NOT NULL
);



CREATE TABLE hot.invite (
    invite_id uuid DEFAULT uuidv7() NOT NULL,
    invite_code text NOT NULL,
    email text NOT NULL,
    org_id uuid NOT NULL,
    invite_status_id smallint DEFAULT 1 NOT NULL,
    intended_org_user_role_id smallint DEFAULT 1 NOT NULL,
    active boolean DEFAULT true,
    created_at timestamp with time zone DEFAULT now(),
    created_by_user_id uuid NOT NULL,
    updated_at timestamp with time zone DEFAULT now(),
    updated_by_user_id uuid,
    expires_at timestamp with time zone NOT NULL,
    used_at timestamp with time zone
);



CREATE TABLE hot.invite_status (
    invite_status_id smallint NOT NULL,
    status text NOT NULL,
    sort_order smallint NOT NULL
);



CREATE TABLE hot.mcp_tool (
    mcp_tool_id uuid DEFAULT uuidv7() NOT NULL,
    build_id uuid NOT NULL,
    service text NOT NULL,
    ns text NOT NULL,
    var text NOT NULL,
    name text NOT NULL,
    description text,
    input_schema jsonb,
    output_schema jsonb,
    title text,
    icons jsonb,
    annotations jsonb,
    meta jsonb,
    file text,
    line integer,
    "column" integer,
    "position" integer,
    created_at timestamp with time zone DEFAULT now()
);
ALTER TABLE ONLY hot.mcp_tool ALTER COLUMN input_schema SET COMPRESSION lz4;
ALTER TABLE ONLY hot.mcp_tool ALTER COLUMN output_schema SET COMPRESSION lz4;
ALTER TABLE ONLY hot.mcp_tool ALTER COLUMN icons SET COMPRESSION lz4;
ALTER TABLE ONLY hot.mcp_tool ALTER COLUMN annotations SET COMPRESSION lz4;
ALTER TABLE ONLY hot.mcp_tool ALTER COLUMN meta SET COMPRESSION lz4;



CREATE TABLE hot.org (
    org_id uuid DEFAULT uuidv7() NOT NULL,
    name text NOT NULL,
    slug text NOT NULL,
    settings jsonb DEFAULT '{}'::jsonb,
    active boolean DEFAULT true,
    created_at timestamp with time zone DEFAULT now(),
    created_by_user_id uuid NOT NULL,
    updated_at timestamp with time zone DEFAULT now(),
    updated_by_user_id uuid,
    active_toggle_at timestamp with time zone,
    active_toggle_by_user_id uuid,
    features jsonb,
    org_type hot.org_type DEFAULT 'organization'::hot.org_type NOT NULL
);
ALTER TABLE ONLY hot.org ALTER COLUMN settings SET COMPRESSION lz4;
ALTER TABLE ONLY hot.org ALTER COLUMN features SET COMPRESSION lz4;



CREATE TABLE hot.org_note (
    note_id uuid DEFAULT gen_random_uuid() NOT NULL,
    org_id uuid NOT NULL,
    category character varying(50) NOT NULL,
    note_type character varying(100) NOT NULL,
    message text NOT NULL,
    metadata jsonb,
    created_by uuid,
    created_at timestamp with time zone DEFAULT now() NOT NULL
);



CREATE TABLE hot.org_usage (
    usage_id uuid DEFAULT uuidv7() NOT NULL,
    org_id uuid NOT NULL,
    usage_period_start timestamp with time zone NOT NULL,
    usage_period_end timestamp with time zone NOT NULL,
    runs_count integer DEFAULT 0,
    team_members_count integer DEFAULT 0,
    metrics jsonb,
    created_at timestamp with time zone DEFAULT now()
);
ALTER TABLE ONLY hot.org_usage ALTER COLUMN metrics SET COMPRESSION lz4;



CREATE TABLE hot.org_user (
    org_user_id uuid DEFAULT uuidv7() NOT NULL,
    org_id uuid NOT NULL,
    user_id uuid NOT NULL,
    org_user_role_id smallint DEFAULT 1 NOT NULL,
    active boolean DEFAULT true,
    created_at timestamp with time zone DEFAULT now(),
    created_by_user_id uuid NOT NULL,
    updated_at timestamp with time zone DEFAULT now(),
    updated_by_user_id uuid,
    active_toggle_at timestamp with time zone,
    active_toggle_by_user_id uuid
);



CREATE TABLE hot.org_user_role (
    org_user_role_id smallint NOT NULL,
    role text NOT NULL,
    sort_order smallint NOT NULL
);



CREATE TABLE hot.project (
    project_id uuid DEFAULT uuidv7() NOT NULL,
    env_id uuid NOT NULL,
    name text NOT NULL,
    active boolean DEFAULT true,
    created_by_user_id uuid NOT NULL,
    created_at timestamp with time zone DEFAULT now(),
    updated_at timestamp with time zone DEFAULT now(),
    updated_by_user_id uuid,
    active_toggle_at timestamp with time zone,
    active_toggle_by_user_id uuid
);



CREATE TABLE hot.run (
    run_id uuid DEFAULT uuidv7() NOT NULL,
    env_id uuid NOT NULL,
    stream_id uuid NOT NULL,
    build_id uuid NOT NULL,
    run_type_id smallint NOT NULL,
    origin_run_id uuid,
    event_id uuid,
    start_time timestamp with time zone DEFAULT now(),
    stop_time timestamp with time zone,
    status_id smallint DEFAULT 1 NOT NULL,
    by_user_id uuid,
    result jsonb,
    info jsonb,
    retry_attempt smallint DEFAULT 0 NOT NULL,
    next_retry_at timestamp with time zone,
    access_id uuid,
    agent_type text
);
ALTER TABLE ONLY hot.run ALTER COLUMN result SET COMPRESSION lz4;
ALTER TABLE ONLY hot.run ALTER COLUMN info SET COMPRESSION lz4;



CREATE TABLE hot.run_status (
    status_id smallint NOT NULL,
    status text NOT NULL,
    sort_order smallint NOT NULL
);



CREATE TABLE hot.run_type (
    run_type_id smallint NOT NULL,
    run_type text NOT NULL,
    sort_order smallint NOT NULL
);



CREATE TABLE hot.schedule (
    schedule_id uuid DEFAULT uuidv7() NOT NULL,
    build_id uuid NOT NULL,
    cron text NOT NULL,
    ns text NOT NULL,
    var text NOT NULL,
    meta jsonb,
    value jsonb,
    file text,
    line integer,
    "column" integer,
    "position" integer,
    active boolean DEFAULT true NOT NULL,
    created_at timestamp with time zone DEFAULT now() NOT NULL,
    deactivated_at timestamp with time zone
);
ALTER TABLE ONLY hot.schedule ALTER COLUMN meta SET COMPRESSION lz4;
ALTER TABLE ONLY hot.schedule ALTER COLUMN value SET COMPRESSION lz4;



CREATE TABLE hot.schedule_log (
    log_id uuid DEFAULT uuidv7() NOT NULL,
    schedule_id uuid NOT NULL,
    event_id uuid,
    scheduled_time timestamp with time zone NOT NULL,
    executed_at timestamp with time zone DEFAULT now() NOT NULL,
    is_backfill boolean DEFAULT false NOT NULL,
    created_at timestamp with time zone DEFAULT now()
);



CREATE TABLE hot.scheduler_state (
    scheduler_id text DEFAULT 'main'::text NOT NULL,
    last_successful_sync_time timestamp with time zone NOT NULL,
    updated_at timestamp with time zone DEFAULT now()
);



CREATE TABLE hot.service_key (
    service_key_id uuid DEFAULT uuidv7() NOT NULL,
    api_key_id uuid NOT NULL,
    env_id uuid NOT NULL,
    name text,
    description text,
    secret_hash bytea NOT NULL,
    permissions jsonb NOT NULL,
    metadata text,
    expires_at timestamp with time zone,
    revoked_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now(),
    last_used_at timestamp with time zone
);
ALTER TABLE ONLY hot.service_key ALTER COLUMN permissions SET COMPRESSION lz4;



CREATE TABLE hot.session (
    session_id uuid DEFAULT uuidv7() NOT NULL,
    api_key_id uuid NOT NULL,
    env_id uuid NOT NULL,
    secret_hash bytea NOT NULL,
    permissions jsonb NOT NULL,
    metadata jsonb,
    expires_at timestamp with time zone NOT NULL,
    revoked_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now(),
    last_used_at timestamp with time zone
);
ALTER TABLE ONLY hot.session ALTER COLUMN permissions SET COMPRESSION lz4;
ALTER TABLE ONLY hot.session ALTER COLUMN metadata SET COMPRESSION lz4;



CREATE TABLE hot.store_map (
    store_id uuid DEFAULT gen_random_uuid() NOT NULL,
    name text NOT NULL,
    org_id uuid NOT NULL,
    env_id uuid,
    embedding_model text,
    embedding_dimensions integer,
    embedding_field text DEFAULT 'content'::text,
    text_search boolean DEFAULT false,
    created_at timestamp with time zone DEFAULT now()
);



CREATE TABLE hot.store_map_entry (
    org_id uuid NOT NULL,
    env_id uuid NOT NULL,
    store_name text NOT NULL,
    key jsonb NOT NULL,
    value jsonb NOT NULL,
    seq bigint NOT NULL,
    embedding hot.vector,
    text_content text,
    size bigint DEFAULT 0 NOT NULL,
    created_at timestamp with time zone DEFAULT now(),
    updated_at timestamp with time zone DEFAULT now()
);



CREATE SEQUENCE hot.store_map_entry_seq_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;



ALTER SEQUENCE hot.store_map_entry_seq_seq OWNED BY hot.store_map_entry.seq;



CREATE TABLE hot.stream (
    stream_id uuid DEFAULT uuidv7() NOT NULL,
    env_id uuid NOT NULL,
    name text,
    description text,
    tags jsonb,
    created_at timestamp with time zone DEFAULT now(),
    started_at timestamp with time zone,
    last_activity_at timestamp with time zone DEFAULT now(),
    total_runs integer DEFAULT 0,
    total_events integer DEFAULT 0,
    total_duration_ms bigint DEFAULT 0
);
ALTER TABLE ONLY hot.stream ALTER COLUMN tags SET COMPRESSION lz4;



CREATE TABLE hot.org_plan (
    org_plan_id uuid DEFAULT uuidv7() NOT NULL,
    org_id uuid NOT NULL,
    plan_uuid uuid NOT NULL,
    status_id smallint NOT NULL,
    billing_period text NOT NULL,
    current_period_start timestamp with time zone,
    current_period_end timestamp with time zone,
    created_at timestamp with time zone DEFAULT now(),
    created_by_user_id uuid NOT NULL,
    updated_at timestamp with time zone DEFAULT now(),
    updated_by_user_id uuid
);



CREATE TABLE hot.plan (
    plan_uuid uuid DEFAULT uuidv7() NOT NULL,
    plan_id text,
    plan_name text NOT NULL,
    base_price_monthly_cents integer NOT NULL,
    base_price_annual_cents integer NOT NULL,
    sort_order integer NOT NULL,
    active boolean DEFAULT true,
    created_at timestamp with time zone DEFAULT now(),
    updated_at timestamp with time zone DEFAULT now(),
    features jsonb
);
ALTER TABLE ONLY hot.plan ALTER COLUMN features SET COMPRESSION lz4;



CREATE TABLE hot.org_plan_status (
    status_id smallint NOT NULL,
    status text NOT NULL,
    sort_order smallint NOT NULL
);



CREATE TABLE hot.task (
    task_id uuid DEFAULT gen_random_uuid() NOT NULL,
    env_id uuid NOT NULL,
    stream_id uuid NOT NULL,
    build_id uuid NOT NULL,
    origin_run_id uuid,
    task_status_id smallint DEFAULT 1 NOT NULL,
    function_name text NOT NULL,
    args jsonb,
    options jsonb,
    task_type text DEFAULT 'code'::text NOT NULL,
    start_time timestamp with time zone,
    stop_time timestamp with time zone,
    duration_ms bigint,
    result jsonb,
    info jsonb,
    timeout_ms bigint DEFAULT 1800000 NOT NULL,
    retry_attempt smallint DEFAULT 0 NOT NULL,
    next_retry_at timestamp with time zone,
    created_at timestamp with time zone DEFAULT now(),
    by_user_id uuid,
    run_id uuid,
    worker_id text,
    last_heartbeat_at timestamp with time zone,
    container_id text
);
ALTER TABLE ONLY hot.task ALTER COLUMN args SET COMPRESSION lz4;
ALTER TABLE ONLY hot.task ALTER COLUMN options SET COMPRESSION lz4;
ALTER TABLE ONLY hot.task ALTER COLUMN result SET COMPRESSION lz4;
ALTER TABLE ONLY hot.task ALTER COLUMN info SET COMPRESSION lz4;



CREATE TABLE hot.task_status (
    task_status_id smallint NOT NULL,
    name text NOT NULL
);



CREATE TABLE hot.team (
    team_id uuid DEFAULT uuidv7() NOT NULL,
    org_id uuid NOT NULL,
    name text NOT NULL,
    active boolean DEFAULT true,
    created_at timestamp with time zone DEFAULT now(),
    created_by_user_id uuid NOT NULL,
    updated_at timestamp with time zone DEFAULT now(),
    updated_by_user_id uuid,
    active_toggle_at timestamp with time zone,
    active_toggle_by_user_id uuid
);



CREATE TABLE hot.team_user (
    team_user_id uuid DEFAULT uuidv7() NOT NULL,
    team_id uuid NOT NULL,
    user_id uuid NOT NULL,
    team_user_role_id smallint DEFAULT 1 NOT NULL,
    active boolean DEFAULT true,
    created_at timestamp with time zone DEFAULT now(),
    created_by_user_id uuid NOT NULL,
    updated_at timestamp with time zone DEFAULT now(),
    updated_by_user_id uuid,
    active_toggle_at timestamp with time zone,
    active_toggle_by_user_id uuid
);



CREATE TABLE hot.team_user_role (
    team_user_role_id smallint NOT NULL,
    role text NOT NULL,
    sort_order smallint NOT NULL
);



CREATE TABLE hot."user" (
    user_id uuid DEFAULT uuidv7() NOT NULL,
    email text NOT NULL,
    name text,
    settings jsonb DEFAULT '{"notifications": {"newsletter": true, "product_updates": true}}'::jsonb,
    active boolean DEFAULT true,
    created_at timestamp with time zone DEFAULT now(),
    created_by_user_id uuid NOT NULL,
    updated_at timestamp with time zone DEFAULT now(),
    updated_by_user_id uuid,
    active_toggle_at timestamp with time zone,
    active_toggle_by_user_id uuid
);
ALTER TABLE ONLY hot."user" ALTER COLUMN settings SET COMPRESSION lz4;



CREATE TABLE hot.user_auth (
    user_auth_id uuid DEFAULT uuidv7() NOT NULL,
    user_id uuid NOT NULL,
    auth_type text NOT NULL,
    auth_identifier text NOT NULL,
    auth_data jsonb,
    active boolean DEFAULT true,
    created_at timestamp with time zone DEFAULT now(),
    created_by_user_id uuid NOT NULL,
    updated_at timestamp with time zone DEFAULT now(),
    updated_by_user_id uuid,
    active_toggle_at timestamp with time zone,
    active_toggle_by_user_id uuid
);
ALTER TABLE ONLY hot.user_auth ALTER COLUMN auth_data SET COMPRESSION lz4;



CREATE TABLE hot.webhook (
    webhook_id uuid DEFAULT uuidv7() NOT NULL,
    build_id uuid NOT NULL,
    service text NOT NULL,
    path text NOT NULL,
    method text DEFAULT 'POST'::text NOT NULL,
    ns text NOT NULL,
    var text NOT NULL,
    name text NOT NULL,
    description text,
    meta jsonb,
    file text,
    line integer,
    "column" integer,
    "position" integer,
    created_at timestamp with time zone DEFAULT now()
);
ALTER TABLE ONLY hot.webhook ALTER COLUMN meta SET COMPRESSION lz4;



ALTER TABLE ONLY hot.store_map_entry ALTER COLUMN seq SET DEFAULT nextval('hot.store_map_entry_seq_seq'::regclass);



ALTER TABLE ONLY hot.access
    ADD CONSTRAINT access_pkey PRIMARY KEY (access_id);



ALTER TABLE ONLY hot.agent
    ADD CONSTRAINT agent_pkey PRIMARY KEY (agent_id);



ALTER TABLE ONLY hot.workflow
    ADD CONSTRAINT workflow_pkey PRIMARY KEY (workflow_id);



ALTER TABLE ONLY hot.alert_channel
    ADD CONSTRAINT alert_channel_pkey PRIMARY KEY (alert_channel_id);



ALTER TABLE ONLY hot.alert_delivery
    ADD CONSTRAINT alert_delivery_pkey PRIMARY KEY (alert_delivery_id);



ALTER TABLE ONLY hot.alert_delivery_status
    ADD CONSTRAINT alert_delivery_status_pkey PRIMARY KEY (status_id);



ALTER TABLE ONLY hot.alert_delivery_status
    ADD CONSTRAINT alert_delivery_status_status_name_key UNIQUE (status_name);



ALTER TABLE ONLY hot.alert_destination
    ADD CONSTRAINT alert_destination_org_id_name_key UNIQUE (org_id, name);



ALTER TABLE ONLY hot.alert_destination
    ADD CONSTRAINT alert_destination_pkey PRIMARY KEY (alert_destination_id);



ALTER TABLE ONLY hot.alert_destination_type
    ADD CONSTRAINT alert_destination_type_pkey PRIMARY KEY (type_id);



ALTER TABLE ONLY hot.alert_destination_type
    ADD CONSTRAINT alert_destination_type_type_name_key UNIQUE (type_name);



ALTER TABLE ONLY hot.alert_destination
    ADD CONSTRAINT alert_destination_verification_token_key UNIQUE (verification_token);



ALTER TABLE ONLY hot.alert
    ADD CONSTRAINT alert_pkey PRIMARY KEY (alert_id);



ALTER TABLE ONLY hot.alert_subscription_channel
    ADD CONSTRAINT alert_subscription_channel_pkey PRIMARY KEY (subscription_id, channel_id);



ALTER TABLE ONLY hot.alert_subscription_destination
    ADD CONSTRAINT alert_subscription_destination_pkey PRIMARY KEY (subscription_id, destination_id);



ALTER TABLE ONLY hot.alert_subscription
    ADD CONSTRAINT alert_subscription_pkey PRIMARY KEY (alert_subscription_id);



ALTER TABLE ONLY hot.api_key
    ADD CONSTRAINT api_key_pkey PRIMARY KEY (api_key_id);



ALTER TABLE ONLY hot.build
    ADD CONSTRAINT build_pkey PRIMARY KEY (build_id);



ALTER TABLE ONLY hot.build_type
    ADD CONSTRAINT build_type_build_type_key UNIQUE (build_type);



ALTER TABLE ONLY hot.build_type
    ADD CONSTRAINT build_type_pkey PRIMARY KEY (build_type_id);



ALTER TABLE ONLY hot.call
    ADD CONSTRAINT call_pkey PRIMARY KEY (call_id);



ALTER TABLE ONLY hot.context
    ADD CONSTRAINT context_pkey PRIMARY KEY (context_id);



ALTER TABLE ONLY hot.domain
    ADD CONSTRAINT domain_domain_key UNIQUE (domain);



ALTER TABLE ONLY hot.domain
    ADD CONSTRAINT domain_pkey PRIMARY KEY (domain_id);



ALTER TABLE ONLY hot.email_queue
    ADD CONSTRAINT email_queue_pkey PRIMARY KEY (email_queue_id);



ALTER TABLE ONLY hot.email_verification
    ADD CONSTRAINT email_verification_pkey PRIMARY KEY (verification_id);



ALTER TABLE ONLY hot.email_verification_status
    ADD CONSTRAINT email_verification_status_pkey PRIMARY KEY (status_id);



ALTER TABLE ONLY hot.email_verification_status
    ADD CONSTRAINT email_verification_status_status_key UNIQUE (status);



ALTER TABLE ONLY hot.email_verification
    ADD CONSTRAINT email_verification_verification_token_key UNIQUE (verification_token);



ALTER TABLE ONLY hot.env
    ADD CONSTRAINT env_pkey PRIMARY KEY (env_id);



ALTER TABLE ONLY hot.event_handler
    ADD CONSTRAINT event_handler_pkey PRIMARY KEY (event_handler_id);



ALTER TABLE ONLY hot.event
    ADD CONSTRAINT event_pkey PRIMARY KEY (event_id);



ALTER TABLE ONLY hot.file
    ADD CONSTRAINT file_pkey PRIMARY KEY (file_id);



ALTER TABLE ONLY hot.file_upload
    ADD CONSTRAINT file_upload_pkey PRIMARY KEY (upload_id);



ALTER TABLE ONLY hot.invite
    ADD CONSTRAINT invite_invite_code_key UNIQUE (invite_code);



ALTER TABLE ONLY hot.invite
    ADD CONSTRAINT invite_pkey PRIMARY KEY (invite_id);



ALTER TABLE ONLY hot.invite_status
    ADD CONSTRAINT invite_status_pkey PRIMARY KEY (invite_status_id);



ALTER TABLE ONLY hot.invite_status
    ADD CONSTRAINT invite_status_status_key UNIQUE (status);



ALTER TABLE ONLY hot.mcp_tool
    ADD CONSTRAINT mcp_tool_pkey PRIMARY KEY (mcp_tool_id);



ALTER TABLE ONLY hot.org_note
    ADD CONSTRAINT org_note_pkey PRIMARY KEY (note_id);



ALTER TABLE ONLY hot.org
    ADD CONSTRAINT org_pkey PRIMARY KEY (org_id);



ALTER TABLE ONLY hot.org
    ADD CONSTRAINT org_slug_key UNIQUE (slug);



ALTER TABLE ONLY hot.org_usage
    ADD CONSTRAINT org_usage_pkey PRIMARY KEY (usage_id);



ALTER TABLE ONLY hot.org_user
    ADD CONSTRAINT org_user_org_id_user_id_key UNIQUE (org_id, user_id);



ALTER TABLE ONLY hot.org_user
    ADD CONSTRAINT org_user_pkey PRIMARY KEY (org_user_id);



ALTER TABLE ONLY hot.org_user_role
    ADD CONSTRAINT org_user_role_pkey PRIMARY KEY (org_user_role_id);



ALTER TABLE ONLY hot.org_user_role
    ADD CONSTRAINT org_user_role_role_key UNIQUE (role);



ALTER TABLE ONLY hot.project
    ADD CONSTRAINT project_pkey PRIMARY KEY (project_id);



ALTER TABLE ONLY hot.run
    ADD CONSTRAINT run_pkey PRIMARY KEY (run_id);



ALTER TABLE ONLY hot.run_status
    ADD CONSTRAINT run_status_pkey PRIMARY KEY (status_id);



ALTER TABLE ONLY hot.run_status
    ADD CONSTRAINT run_status_status_key UNIQUE (status);



ALTER TABLE ONLY hot.run_type
    ADD CONSTRAINT run_type_pkey PRIMARY KEY (run_type_id);



ALTER TABLE ONLY hot.run_type
    ADD CONSTRAINT run_type_run_type_key UNIQUE (run_type);



ALTER TABLE ONLY hot.schedule_log
    ADD CONSTRAINT schedule_log_pkey PRIMARY KEY (log_id);



ALTER TABLE ONLY hot.schedule
    ADD CONSTRAINT schedule_pkey PRIMARY KEY (schedule_id);



ALTER TABLE ONLY hot.scheduler_state
    ADD CONSTRAINT scheduler_state_pkey PRIMARY KEY (scheduler_id);



ALTER TABLE ONLY hot.service_key
    ADD CONSTRAINT service_key_pkey PRIMARY KEY (service_key_id);



ALTER TABLE ONLY hot.session
    ADD CONSTRAINT session_pkey PRIMARY KEY (session_id);



ALTER TABLE ONLY hot.store_map_entry
    ADD CONSTRAINT store_map_entry_pkey PRIMARY KEY (org_id, env_id, store_name, key);



ALTER TABLE ONLY hot.store_map
    ADD CONSTRAINT store_map_org_id_env_id_name_key UNIQUE (org_id, env_id, name);



ALTER TABLE ONLY hot.store_map
    ADD CONSTRAINT store_map_pkey PRIMARY KEY (store_id);



ALTER TABLE ONLY hot.stream
    ADD CONSTRAINT stream_pkey PRIMARY KEY (stream_id);



ALTER TABLE ONLY hot.org_plan
    ADD CONSTRAINT subscription_org_id_key UNIQUE (org_id);



ALTER TABLE ONLY hot.org_plan
    ADD CONSTRAINT org_plan_pkey PRIMARY KEY (org_plan_id);



ALTER TABLE ONLY hot.plan
    ADD CONSTRAINT plan_pkey PRIMARY KEY (plan_uuid);



ALTER TABLE ONLY hot.plan
    ADD CONSTRAINT plan_plan_name_key UNIQUE (plan_name);



ALTER TABLE ONLY hot.org_plan_status
    ADD CONSTRAINT org_plan_status_pkey PRIMARY KEY (status_id);



ALTER TABLE ONLY hot.org_plan_status
    ADD CONSTRAINT org_plan_status_status_key UNIQUE (status);





ALTER TABLE ONLY hot.task
    ADD CONSTRAINT task_pkey PRIMARY KEY (task_id);



ALTER TABLE ONLY hot.task_status
    ADD CONSTRAINT task_status_name_key UNIQUE (name);



ALTER TABLE ONLY hot.task_status
    ADD CONSTRAINT task_status_pkey PRIMARY KEY (task_status_id);



ALTER TABLE ONLY hot.team
    ADD CONSTRAINT team_pkey PRIMARY KEY (team_id);



ALTER TABLE ONLY hot.team_user
    ADD CONSTRAINT team_user_pkey PRIMARY KEY (team_user_id);



ALTER TABLE ONLY hot.team_user_role
    ADD CONSTRAINT team_user_role_pkey PRIMARY KEY (team_user_role_id);



ALTER TABLE ONLY hot.team_user_role
    ADD CONSTRAINT team_user_role_role_key UNIQUE (role);



ALTER TABLE ONLY hot.team_user
    ADD CONSTRAINT team_user_team_id_user_id_key UNIQUE (team_id, user_id);



ALTER TABLE ONLY hot.agent
    ADD CONSTRAINT unique_agent_per_build UNIQUE (build_id, namespace, type_name);



ALTER TABLE ONLY hot.workflow
    ADD CONSTRAINT unique_workflow_per_build UNIQUE (build_id, namespace, type_name);



ALTER TABLE ONLY hot.event_handler
    ADD CONSTRAINT unique_event_handler_per_build UNIQUE (build_id, ns, var, event_type);



ALTER TABLE ONLY hot.mcp_tool
    ADD CONSTRAINT unique_mcp_tool_per_build UNIQUE (build_id, service, name);



ALTER TABLE ONLY hot.webhook
    ADD CONSTRAINT unique_webhook_per_build UNIQUE (build_id, service, path, method);



ALTER TABLE ONLY hot.user_auth
    ADD CONSTRAINT user_auth_auth_type_auth_identifier_key UNIQUE (auth_type, auth_identifier);



ALTER TABLE ONLY hot.user_auth
    ADD CONSTRAINT user_auth_pkey PRIMARY KEY (user_auth_id);



ALTER TABLE ONLY hot."user"
    ADD CONSTRAINT user_email_key UNIQUE (email);



ALTER TABLE ONLY hot."user"
    ADD CONSTRAINT user_pkey PRIMARY KEY (user_id);



ALTER TABLE ONLY hot.webhook
    ADD CONSTRAINT webhook_pkey PRIMARY KEY (webhook_id);



CREATE INDEX idx_access_api_key ON hot.access USING btree (api_key_id) WHERE (api_key_id IS NOT NULL);



CREATE INDEX idx_access_created ON hot.access USING btree (created_at);



CREATE INDEX idx_access_env ON hot.access USING btree (env_id);



CREATE INDEX idx_access_service_key ON hot.access USING btree (service_key_id) WHERE (service_key_id IS NOT NULL);



CREATE INDEX idx_access_session ON hot.access USING btree (session_id) WHERE (session_id IS NOT NULL);



CREATE INDEX idx_agent_build_env ON hot.agent USING btree (build_id, env_id);



CREATE INDEX idx_agent_build_id ON hot.agent USING btree (build_id);



CREATE INDEX idx_agent_env_id ON hot.agent USING btree (env_id);



CREATE INDEX idx_agent_env_namespace_type ON hot.agent USING btree (env_id, namespace, type_name);



CREATE INDEX idx_agent_type_name ON hot.agent USING btree (type_name);



CREATE INDEX idx_workflow_build_env ON hot.workflow USING btree (build_id, env_id);



CREATE INDEX idx_workflow_build_id ON hot.workflow USING btree (build_id);



CREATE INDEX idx_workflow_env_id ON hot.workflow USING btree (env_id);



CREATE INDEX idx_workflow_env_namespace_type ON hot.workflow USING btree (env_id, namespace, type_name);



CREATE INDEX idx_workflow_type_name ON hot.workflow USING btree (type_name);



CREATE INDEX idx_alert_channel ON hot.alert USING btree (channel);



CREATE INDEX idx_alert_channel_enabled ON hot.alert_channel USING btree (enabled) WHERE (enabled = true);



CREATE INDEX idx_alert_channel_env_id ON hot.alert_channel USING btree (env_id);



CREATE UNIQUE INDEX idx_alert_channel_name_env ON hot.alert_channel USING btree (org_id, env_id, name) WHERE ((org_id IS NOT NULL) AND (env_id IS NOT NULL));



CREATE UNIQUE INDEX idx_alert_channel_name_org ON hot.alert_channel USING btree (org_id, name) WHERE ((org_id IS NOT NULL) AND (env_id IS NULL));



CREATE UNIQUE INDEX idx_alert_channel_name_system ON hot.alert_channel USING btree (name) WHERE (org_id IS NULL);



CREATE INDEX idx_alert_channel_org_id ON hot.alert_channel USING btree (org_id);



CREATE INDEX idx_alert_created_at ON hot.alert USING brin (created_at);



CREATE INDEX idx_alert_delivery_alert_id ON hot.alert_delivery USING btree (alert_id);



CREATE INDEX idx_alert_delivery_created_at ON hot.alert_delivery USING brin (created_at);



CREATE INDEX idx_alert_delivery_destination_id ON hot.alert_delivery USING btree (destination_id);



CREATE INDEX idx_alert_delivery_pending_retry ON hot.alert_delivery USING btree (next_retry_at) WHERE ((status_id = ANY (ARRAY[1, 4])) AND (next_retry_at IS NOT NULL));



CREATE INDEX idx_alert_delivery_status ON hot.alert_delivery USING btree (status_id);



CREATE INDEX idx_alert_delivery_subscription_id ON hot.alert_delivery USING btree (subscription_id);



CREATE INDEX idx_alert_destination_enabled ON hot.alert_destination USING btree (enabled) WHERE (enabled = true);



CREATE INDEX idx_alert_destination_org_id ON hot.alert_destination USING btree (org_id);



CREATE INDEX idx_alert_destination_type ON hot.alert_destination USING btree (destination_type_id);



CREATE INDEX idx_alert_destination_verification_token ON hot.alert_destination USING btree (verification_token) WHERE (verification_token IS NOT NULL);



CREATE INDEX idx_alert_env_id ON hot.alert USING btree (env_id);



CREATE INDEX idx_alert_org_env_created ON hot.alert USING btree (org_id, env_id, created_at DESC);



CREATE INDEX idx_alert_org_id ON hot.alert USING btree (org_id);



CREATE INDEX idx_alert_subscription_channel_channel ON hot.alert_subscription_channel USING btree (channel_id);



CREATE INDEX idx_alert_subscription_channel_enabled ON hot.alert_subscription_channel USING btree (enabled) WHERE (enabled = true);



CREATE INDEX idx_alert_subscription_destination_destination ON hot.alert_subscription_destination USING btree (destination_id);



CREATE INDEX idx_alert_subscription_destination_enabled ON hot.alert_subscription_destination USING btree (enabled) WHERE (enabled = true);



CREATE INDEX idx_alert_subscription_enabled ON hot.alert_subscription USING btree (enabled) WHERE (enabled = true);



CREATE INDEX idx_alert_subscription_env_id ON hot.alert_subscription USING btree (env_id);



CREATE INDEX idx_alert_subscription_org_id ON hot.alert_subscription USING btree (org_id);



CREATE INDEX idx_alert_subscription_team_id ON hot.alert_subscription USING btree (team_id);



CREATE INDEX idx_alert_subscription_user_id ON hot.alert_subscription USING btree (user_id);



CREATE INDEX idx_build_storage_backend ON hot.build USING btree (storage_backend);



CREATE INDEX idx_call_root_by_run ON hot.call USING btree (run_id) INCLUDE (function_name) WHERE (parent_call_id IS NULL);



CREATE UNIQUE INDEX idx_domain_domain ON hot.domain USING btree (domain);



CREATE INDEX idx_domain_env ON hot.domain USING btree (env_id);



CREATE INDEX idx_email_queue_status ON hot.email_queue USING btree (status_id, created_at) WHERE (status_id = 1);



CREATE INDEX idx_email_verification_created_at ON hot.email_verification USING brin (created_at);



CREATE INDEX idx_email_verification_email ON hot.email_verification USING btree (email);



CREATE INDEX idx_email_verification_expires_at ON hot.email_verification USING brin (expires_at);



CREATE INDEX idx_email_verification_status ON hot.email_verification USING btree (status_id);



CREATE INDEX idx_email_verification_token ON hot.email_verification USING btree (verification_token);



CREATE INDEX idx_event_access ON hot.event USING btree (access_id) WHERE (access_id IS NOT NULL);



CREATE INDEX idx_event_handler_build_id ON hot.event_handler USING btree (build_id);



CREATE INDEX idx_event_handler_event_type ON hot.event_handler USING btree (event_type);



CREATE INDEX idx_file_active ON hot.file USING btree (active) WHERE (active = true);



CREATE INDEX idx_file_created_at ON hot.file USING btree (created_at);



CREATE INDEX idx_file_created_by_run_id ON hot.file USING btree (created_by_run_id);



CREATE INDEX idx_file_env_id ON hot.file USING btree (env_id);



CREATE INDEX idx_file_org_env ON hot.file USING btree (org_id, env_id);



CREATE UNIQUE INDEX idx_file_org_env_path_active_unique ON hot.file USING btree (org_id, env_id, path) WHERE (active = true);



CREATE INDEX idx_file_org_id ON hot.file USING btree (org_id);



CREATE INDEX idx_file_org_size ON hot.file USING btree (org_id, size) WHERE (active = true);



CREATE INDEX idx_file_path ON hot.file USING btree (path);



CREATE INDEX idx_file_storage_backend ON hot.file USING btree (storage_backend);



CREATE INDEX idx_file_upload_expires ON hot.file_upload USING btree (expires_at) WHERE (status = 'pending'::text);



CREATE INDEX idx_file_upload_org ON hot.file_upload USING btree (org_id);



CREATE INDEX idx_file_upload_status ON hot.file_upload USING btree (status) WHERE (status = 'pending'::text);



CREATE INDEX idx_hot_api_key_active ON hot.api_key USING btree (active) WHERE (active = true);



CREATE INDEX idx_hot_api_key_env_id ON hot.api_key USING btree (env_id);



CREATE INDEX idx_hot_build_active ON hot.build USING btree (active) WHERE (active = true);



CREATE UNIQUE INDEX idx_hot_build_project_deployed_unique ON hot.build USING btree (project_id) WHERE (deployed = true);



CREATE INDEX idx_hot_build_project_id ON hot.build USING btree (project_id);



CREATE UNIQUE INDEX idx_hot_build_project_live_unique ON hot.build USING btree (project_id) WHERE (build_type_id = 2);



CREATE INDEX idx_hot_build_type_id ON hot.build USING btree (build_type_id);



CREATE INDEX idx_hot_call_duration_us ON hot.call USING btree (duration_us) WHERE (duration_us IS NOT NULL);



CREATE INDEX idx_hot_call_function_name ON hot.call USING btree (function_name);



CREATE INDEX idx_hot_call_parent_call_id ON hot.call USING btree (parent_call_id);



CREATE INDEX idx_hot_call_run_id ON hot.call USING btree (run_id);



CREATE INDEX idx_hot_call_runtime_path ON hot.call USING btree (runtime_path);



CREATE INDEX idx_hot_call_start_time ON hot.call USING brin (start_time);



CREATE INDEX idx_hot_call_static_scope ON hot.call USING btree (static_scope);



CREATE INDEX idx_hot_context_active ON hot.context USING btree (active) WHERE (active = true);



CREATE INDEX idx_hot_context_env_id ON hot.context USING btree (env_id);



CREATE UNIQUE INDEX idx_hot_context_env_key ON hot.context USING btree (env_id, key) WHERE ((env_id IS NOT NULL) AND (active = true));



CREATE INDEX idx_hot_context_project_id ON hot.context USING btree (project_id);



CREATE UNIQUE INDEX idx_hot_context_project_key ON hot.context USING btree (project_id, key) WHERE ((project_id IS NOT NULL) AND (active = true));



CREATE INDEX idx_hot_env_active ON hot.env USING btree (active) WHERE (active = true);



CREATE INDEX idx_hot_env_org_id ON hot.env USING btree (org_id);



CREATE INDEX idx_hot_event_created_at ON hot.event USING brin (created_at);



CREATE INDEX idx_hot_event_env_created_at ON hot.event USING btree (env_id, created_at DESC);



CREATE INDEX idx_hot_event_env_handled_created_at ON hot.event USING btree (env_id, handled, created_at DESC);



CREATE INDEX idx_hot_event_env_id ON hot.event USING btree (env_id);



CREATE INDEX idx_hot_event_event_type ON hot.event USING btree (event_type);



CREATE INDEX idx_hot_event_handled ON hot.event USING btree (handled);



CREATE INDEX idx_hot_event_stream_id ON hot.event USING btree (stream_id);



CREATE INDEX idx_hot_invite_active ON hot.invite USING btree (active) WHERE (active = true);



CREATE INDEX idx_hot_invite_code ON hot.invite USING btree (invite_code);



CREATE INDEX idx_hot_invite_created_at ON hot.invite USING brin (created_at);



CREATE INDEX idx_hot_invite_email ON hot.invite USING btree (email);



CREATE INDEX idx_hot_invite_expires_at ON hot.invite USING brin (expires_at);



CREATE INDEX idx_hot_invite_org_id ON hot.invite USING btree (org_id);



CREATE INDEX idx_hot_invite_status_id ON hot.invite USING btree (invite_status_id);



CREATE INDEX idx_hot_org_active ON hot.org USING btree (active) WHERE (active = true);



CREATE INDEX idx_hot_org_name ON hot.org USING btree (name);



CREATE INDEX idx_hot_org_type ON hot.org USING btree (org_type);



CREATE INDEX idx_hot_org_usage_org_id ON hot.org_usage USING btree (org_id);



CREATE INDEX idx_hot_org_usage_period_start ON hot.org_usage USING brin (usage_period_start);



CREATE INDEX idx_hot_org_user_active ON hot.org_user USING btree (active) WHERE (active = true);



CREATE INDEX idx_hot_org_user_org_id ON hot.org_user USING btree (org_id);



CREATE INDEX idx_hot_org_user_user_id ON hot.org_user USING btree (user_id);



CREATE INDEX idx_hot_project_active ON hot.project USING btree (active) WHERE (active = true);



CREATE INDEX idx_hot_project_env_id ON hot.project USING btree (env_id);



CREATE INDEX idx_hot_run_build_id ON hot.run USING btree (build_id);



CREATE INDEX idx_hot_run_by_user_id ON hot.run USING btree (by_user_id);



CREATE INDEX idx_hot_run_env_id ON hot.run USING btree (env_id);



CREATE INDEX idx_hot_run_env_run_type_start_time ON hot.run USING btree (env_id, run_type_id, start_time DESC);



CREATE INDEX idx_hot_run_env_start_time ON hot.run USING btree (env_id, start_time DESC);



CREATE INDEX idx_hot_run_env_status_start_time ON hot.run USING btree (env_id, status_id, start_time DESC);



CREATE INDEX idx_hot_run_event_id ON hot.run USING btree (event_id);



CREATE INDEX idx_hot_run_info_warning ON hot.run USING btree (env_id, start_time DESC) WHERE ((info ->> 'warning'::text) IS NOT NULL);



CREATE INDEX idx_hot_run_origin_run_id ON hot.run USING btree (origin_run_id);



CREATE INDEX idx_hot_run_pending_retry ON hot.run USING btree (next_retry_at) WHERE ((status_id = 5) AND (next_retry_at IS NOT NULL));



CREATE INDEX idx_hot_run_run_type_id ON hot.run USING btree (run_type_id);



CREATE INDEX idx_hot_run_start_time ON hot.run USING brin (start_time);



CREATE INDEX idx_hot_run_status_id ON hot.run USING btree (status_id);



CREATE INDEX idx_hot_run_stop_time ON hot.run USING brin (stop_time);



CREATE INDEX idx_hot_run_stream_id ON hot.run USING btree (stream_id);



CREATE INDEX idx_hot_schedule_build_id ON hot.schedule USING btree (build_id);



CREATE INDEX idx_hot_stream_created_at ON hot.stream USING brin (created_at);



CREATE INDEX idx_hot_stream_env_id ON hot.stream USING btree (env_id);



CREATE INDEX idx_hot_stream_env_last_activity_at ON hot.stream USING btree (env_id, last_activity_at DESC);



CREATE INDEX idx_hot_stream_env_started_at ON hot.stream USING btree (env_id, started_at DESC NULLS LAST);



CREATE INDEX idx_hot_stream_last_activity_at ON hot.stream USING btree (last_activity_at);



CREATE INDEX idx_hot_stream_started_at ON hot.stream USING brin (started_at);



CREATE INDEX idx_hot_subscription_org_id ON hot.org_plan USING btree (org_id);



CREATE INDEX idx_hot_plan_uuid ON hot.org_plan USING btree (plan_uuid);



CREATE INDEX idx_hot_org_plan_status_id ON hot.org_plan USING btree (status_id);







CREATE INDEX idx_hot_team_active ON hot.team USING btree (active) WHERE (active = true);



CREATE INDEX idx_hot_team_name ON hot.team USING btree (name);



CREATE INDEX idx_hot_team_org_id ON hot.team USING btree (org_id);



CREATE INDEX idx_hot_team_user_active ON hot.team_user USING btree (active) WHERE (active = true);



CREATE INDEX idx_hot_team_user_team_id ON hot.team_user USING btree (team_id);



CREATE INDEX idx_hot_team_user_user_id ON hot.team_user USING btree (user_id);



CREATE INDEX idx_hot_user_active ON hot."user" USING btree (active) WHERE (active = true);



CREATE INDEX idx_hot_user_auth_active ON hot.user_auth USING btree (active) WHERE (active = true);



CREATE INDEX idx_hot_user_auth_type_identifier ON hot.user_auth USING btree (auth_type, auth_identifier);



CREATE INDEX idx_hot_user_auth_user_id ON hot.user_auth USING btree (user_id);



CREATE INDEX idx_hot_user_email ON hot."user" USING btree (email);



CREATE INDEX idx_mcp_tool_build_id ON hot.mcp_tool USING btree (build_id);



CREATE INDEX idx_mcp_tool_build_service ON hot.mcp_tool USING btree (build_id, service);



CREATE INDEX idx_mcp_tool_name ON hot.mcp_tool USING btree (name);



CREATE INDEX idx_mcp_tool_service ON hot.mcp_tool USING btree (service);



CREATE INDEX idx_org_note_category ON hot.org_note USING btree (org_id, category);



CREATE INDEX idx_org_note_created_at ON hot.org_note USING btree (org_id, created_at DESC);



CREATE INDEX idx_org_note_org_id ON hot.org_note USING btree (org_id);



CREATE INDEX idx_run_access ON hot.run USING btree (access_id) WHERE (access_id IS NOT NULL);



CREATE INDEX idx_run_agent_type ON hot.run USING btree (agent_type) WHERE (agent_type IS NOT NULL);



CREATE INDEX idx_run_env_start_non_task ON hot.run USING btree (env_id, start_time DESC) WHERE (run_type_id <> 7);



CREATE INDEX idx_run_env_status_start_non_task ON hot.run USING btree (env_id, status_id, start_time DESC) WHERE (run_type_id <> 7);



CREATE INDEX idx_schedule_active ON hot.schedule USING btree (active) WHERE (active = true);



CREATE INDEX idx_schedule_build_active ON hot.schedule USING btree (build_id, active) WHERE (active = true);



CREATE INDEX idx_schedule_log_event_id ON hot.schedule_log USING btree (event_id);



CREATE INDEX idx_schedule_log_schedule_time ON hot.schedule_log USING btree (schedule_id, scheduled_time DESC);



CREATE INDEX idx_schedule_log_time ON hot.schedule_log USING btree (scheduled_time);



CREATE UNIQUE INDEX idx_schedule_unique_function ON hot.schedule USING btree (build_id, ns, var, cron);



CREATE INDEX idx_service_key_active ON hot.service_key USING btree (env_id) WHERE (revoked_at IS NULL);



CREATE INDEX idx_service_key_api_key ON hot.service_key USING btree (api_key_id);



CREATE INDEX idx_service_key_env ON hot.service_key USING btree (env_id);



CREATE INDEX idx_session_active_expires ON hot.session USING btree (expires_at) WHERE (revoked_at IS NULL);



CREATE INDEX idx_session_api_key ON hot.session USING btree (api_key_id);



CREATE INDEX idx_session_env ON hot.session USING btree (env_id);



CREATE INDEX idx_store_map_entry_order ON hot.store_map_entry USING btree (org_id, env_id, store_name, seq);



CREATE INDEX idx_store_map_entry_text ON hot.store_map_entry USING gin (text_content hot.gin_trgm_ops) WHERE (text_content IS NOT NULL);



CREATE UNIQUE INDEX idx_plan_plan_id_unique ON hot.plan USING btree (plan_id) WHERE (plan_id IS NOT NULL);



CREATE INDEX idx_task_created_at ON hot.task USING brin (created_at);



CREATE INDEX idx_task_env_created_at ON hot.task USING btree (env_id, created_at DESC);



CREATE INDEX idx_task_env_id ON hot.task USING btree (env_id);



CREATE INDEX idx_task_env_status ON hot.task USING btree (env_id, task_status_id);



CREATE INDEX idx_task_heartbeat_stale ON hot.task USING btree (last_heartbeat_at) WHERE ((task_status_id = 2) AND (last_heartbeat_at IS NOT NULL));



CREATE INDEX idx_task_origin_run ON hot.task USING btree (origin_run_id);



CREATE INDEX idx_task_pending_retry ON hot.task USING btree (next_retry_at) WHERE ((task_status_id = 4) AND (next_retry_at IS NOT NULL));



CREATE INDEX idx_task_run_id_fn ON hot.task USING btree (run_id) INCLUDE (function_name);



CREATE INDEX idx_task_status ON hot.task USING btree (task_status_id);



CREATE INDEX idx_task_stream_id ON hot.task USING btree (stream_id);



CREATE INDEX idx_webhook_build_id ON hot.webhook USING btree (build_id);



CREATE INDEX idx_webhook_build_service_path ON hot.webhook USING btree (build_id, service, path, method);



CREATE INDEX idx_webhook_path ON hot.webhook USING btree (service, path);



CREATE INDEX idx_webhook_service ON hot.webhook USING btree (service);



ALTER TABLE ONLY hot.agent
    ADD CONSTRAINT agent_build_id_fkey FOREIGN KEY (build_id) REFERENCES hot.build(build_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.agent
    ADD CONSTRAINT agent_env_id_fkey FOREIGN KEY (env_id) REFERENCES hot.env(env_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.workflow
    ADD CONSTRAINT workflow_build_id_fkey FOREIGN KEY (build_id) REFERENCES hot.build(build_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.workflow
    ADD CONSTRAINT workflow_env_id_fkey FOREIGN KEY (env_id) REFERENCES hot.env(env_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.alert_channel
    ADD CONSTRAINT alert_channel_created_by_user_id_fkey FOREIGN KEY (created_by_user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.alert_channel
    ADD CONSTRAINT alert_channel_env_id_fkey FOREIGN KEY (env_id) REFERENCES hot.env(env_id) ON UPDATE RESTRICT ON DELETE CASCADE;



ALTER TABLE ONLY hot.alert_channel
    ADD CONSTRAINT alert_channel_org_id_fkey FOREIGN KEY (org_id) REFERENCES hot.org(org_id) ON UPDATE RESTRICT ON DELETE CASCADE;



ALTER TABLE ONLY hot.alert_channel
    ADD CONSTRAINT alert_channel_updated_by_user_id_fkey FOREIGN KEY (updated_by_user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.alert_delivery
    ADD CONSTRAINT alert_delivery_alert_id_fkey FOREIGN KEY (alert_id) REFERENCES hot.alert(alert_id) ON UPDATE RESTRICT ON DELETE CASCADE;



ALTER TABLE ONLY hot.alert_delivery
    ADD CONSTRAINT alert_delivery_destination_id_fkey FOREIGN KEY (destination_id) REFERENCES hot.alert_destination(alert_destination_id) ON UPDATE RESTRICT ON DELETE CASCADE;



ALTER TABLE ONLY hot.alert_delivery
    ADD CONSTRAINT alert_delivery_resolved_user_id_fkey FOREIGN KEY (resolved_user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE SET NULL;



ALTER TABLE ONLY hot.alert_delivery
    ADD CONSTRAINT alert_delivery_status_id_fkey FOREIGN KEY (status_id) REFERENCES hot.alert_delivery_status(status_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.alert_delivery
    ADD CONSTRAINT alert_delivery_subscription_id_fkey FOREIGN KEY (subscription_id) REFERENCES hot.alert_subscription(alert_subscription_id) ON UPDATE RESTRICT ON DELETE CASCADE;



ALTER TABLE ONLY hot.alert_destination
    ADD CONSTRAINT alert_destination_created_by_user_id_fkey FOREIGN KEY (created_by_user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.alert_destination
    ADD CONSTRAINT alert_destination_destination_type_id_fkey FOREIGN KEY (destination_type_id) REFERENCES hot.alert_destination_type(type_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.alert_destination
    ADD CONSTRAINT alert_destination_org_id_fkey FOREIGN KEY (org_id) REFERENCES hot.org(org_id) ON UPDATE RESTRICT ON DELETE CASCADE;



ALTER TABLE ONLY hot.alert_destination
    ADD CONSTRAINT alert_destination_updated_by_user_id_fkey FOREIGN KEY (updated_by_user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.alert
    ADD CONSTRAINT alert_env_id_fkey FOREIGN KEY (env_id) REFERENCES hot.env(env_id) ON UPDATE RESTRICT ON DELETE CASCADE;



ALTER TABLE ONLY hot.alert
    ADD CONSTRAINT alert_org_id_fkey FOREIGN KEY (org_id) REFERENCES hot.org(org_id) ON UPDATE RESTRICT ON DELETE CASCADE;



ALTER TABLE ONLY hot.alert_subscription_channel
    ADD CONSTRAINT alert_subscription_channel_channel_id_fkey FOREIGN KEY (channel_id) REFERENCES hot.alert_channel(alert_channel_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.alert_subscription_channel
    ADD CONSTRAINT alert_subscription_channel_subscription_id_fkey FOREIGN KEY (subscription_id) REFERENCES hot.alert_subscription(alert_subscription_id) ON UPDATE RESTRICT ON DELETE CASCADE;



ALTER TABLE ONLY hot.alert_subscription
    ADD CONSTRAINT alert_subscription_created_by_user_id_fkey FOREIGN KEY (created_by_user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.alert_subscription_destination
    ADD CONSTRAINT alert_subscription_destination_destination_id_fkey FOREIGN KEY (destination_id) REFERENCES hot.alert_destination(alert_destination_id) ON UPDATE RESTRICT ON DELETE CASCADE;



ALTER TABLE ONLY hot.alert_subscription_destination
    ADD CONSTRAINT alert_subscription_destination_subscription_id_fkey FOREIGN KEY (subscription_id) REFERENCES hot.alert_subscription(alert_subscription_id) ON UPDATE RESTRICT ON DELETE CASCADE;



ALTER TABLE ONLY hot.alert_subscription
    ADD CONSTRAINT alert_subscription_env_id_fkey FOREIGN KEY (env_id) REFERENCES hot.env(env_id) ON UPDATE RESTRICT ON DELETE CASCADE;



ALTER TABLE ONLY hot.alert_subscription
    ADD CONSTRAINT alert_subscription_org_id_fkey FOREIGN KEY (org_id) REFERENCES hot.org(org_id) ON UPDATE RESTRICT ON DELETE CASCADE;



ALTER TABLE ONLY hot.alert_subscription
    ADD CONSTRAINT alert_subscription_team_id_fkey FOREIGN KEY (team_id) REFERENCES hot.team(team_id) ON UPDATE RESTRICT ON DELETE CASCADE;



ALTER TABLE ONLY hot.alert_subscription
    ADD CONSTRAINT alert_subscription_updated_by_user_id_fkey FOREIGN KEY (updated_by_user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.alert_subscription
    ADD CONSTRAINT alert_subscription_user_id_fkey FOREIGN KEY (user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE CASCADE;



ALTER TABLE ONLY hot.api_key
    ADD CONSTRAINT api_key_active_toggle_by_user_id_fkey FOREIGN KEY (active_toggle_by_user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.api_key
    ADD CONSTRAINT api_key_created_by_user_id_fkey FOREIGN KEY (created_by_user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.api_key
    ADD CONSTRAINT api_key_env_id_fkey FOREIGN KEY (env_id) REFERENCES hot.env(env_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.api_key
    ADD CONSTRAINT api_key_updated_by_user_id_fkey FOREIGN KEY (updated_by_user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.build
    ADD CONSTRAINT build_active_toggle_by_user_id_fkey FOREIGN KEY (active_toggle_by_user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.build
    ADD CONSTRAINT build_build_type_id_fkey FOREIGN KEY (build_type_id) REFERENCES hot.build_type(build_type_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.build
    ADD CONSTRAINT build_created_by_user_id_fkey FOREIGN KEY (created_by_user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.build
    ADD CONSTRAINT build_project_id_fkey FOREIGN KEY (project_id) REFERENCES hot.project(project_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.build
    ADD CONSTRAINT build_updated_by_user_id_fkey FOREIGN KEY (updated_by_user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.call
    ADD CONSTRAINT call_run_id_fkey FOREIGN KEY (run_id) REFERENCES hot.run(run_id) ON UPDATE RESTRICT ON DELETE CASCADE;



ALTER TABLE ONLY hot.context
    ADD CONSTRAINT context_created_by_user_id_fkey FOREIGN KEY (created_by_user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.context
    ADD CONSTRAINT context_env_id_fkey FOREIGN KEY (env_id) REFERENCES hot.env(env_id) ON UPDATE RESTRICT ON DELETE CASCADE;



ALTER TABLE ONLY hot.context
    ADD CONSTRAINT context_project_id_fkey FOREIGN KEY (project_id) REFERENCES hot.project(project_id) ON UPDATE RESTRICT ON DELETE CASCADE;



ALTER TABLE ONLY hot.context
    ADD CONSTRAINT context_updated_by_user_id_fkey FOREIGN KEY (updated_by_user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.domain
    ADD CONSTRAINT domain_env_id_fkey FOREIGN KEY (env_id) REFERENCES hot.env(env_id) ON UPDATE RESTRICT ON DELETE CASCADE;



ALTER TABLE ONLY hot.email_verification
    ADD CONSTRAINT email_verification_status_id_fkey FOREIGN KEY (status_id) REFERENCES hot.email_verification_status(status_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.env
    ADD CONSTRAINT env_active_toggle_by_user_id_fkey FOREIGN KEY (active_toggle_by_user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.env
    ADD CONSTRAINT env_created_by_user_id_fkey FOREIGN KEY (created_by_user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.env
    ADD CONSTRAINT env_org_id_fkey FOREIGN KEY (org_id) REFERENCES hot.org(org_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.env
    ADD CONSTRAINT env_updated_by_user_id_fkey FOREIGN KEY (updated_by_user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.event
    ADD CONSTRAINT event_created_by_user_id_fkey FOREIGN KEY (created_by_user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.event
    ADD CONSTRAINT event_env_id_fkey FOREIGN KEY (env_id) REFERENCES hot.env(env_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.event_handler
    ADD CONSTRAINT event_handler_build_id_fkey FOREIGN KEY (build_id) REFERENCES hot.build(build_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.event
    ADD CONSTRAINT event_stream_id_fkey FOREIGN KEY (stream_id) REFERENCES hot.stream(stream_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.file
    ADD CONSTRAINT file_active_toggle_by_user_id_fkey FOREIGN KEY (active_toggle_by_user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.file
    ADD CONSTRAINT file_created_by_run_id_fkey FOREIGN KEY (created_by_run_id) REFERENCES hot.run(run_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.file
    ADD CONSTRAINT file_created_by_user_id_fkey FOREIGN KEY (created_by_user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.file
    ADD CONSTRAINT file_env_id_fkey FOREIGN KEY (env_id) REFERENCES hot.env(env_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.file
    ADD CONSTRAINT file_org_id_fkey FOREIGN KEY (org_id) REFERENCES hot.org(org_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.file
    ADD CONSTRAINT file_updated_by_run_id_fkey FOREIGN KEY (updated_by_run_id) REFERENCES hot.run(run_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.file
    ADD CONSTRAINT file_updated_by_user_id_fkey FOREIGN KEY (updated_by_user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.file_upload
    ADD CONSTRAINT file_upload_env_id_fkey FOREIGN KEY (env_id) REFERENCES hot.env(env_id);



ALTER TABLE ONLY hot.file_upload
    ADD CONSTRAINT file_upload_org_id_fkey FOREIGN KEY (org_id) REFERENCES hot.org(org_id);



ALTER TABLE ONLY hot.invite
    ADD CONSTRAINT invite_created_by_user_id_fkey FOREIGN KEY (created_by_user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.invite
    ADD CONSTRAINT invite_intended_org_user_role_id_fkey FOREIGN KEY (intended_org_user_role_id) REFERENCES hot.org_user_role(org_user_role_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.invite
    ADD CONSTRAINT invite_invite_status_id_fkey FOREIGN KEY (invite_status_id) REFERENCES hot.invite_status(invite_status_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.invite
    ADD CONSTRAINT invite_org_id_fkey FOREIGN KEY (org_id) REFERENCES hot.org(org_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.invite
    ADD CONSTRAINT invite_updated_by_user_id_fkey FOREIGN KEY (updated_by_user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.mcp_tool
    ADD CONSTRAINT mcp_tool_build_id_fkey FOREIGN KEY (build_id) REFERENCES hot.build(build_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.org
    ADD CONSTRAINT org_active_toggle_by_user_id_fkey FOREIGN KEY (active_toggle_by_user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.org
    ADD CONSTRAINT org_created_by_user_id_fkey FOREIGN KEY (created_by_user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.org_note
    ADD CONSTRAINT org_note_created_by_fkey FOREIGN KEY (created_by) REFERENCES hot."user"(user_id);



ALTER TABLE ONLY hot.org_note
    ADD CONSTRAINT org_note_org_id_fkey FOREIGN KEY (org_id) REFERENCES hot.org(org_id);



ALTER TABLE ONLY hot.org
    ADD CONSTRAINT org_updated_by_user_id_fkey FOREIGN KEY (updated_by_user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.org_usage
    ADD CONSTRAINT org_usage_org_id_fkey FOREIGN KEY (org_id) REFERENCES hot.org(org_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.org_user
    ADD CONSTRAINT org_user_active_toggle_by_user_id_fkey FOREIGN KEY (active_toggle_by_user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.org_user
    ADD CONSTRAINT org_user_created_by_user_id_fkey FOREIGN KEY (created_by_user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.org_user
    ADD CONSTRAINT org_user_org_id_fkey FOREIGN KEY (org_id) REFERENCES hot.org(org_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.org_user
    ADD CONSTRAINT org_user_org_user_role_id_fkey FOREIGN KEY (org_user_role_id) REFERENCES hot.org_user_role(org_user_role_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.org_user
    ADD CONSTRAINT org_user_updated_by_user_id_fkey FOREIGN KEY (updated_by_user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.org_user
    ADD CONSTRAINT org_user_user_id_fkey FOREIGN KEY (user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.project
    ADD CONSTRAINT project_active_toggle_by_user_id_fkey FOREIGN KEY (active_toggle_by_user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.project
    ADD CONSTRAINT project_created_by_user_id_fkey FOREIGN KEY (created_by_user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.project
    ADD CONSTRAINT project_env_id_fkey FOREIGN KEY (env_id) REFERENCES hot.env(env_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.project
    ADD CONSTRAINT project_updated_by_user_id_fkey FOREIGN KEY (updated_by_user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.run
    ADD CONSTRAINT run_build_id_fkey FOREIGN KEY (build_id) REFERENCES hot.build(build_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.run
    ADD CONSTRAINT run_by_user_id_fkey FOREIGN KEY (by_user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.run
    ADD CONSTRAINT run_env_id_fkey FOREIGN KEY (env_id) REFERENCES hot.env(env_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.run
    ADD CONSTRAINT run_event_id_fkey FOREIGN KEY (event_id) REFERENCES hot.event(event_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.run
    ADD CONSTRAINT run_origin_run_id_fkey FOREIGN KEY (origin_run_id) REFERENCES hot.run(run_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.run
    ADD CONSTRAINT run_run_type_id_fkey FOREIGN KEY (run_type_id) REFERENCES hot.run_type(run_type_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.run
    ADD CONSTRAINT run_status_id_fkey FOREIGN KEY (status_id) REFERENCES hot.run_status(status_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.run
    ADD CONSTRAINT run_stream_id_fkey FOREIGN KEY (stream_id) REFERENCES hot.stream(stream_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.schedule
    ADD CONSTRAINT schedule_build_id_fkey FOREIGN KEY (build_id) REFERENCES hot.build(build_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.schedule_log
    ADD CONSTRAINT schedule_log_event_id_fkey FOREIGN KEY (event_id) REFERENCES hot.event(event_id) ON DELETE SET NULL;



ALTER TABLE ONLY hot.schedule_log
    ADD CONSTRAINT schedule_log_schedule_id_fkey FOREIGN KEY (schedule_id) REFERENCES hot.schedule(schedule_id) ON DELETE CASCADE;



ALTER TABLE ONLY hot.service_key
    ADD CONSTRAINT service_key_api_key_id_fkey FOREIGN KEY (api_key_id) REFERENCES hot.api_key(api_key_id) ON UPDATE RESTRICT ON DELETE CASCADE;



ALTER TABLE ONLY hot.service_key
    ADD CONSTRAINT service_key_env_id_fkey FOREIGN KEY (env_id) REFERENCES hot.env(env_id) ON UPDATE RESTRICT ON DELETE CASCADE;



ALTER TABLE ONLY hot.session
    ADD CONSTRAINT session_api_key_id_fkey FOREIGN KEY (api_key_id) REFERENCES hot.api_key(api_key_id) ON UPDATE RESTRICT ON DELETE CASCADE;



ALTER TABLE ONLY hot.session
    ADD CONSTRAINT session_env_id_fkey FOREIGN KEY (env_id) REFERENCES hot.env(env_id) ON UPDATE RESTRICT ON DELETE CASCADE;



ALTER TABLE ONLY hot.stream
    ADD CONSTRAINT stream_env_id_fkey FOREIGN KEY (env_id) REFERENCES hot.env(env_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.org_plan
    ADD CONSTRAINT subscription_created_by_user_id_fkey FOREIGN KEY (created_by_user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.org_plan
    ADD CONSTRAINT subscription_org_id_fkey FOREIGN KEY (org_id) REFERENCES hot.org(org_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.org_plan
    ADD CONSTRAINT org_plan_status_id_fkey FOREIGN KEY (status_id) REFERENCES hot.org_plan_status(status_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.org_plan
    ADD CONSTRAINT plan_uuid_fkey FOREIGN KEY (plan_uuid) REFERENCES hot.plan(plan_uuid) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.org_plan
    ADD CONSTRAINT subscription_updated_by_user_id_fkey FOREIGN KEY (updated_by_user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.task
    ADD CONSTRAINT task_build_id_fkey FOREIGN KEY (build_id) REFERENCES hot.build(build_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.task
    ADD CONSTRAINT task_by_user_id_fkey FOREIGN KEY (by_user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.task
    ADD CONSTRAINT task_env_id_fkey FOREIGN KEY (env_id) REFERENCES hot.env(env_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.task
    ADD CONSTRAINT task_origin_run_id_fkey FOREIGN KEY (origin_run_id) REFERENCES hot.run(run_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.task
    ADD CONSTRAINT task_run_id_fkey FOREIGN KEY (run_id) REFERENCES hot.run(run_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.task
    ADD CONSTRAINT task_stream_id_fkey FOREIGN KEY (stream_id) REFERENCES hot.stream(stream_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.task
    ADD CONSTRAINT task_task_status_id_fkey FOREIGN KEY (task_status_id) REFERENCES hot.task_status(task_status_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.team
    ADD CONSTRAINT team_active_toggle_by_user_id_fkey FOREIGN KEY (active_toggle_by_user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.team
    ADD CONSTRAINT team_created_by_user_id_fkey FOREIGN KEY (created_by_user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.team
    ADD CONSTRAINT team_org_id_fkey FOREIGN KEY (org_id) REFERENCES hot.org(org_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.team
    ADD CONSTRAINT team_updated_by_user_id_fkey FOREIGN KEY (updated_by_user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.team_user
    ADD CONSTRAINT team_user_active_toggle_by_user_id_fkey FOREIGN KEY (active_toggle_by_user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.team_user
    ADD CONSTRAINT team_user_created_by_user_id_fkey FOREIGN KEY (created_by_user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.team_user
    ADD CONSTRAINT team_user_team_id_fkey FOREIGN KEY (team_id) REFERENCES hot.team(team_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.team_user
    ADD CONSTRAINT team_user_team_user_role_id_fkey FOREIGN KEY (team_user_role_id) REFERENCES hot.team_user_role(team_user_role_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.team_user
    ADD CONSTRAINT team_user_updated_by_user_id_fkey FOREIGN KEY (updated_by_user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.team_user
    ADD CONSTRAINT team_user_user_id_fkey FOREIGN KEY (user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot."user"
    ADD CONSTRAINT user_active_toggle_by_user_id_fkey FOREIGN KEY (active_toggle_by_user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.user_auth
    ADD CONSTRAINT user_auth_active_toggle_by_user_id_fkey FOREIGN KEY (active_toggle_by_user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.user_auth
    ADD CONSTRAINT user_auth_created_by_user_id_fkey FOREIGN KEY (created_by_user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.user_auth
    ADD CONSTRAINT user_auth_updated_by_user_id_fkey FOREIGN KEY (updated_by_user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.user_auth
    ADD CONSTRAINT user_auth_user_id_fkey FOREIGN KEY (user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot."user"
    ADD CONSTRAINT user_created_by_user_id_fkey FOREIGN KEY (created_by_user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot."user"
    ADD CONSTRAINT user_updated_by_user_id_fkey FOREIGN KEY (updated_by_user_id) REFERENCES hot."user"(user_id) ON UPDATE RESTRICT ON DELETE RESTRICT;



ALTER TABLE ONLY hot.webhook
    ADD CONSTRAINT webhook_build_id_fkey FOREIGN KEY (build_id) REFERENCES hot.build(build_id) ON UPDATE RESTRICT ON DELETE RESTRICT;

-- Generic reference data.

INSERT INTO org_user_role (org_user_role_id, role, sort_order) VALUES
    ('1', 'member', '1'),
    ('2', 'admin', '2');

INSERT INTO team_user_role (team_user_role_id, role, sort_order) VALUES
    ('1', 'member', '1'),
    ('2', 'admin', '2');

INSERT INTO invite_status (invite_status_id, status, sort_order) VALUES
    ('1', 'invited', '1'),
    ('2', 'joined', '2'),
    ('3', 'declined', '3');

INSERT INTO build_type (build_type_id, build_type, sort_order) VALUES
    ('1', 'bundle', '1'),
    ('2', 'live', '2');

INSERT INTO run_status (status_id, status, sort_order) VALUES
    ('1', 'running', '1'),
    ('2', 'succeeded', '2'),
    ('3', 'failed', '3'),
    ('4', 'cancelled', '4'),
    ('5', 'pending_retry', '5');

INSERT INTO run_type (run_type_id, run_type, sort_order) VALUES
    ('1', 'call', '1'),
    ('2', 'event', '2'),
    ('3', 'schedule', '3'),
    ('4', 'run', '4'),
    ('5', 'eval', '5'),
    ('6', 'repl', '6'),
    ('7', 'task', '7');

INSERT INTO scheduler_state (scheduler_id, last_successful_sync_time, updated_at) VALUES
    ('main', '2026-04-25 00:29:35.780331+00', '2026-04-25 00:29:35.780331+00');

INSERT INTO alert_destination_type (type_id, type_name, sort_order) VALUES
    ('1', 'email', '1'),
    ('2', 'slack', '2'),
    ('3', 'pagerduty', '3'),
    ('4', 'webhook', '4');

INSERT INTO alert_channel (alert_channel_id, org_id, env_id, name, pattern, enabled, created_at, updated_at, created_by_user_id, updated_by_user_id) VALUES
    ('02a85a9c-8c71-495c-a17f-8260e14e5a4a', NULL, NULL, 'run:cancelled', 'run:cancelled', 'true', '2026-04-25 00:29:36.022137+00', '2026-04-25 00:29:36.022137+00', NULL, NULL),
    ('1e7c2f7b-89ca-489d-b195-7f2042665bb9', NULL, NULL, 'deploy:failed', 'deploy:failed', 'true', '2026-04-25 00:29:36.022137+00', '2026-04-25 00:29:36.022137+00', NULL, NULL),
    ('424864cd-bec5-4065-92f8-a684d2745dd2', NULL, NULL, 'run:failed', 'run:failed', 'true', '2026-04-25 00:29:36.022137+00', '2026-04-25 00:29:36.022137+00', NULL, NULL),
    ('a788ff3e-55ab-4a93-af9b-c24e9ba10e23', NULL, NULL, 'deploy:succeeded', 'deploy:succeeded', 'true', '2026-04-25 00:29:36.022137+00', '2026-04-25 00:29:36.022137+00', NULL, NULL);

INSERT INTO alert_delivery_status (status_id, status_name, sort_order) VALUES
    ('1', 'pending', '1'),
    ('2', 'sent', '2'),
    ('3', 'failed', '3'),
    ('4', 'retrying', '4');

INSERT INTO org_plan_status (status_id, status, sort_order) VALUES
    ('1', 'active', '1'),
    ('2', 'inactive', '2'),
    ('3', 'pending', '3');

INSERT INTO email_verification_status (status_id, status, sort_order) VALUES
    ('1', 'pending', '1'),
    ('2', 'verified', '2'),
    ('3', 'expired', '3');

INSERT INTO task_status (task_status_id, name) VALUES
    ('1', 'queued'),
    ('2', 'running'),
    ('3', 'completed'),
    ('4', 'failed'),
    ('5', 'cancelled'),
    ('6', 'timed_out');
