use postgres::{Client, NoTls};

/// A fresh database on the server named by AGENTSCM_TEST_DATABASE_URL, holding one fresh
/// schema, dropped on drop. The store's advisory locks are database-wide, so a database per
/// test keeps parallel tests from waiting on each other's locks.
pub struct PgSchema {
    /// Connects to the test database with the schema as search_path and a unique
    /// application_name.
    pub url: String,
    /// Names the schema, the database and the connections' application_name.
    pub schema: String,
    base: String,
}

impl PgSchema {
    /// None (after saying why) when no server is configured; panics under CI.
    pub fn create() -> Option<Self> {
        let Some(base) = std::env::var("AGENTSCM_TEST_DATABASE_URL")
            .ok()
            .filter(|url| !url.is_empty())
        else {
            assert!(
                !matches!(std::env::var("CI"), Ok(v) if !v.is_empty() && v != "false"),
                "AGENTSCM_TEST_DATABASE_URL must be set in CI"
            );
            eprintln!("skipping Postgres conformance: AGENTSCM_TEST_DATABASE_URL unset");
            return None;
        };
        let mut random = [0_u8; 8];
        getrandom::fill(&mut random).unwrap();
        let schema = format!(
            "hub_t_{}",
            random
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        Client::connect(&base, NoTls)
            .unwrap()
            .batch_execute(&format!("CREATE DATABASE {schema}"))
            .expect("the test role must be able to create databases");
        let mut database = reqwest::Url::parse(&base).unwrap();
        database.set_path(&format!("/{schema}"));
        let separator = if database.query().is_some() { '&' } else { '?' };
        // Built before CREATE SCHEMA, so that its Drop removes the database if that fails.
        let pg = Self {
            url: format!(
                "{database}{separator}options=-c%20search_path%3D{schema}&application_name={schema}"
            ),
            schema,
            base,
        };
        Client::connect(database.as_str(), NoTls)
            .unwrap()
            .batch_execute(&format!("CREATE SCHEMA {}", pg.schema))
            .unwrap();
        Some(pg)
    }

    /// A connection to the server's base database, outside the test database.
    #[allow(dead_code)] // only the conformance suite uses it
    pub fn admin(&self) -> Client {
        Client::connect(&self.base, NoTls).unwrap()
    }
}

impl Drop for PgSchema {
    fn drop(&mut self) {
        if let Ok(mut client) = Client::connect(&self.base, NoTls) {
            // FORCE ends connections the test left open, which would otherwise block the drop.
            let _ = client.batch_execute(&format!("DROP DATABASE {} WITH (FORCE)", self.schema));
        }
    }
}
