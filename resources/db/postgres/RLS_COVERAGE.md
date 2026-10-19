# PostgreSQL scope policy coverage

Migration 011 installs and forces RLS. Mutation policies are separate from read policies.
The scope settings are trusted backend context, not customer SQL credentials.

| Table | Ownership |
| --- | --- |
| `org` | Explicit policy in migration 011 |
| `env` | Explicit policy in migration 011 |
| `org_note` | Explicit policy in migration 011 |
| `team` | Explicit policy in migration 011 |
| `invite` | Explicit policy in migration 011 |
| `org_user` | Explicit policy in migration 011 |
| `org_usage` | Explicit policy in migration 011 |
| `org_plan` | Explicit policy in migration 011 |
| `org_child_provisioning` | Explicit policy in migration 011 |
| `api_key` | Explicit policy in migration 011 |
| `service_key` | Explicit policy in migration 011 |
| `session` | Explicit policy in migration 011 |
| `project` | Explicit policy in migration 011 |
| `domain` | Explicit policy in migration 011 |
| `run` | Explicit policy in migration 011 |
| `task` | Explicit policy in migration 011 |
| `event` | Explicit policy in migration 011 |
| `stream` | Explicit policy in migration 011 |
| `access` | Explicit policy in migration 011 |
| `agent` | Explicit policy in migration 011 |
| `workflow` | Explicit policy in migration 011 |
| `build` | Explicit policy in migration 011 |
| `call` | Explicit policy in migration 011 |
| `event_handler` | Explicit policy in migration 011 |
| `mcp_tool` | Explicit policy in migration 011 |
| `webhook` | Explicit policy in migration 011 |
| `schedule` | Explicit policy in migration 011 |
| `schedule_log` | Explicit policy in migration 011 |
| `team_user` | Explicit policy in migration 011 |
| `alert_delivery` | Explicit policy in migration 011 |
| `alert_subscription_channel` | Explicit policy in migration 011 |
| `alert_subscription_destination` | Explicit policy in migration 011 |
| `file` | Explicit policy in migration 011 |
| `file_upload` | Explicit policy in migration 011 |
| `blob_object` | Explicit policy in migration 011 |
| `blob_ref` | Explicit policy in migration 011 |
| `alert_subscription` | Explicit policy in migration 011 |
| `store_map` | Explicit policy in migration 011 |
| `store_map_entry` | Explicit policy in migration 011 |
| `context` | Explicit policy in migration 011 |
| `alert` | Explicit policy in migration 011 |
| `alert_destination` | Explicit policy in migration 011 |
| `alert_channel` | Explicit policy in migration 011 |
| `user` | Explicit policy in migration 011 |
| `user_auth` | Explicit policy in migration 011 |
| `email_verification` | Explicit policy in migration 011 |
| `email_queue` | Explicit policy in migration 011 |
| `scheduler_state` | Explicit policy in migration 011 |

Reference tables deliberately have no RLS; runtime receives SELECT only:
`build_type`, `email_verification_status`, `invite_status`, `org_user_role`,
`run_status`, `run_type`, `task_status`, `team_user_role`, `org_plan_status`,
`alert_delivery_status`, `alert_destination_type`, `plan`.

SQLx migration metadata is deployment control state and receives no runtime grants.
Management credential tables introduced by migration 012 carry their own policies.
