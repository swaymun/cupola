use super::*;

#[derive(
    Clone, Copy, Debug, Default, Eq, PartialEq, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum AgentFamily {
    #[default]
    Codex,
    Claude,
    /// OpenCode through the Agent Client Protocol (experimental).
    OpenCode,
    /// Cursor Agent through the Agent Client Protocol.
    Cursor,
}
impl AgentFamily {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::OpenCode => "opencode",
            Self::Cursor => "cursor",
        }
    }
    pub(crate) fn from_storage(value: &str) -> Result<Self, sqlx::Error> {
        match value {
            "codex" => Ok(Self::Codex),
            "claude" => Ok(Self::Claude),
            "opencode" => Ok(Self::OpenCode),
            "cursor" => Ok(Self::Cursor),
            _ => Err(sqlx::Error::Protocol("Unknown runtime agent family".into())),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeBinding {
    pub conversation_id: String,
    pub family: AgentFamily,
    pub thread_id: String,
    pub session_id: Option<String>,
    /// Project-owned provider execution scope.
    pub execution_scope: String,
}

impl Store {
    pub async fn runtime_binding(
        &self,
        conversation: &str,
    ) -> Result<Option<RuntimeBinding>, sqlx::Error> {
        let row = sqlx::query("SELECT * FROM runtime_bindings WHERE conversation_id=?")
            .bind(conversation)
            .fetch_optional(&self.pool)
            .await?;
        row.map(|row| {
            Ok(RuntimeBinding {
                conversation_id: row.get("conversation_id"),
                family: AgentFamily::from_storage(row.get("agent_family"))?,
                thread_id: row.get("runtime_thread_id"),
                session_id: row.get("session_id"),
                execution_scope: row.get("execution_scope"),
            })
        })
        .transpose()
    }

    /// A route is valid only while its native binding and every child link
    /// still belong to the included Project's current provider store and roots.
    pub async fn verified_project_runtime_owner(
        &self,
        conversation: &str,
        thread: &str,
    ) -> Result<bool, sqlx::Error> {
        // Membership and bindings must come from one SQLite view: a removed
        // folder cannot survive as an owner between separate reads.
        sqlx::query_scalar(concat!(
            "WITH owner AS (",
            crate::questions::verified_subagent_root_sql!(),
            ") SELECT EXISTS(SELECT 1 FROM runtime_bindings b JOIN conversations c ON c.id=b.conversation_id AND c.codex_thread_id=b.runtime_thread_id JOIN owner o JOIN runtime_bindings root ON root.conversation_id=o.conversation_id JOIN conversations rc ON rc.id=root.conversation_id AND rc.codex_thread_id=root.runtime_thread_id JOIN project_conversations pc ON pc.conversation_id=root.conversation_id AND pc.agent_family=root.agent_family AND pc.provider_store=root.provider_store WHERE b.conversation_id=? AND b.runtime_thread_id=? AND b.agent_family=root.agent_family AND b.provider_store=root.provider_store AND b.execution_scope='projects' AND root.execution_scope='projects')"
        ))
            .bind(conversation)
            .bind(conversation)
            .bind(thread)
            .fetch_one(&self.pool)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Contract: upgrading a populated host admits Cursor without losing the
    // owner's Projects, account selections, provider history or child links.
    #[tokio::test]
    async fn cursor_migration_preserves_existing_provider_state() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let migrations = sqlx::migrate!();
        let previous = sqlx::migrate::Migrator::with_migrations(
            migrations
                .iter()
                .filter(|m| m.version < 115)
                .cloned()
                .collect(),
        );
        previous.run(&pool).await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("PRAGMA foreign_keys")
                .fetch_one(&pool)
                .await
                .unwrap(),
            1
        );
        sqlx::raw_sql(r#"
INSERT INTO conversations(id,created_at) VALUES ('parent','then'),('child','then');
INSERT INTO devices(id,label,role,public_key_jwk,created_at) VALUES ('owner','Owner','owner','{}','then');
INSERT INTO messages(id,device_id,client_message_id,body_sha256,conversation_id,state,created_at) VALUES ('message','owner','client','hash','parent','completed','then');
INSERT INTO projects(id,name,primary_root_id,last_family,created_at,updated_at) VALUES ('project','Kept Project','root','claude','then','then');
INSERT INTO project_roots(id,project_id,path,canonical_path,ordinal) VALUES ('root','project','/work','/work',0);
INSERT INTO project_conversations(conversation_id,project_id,agent_family,provider_store,cwd,roots_revision,title,created_at,updated_at,last_activity_at)
 VALUES ('parent','project','claude','claude:home','/work',1,'Parent','then','then','then'),('child','project','codex','codex:home','/work',1,'Child','then','then','then');
INSERT INTO runtime_bindings VALUES ('parent','claude','claude-thread','native-session','projects','claude:home','then');
INSERT INTO project_provider_refs VALUES ('project','codex','codex:home','native-project');
INSERT INTO provider_accounts VALUES ('account','claude','Work','claude:work','work-home','then');
INSERT INTO project_provider_accounts VALUES ('project','claude','account');
INSERT INTO provider_settings VALUES ('claude','file_checkpointing',1,'then');
INSERT INTO agent_family_settings(agent_family,instruction,updated_at) VALUES ('claude','Retain this instruction','then');
INSERT INTO message_execution_settings(message_id,model,agent_family) VALUES ('message','claude:sonnet','claude');
INSERT INTO project_message_targets VALUES ('owner','queued','codex','gpt-fixture',NULL,NULL,'then');
INSERT INTO project_runtime_history(id,conversation_id,agent_family,provider_store,native_session_id,switched_at) VALUES (5,'parent','codex','codex:home','old-session','then'),(50,'parent','codex','codex:home','deleted-session','then');
DELETE FROM project_runtime_history WHERE id=50;
INSERT INTO project_pending_handoff VALUES ('parent',5,NULL,'then');
INSERT INTO project_handoff_delivery VALUES ('parent','claude-thread','injected','then');
INSERT INTO project_conversation_forks VALUES ('child','parent','turn-before-fork');
INSERT INTO thread_delegations(child_conversation_id,parent_conversation_id,client_request_id,device_id,created_at,wake_message_id,observed_at) VALUES ('child','parent','delegation','owner','then',NULL,NULL);
"#).execute(&pool).await.unwrap();
        let tables = [
            "projects",
            "project_roots",
            "project_conversations",
            "runtime_bindings",
            "project_provider_refs",
            "provider_accounts",
            "project_provider_accounts",
            "provider_settings",
            "agent_family_settings",
            "message_execution_settings",
            "project_message_targets",
            "project_runtime_history",
            "project_pending_handoff",
            "project_handoff_delivery",
            "project_conversation_forks",
            "thread_delegations",
        ];
        async fn columns(pool: &SqlitePool, table: &str) -> String {
            sqlx::query(sqlx::AssertSqlSafe(format!("PRAGMA table_info({table})")))
                .fetch_all(pool)
                .await
                .unwrap()
                .into_iter()
                .map(|r| r.get::<String, _>("name"))
                .collect::<Vec<_>>()
                .join(",")
        }
        // Compares the columns the upgrade started with: a later migration
        // may add a column, but no existing value may change.
        async fn snapshot(pool: &SqlitePool, tables: &[(&str, String)]) -> Vec<String> {
            let mut rows = Vec::new();
            for (table, columns) in tables {
                rows.push(sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                    "SELECT json_group_array(json_array({columns})) FROM (SELECT * FROM {table} ORDER BY 1)")))
                    .fetch_one(pool).await.unwrap());
            }
            rows
        }
        let mut tables_before = Vec::new();
        for table in tables {
            tables_before.push((table, columns(&pool, table).await));
        }
        let before = snapshot(&pool, &tables_before).await;
        assert!(
            sqlx::query("UPDATE projects SET last_family='cursor' WHERE id='project'")
                .execute(&pool)
                .await
                .is_err()
        );
        // This contract owns migration 115; later migrations intentionally
        // retire the added-account tables after preserving the thread identities.
        sqlx::migrate::Migrator::with_migrations(
            migrations
                .iter()
                .filter(|m| m.version <= 115)
                .cloned()
                .collect(),
        )
        .run(&pool)
        .await
        .unwrap();
        assert_eq!(snapshot(&pool, &tables_before).await, before);
        assert!(sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(&pool)
            .await
            .unwrap()
            .is_empty());
        sqlx::query("INSERT INTO project_runtime_history(conversation_id,agent_family,provider_store,switched_at) VALUES ('parent','cursor','cursor','now')")
            .execute(&pool).await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT max(id) FROM project_runtime_history")
                .fetch_one(&pool)
                .await
                .unwrap(),
            51
        );
        sqlx::raw_sql(r#"
UPDATE projects SET last_family='cursor' WHERE id='project';
INSERT INTO conversations(id,created_at) VALUES ('cursor-chat','now');
INSERT INTO project_conversations(conversation_id,project_id,agent_family,provider_store,cwd,roots_revision,title,model,created_at,updated_at,last_activity_at)
 VALUES ('cursor-chat','project','cursor','cursor','/work',1,'Cursor','cursor:default[]','now','now','now');
INSERT INTO runtime_bindings VALUES ('cursor-chat','cursor','cursor-thread','cursor-session','projects','cursor','now');
UPDATE project_conversations SET model='cursor:gpt-6-luna[reasoning=low]' WHERE conversation_id='cursor-chat';
INSERT INTO project_provider_refs VALUES ('project','cursor','cursor','cursor-project');
INSERT INTO provider_accounts VALUES ('cursor-account','cursor','Future account','cursor:future','cursor-home','now');
INSERT INTO project_provider_accounts VALUES ('project','cursor','cursor-account');
INSERT INTO provider_settings VALUES ('cursor','desktop_computer_use',1,'now');
INSERT INTO agent_family_settings(agent_family,updated_at) VALUES ('cursor','now');
UPDATE message_execution_settings SET agent_family='cursor',model='cursor:default[]' WHERE message_id='message';
INSERT INTO project_message_targets VALUES ('owner','cursor-queued','cursor','cursor:default[]',NULL,NULL,'now');
"#).execute(&pool).await.unwrap();
        // Widening the family constraint must preserve native policy, identity
        // and namespace guards, including refusing a caller's invented family.
        for sql in [
            "UPDATE project_conversations SET access_mode='full_access' WHERE conversation_id='cursor-chat'",
            "UPDATE project_conversations SET agent_family='codex' WHERE conversation_id='cursor-chat'",
            "UPDATE project_conversations SET model='claude:sonnet' WHERE conversation_id='cursor-chat'",
            "UPDATE runtime_bindings SET agent_family='codex' WHERE conversation_id='cursor-chat'",
            "UPDATE projects SET last_family='unknown' WHERE id='project'",
        ] { assert!(sqlx::query(sqlx::AssertSqlSafe(sql)).execute(&pool).await.is_err(), "{sql}"); }
        eprintln!("cursor migration preserved 16 populated tables, all Cursor inserts, sequence high-water=51 and policy/identity guards; checking provider readers");
        let store = Store { pool: pool.clone() };
        assert_eq!(
            store
                .runtime_binding("cursor-chat")
                .await
                .unwrap()
                .unwrap()
                .family,
            AgentFamily::Cursor
        );
        assert_eq!(
            store
                .project_conversation("cursor-chat")
                .await
                .unwrap()
                .unwrap()
                .family,
            AgentFamily::Cursor
        );
        migrations.run(&pool).await.unwrap(); // Reopening is idempotent.
    }
    // Contract: removing added accounts keeps user content and working choices,
    // including the original store identity of both added and normal threads.
    #[tokio::test]
    async fn removing_accounts_preserves_threads_and_project_provider_models() {
        let dir = tempfile::tempdir().unwrap();
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(dir.path().join("fixture.sqlite"))
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options.clone())
            .await
            .unwrap();
        let migrations = sqlx::migrate!();
        sqlx::migrate::Migrator::with_migrations(
            migrations
                .iter()
                .filter(|m| m.version < 144)
                .cloned()
                .collect(),
        )
        .run(&pool)
        .await
        .unwrap();
        sqlx::raw_sql(r#"
INSERT INTO projects(id,creation_request_id,creation_payload_sha256,name,primary_root_id,roots_revision,last_family,created_at,updated_at)
 VALUES ('project','request','hash','Work','root',1,'claude','then','then');
INSERT INTO project_roots(id,project_id,path,canonical_path,ordinal) VALUES ('root','project','/work','/work',0);
INSERT INTO project_last_models(project_id,agent_family,model,effort) VALUES ('project','codex','gpt-fixture','high'),('project','claude','claude:sonnet',NULL);
INSERT INTO conversations(id,created_at) VALUES ('added-chat','then'),('normal-chat','then');
INSERT INTO project_conversations(conversation_id,project_id,agent_family,provider_store,native_session_id,cwd,roots_revision,title,model,created_at,updated_at,last_activity_at)
 VALUES ('added-chat','project','claude','claude:added','claude-added','/work',1,'Added history','claude:sonnet','then','then','then'),
 ('normal-chat','project','codex','codex:home','normal-thread','/work',1,'Normal history','gpt-fixture','then','then','then');
INSERT INTO runtime_bindings(conversation_id,agent_family,runtime_thread_id,session_id,execution_scope,provider_store,updated_at) VALUES ('added-chat','claude','claude-added','native-added','projects','claude:added','then'),('normal-chat','codex','normal-thread',NULL,'projects','codex:home','then');
INSERT INTO devices(id,label,role,public_key_jwk,created_at) VALUES ('fixture','Fixture','owner','{}','then');
INSERT INTO messages(id,conversation_id,device_id,client_message_id,body_sha256,state,body,created_at) VALUES ('added-message','added-chat','fixture','added-request','hash','completed','Keep the added thread text','then'),('normal-message','normal-chat','fixture','normal-request','hash','completed','Keep the normal thread text','then');
INSERT INTO provider_accounts VALUES ('added','claude','Work sign-in','claude:added','added-home','then');
INSERT INTO project_provider_accounts VALUES ('project','claude','added');
"#).execute(&pool).await.unwrap();
        let tables = [
            "projects",
            "project_roots",
            "project_last_models",
            "conversations",
            "project_conversations",
            "runtime_bindings",
            "messages",
        ];
        async fn columns(pool: &sqlx::SqlitePool, tables: &[&str]) -> Vec<String> {
            let mut all = Vec::new();
            for table in tables {
                all.push(
                    sqlx::query(sqlx::AssertSqlSafe(format!("PRAGMA table_info({table})")))
                        .fetch_all(pool)
                        .await
                        .unwrap()
                        .iter()
                        .map(|r| r.get::<String, _>("name"))
                        .collect::<Vec<_>>()
                        .join(","),
                );
            }
            all
        }
        // Later migrations may add columns; compare only the columns present before.
        async fn snapshot(
            pool: &sqlx::SqlitePool,
            tables: &[&str],
            columns: &[String],
        ) -> Vec<String> {
            let mut all = Vec::new();
            for (table, columns) in tables.iter().zip(columns) {
                all.push(sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT json_group_array(json_array({columns})) FROM (SELECT {columns} FROM {table} ORDER BY 1)")))
                    .fetch_one(pool).await.unwrap());
            }
            all
        }
        let home = dir.path().join("provider-accounts/claude/added-home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("history.jsonl"),
            "Original provider-owned history",
        )
        .unwrap();
        let columns = columns(&pool, &tables).await;
        let before = snapshot(&pool, &tables, &columns).await;
        migrations.run(&pool).await.unwrap();
        assert_eq!(snapshot(&pool, &tables, &columns).await, before);
        for table in ["provider_accounts", "project_provider_accounts"] {
            assert_eq!(
                sqlx::query_scalar::<_, i64>("SELECT count(*) FROM sqlite_master WHERE name=?")
                    .bind(table)
                    .fetch_one(&pool)
                    .await
                    .unwrap(),
                0
            );
        }
        assert!(sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(&pool)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            std::fs::read_to_string(home.join("history.jsonl")).unwrap(),
            "Original provider-owned history"
        );
        pool.close().await;
        let reopened = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        migrations.run(&reopened).await.unwrap();
        assert_eq!(snapshot(&reopened, &tables, &columns).await, before);
    }
}
