use postgres::{Client, NoTls};

/// A fresh schema on the server named by AGENTSCM_TEST_DATABASE_URL, dropped on drop.
pub struct PgSchema {
    /// Connects with the schema as search_path and a unique application_name.
    pub url: String,
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
        let mut admin = Client::connect(&base, NoTls).unwrap();
        admin
            .batch_execute(&format!("CREATE SCHEMA {schema}"))
            .unwrap();
        let separator = if base.contains('?') { '&' } else { '?' };
        Some(Self {
            url: format!(
                "{base}{separator}options=-c%20search_path%3D{schema}&application_name={schema}"
            ),
            schema,
            base,
        })
    }

    /// A connection to the server's base database, outside the schema.
    #[allow(dead_code)] // only the conformance suite uses it
    pub fn admin(&self) -> Client {
        Client::connect(&self.base, NoTls).unwrap()
    }
}

impl Drop for PgSchema {
    fn drop(&mut self) {
        if let Ok(mut client) = Client::connect(&self.base, NoTls) {
            let _ = client.batch_execute(&format!("DROP SCHEMA {} CASCADE", self.schema));
        }
    }
}
