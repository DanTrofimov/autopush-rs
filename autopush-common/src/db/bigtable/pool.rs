use std::time::Instant;
use std::{fmt, sync::Arc};

use async_trait::async_trait;
use cadence::StatsdClient;
use deadpool::managed::{Manager, PoolConfig, Timeouts};
use grpcio::{Channel, ChannelBuilder, ChannelCredentials, EnvBuilder};

use crate::db::bigtable::{bigtable_client::BigtableDb, BigTableDbSettings, BigTableError};
use crate::db::error::{DbError, DbResult};
use crate::db::DbSettings;

use super::bigtable_client::error;

const MAX_MESSAGE_LEN: i32 = 1 << 28; // 268,435,456 bytes
const DEFAULT_GRPC_PORT: u16 = 8086;

/// The pool of BigTable Clients.
/// Note: BigTable uses HTTP/2 as the backbone, so the only really important bit
/// that we have control over is the "channel". For now, we're using the ClientManager to
/// create new Bigtable clients, which have channels associated with them.
/// The Manager also has the ability to return a channel, which is useful for
/// Bigtable administrative calls, which use their own channel.
#[derive(Clone)]
pub struct BigTablePool {
    /// Pool of db connections
    pub pool: deadpool::managed::Pool<BigtableClientManager>,
    _metrics: Arc<StatsdClient>,
}

impl fmt::Debug for BigTablePool {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("SpannerDbPool").finish()
    }
}

/// Several convenience functions for using the pool.
impl BigTablePool {
    /// Get a new managed object from the pool.
    pub async fn get(
        &self,
    ) -> Result<deadpool::managed::Object<BigtableClientManager>, error::BigTableError> {
        let obj = self
            .pool
            .get()
            .await
            .map_err(|e| error::BigTableError::Pool(e.to_string()))?;
        debug!("🉑 Got db from pool");
        Ok(obj)
    }

    /// Get the pools manager, because we would like to talk to them.
    pub fn get_channel(&self) -> Result<Channel, BigTableError> {
        self.pool.manager().get_channel()
    }

    /// Creates a new pool of BigTable db connections.
    pub fn new(settings: &DbSettings, metrics: &Arc<StatsdClient>) -> DbResult<Self> {
        let Some(endpoint) = &settings.dsn else {
            return Err(DbError::ConnectionError(
                "No DSN specified in settings".to_owned(),
            ));
        };
        let bt_settings = BigTableDbSettings::try_from(settings.db_settings.as_str())?;
        debug!("🉑 DSN: {}", &endpoint);
        // Url::parsed() doesn't know how to handle `grpc:` schema, so it returns "null".
        let parsed = url::Url::parse(endpoint).map_err(|e| {
            DbError::ConnectionError(format!("Invalid DSN: {:?} : {:?}", endpoint, e))
        })?;
        let connection = format!(
            "{}:{}",
            parsed
                .host_str()
                .ok_or_else(|| DbError::ConnectionError(format!(
                    "Invalid DSN: Unparsable host {:?}",
                    endpoint
                )))?,
            parsed.port().unwrap_or(DEFAULT_GRPC_PORT)
        );
        // Make sure the path is empty.
        if !parsed.path().is_empty() {
            return Err(DbError::ConnectionError(format!(
                "Invalid DSN: Table paths belong in AUTO*_DB_SETTINGS `tab: {:?}",
                endpoint
            )));
        }
        debug!("🉑 connection string {}", &connection);

        // Construct a new manager and put them in a pool for handling future requests.
        let manager =
            BigtableClientManager::new(&bt_settings, settings.dsn.clone(), connection.clone())?;
        let mut config = PoolConfig::default();
        if let Some(size) = bt_settings.database_pool_max_size {
            debug!("🏊 Setting pool max size {}", &size);
            config.max_size = size as usize;
        };
        if !bt_settings.database_pool_connection_timeout.is_zero() {
            debug!(
                "🏊 Setting connection timeout to {} seconds",
                &bt_settings.database_pool_connection_timeout.as_secs()
            );
            config.timeouts = Timeouts {
                create: Some(bt_settings.database_pool_connection_timeout),
                ..Default::default()
            };
        }
        let pool = deadpool::managed::Pool::builder(manager)
            .config(config)
            .build()
            .map_err(|e| DbError::BTError(BigTableError::Pool(e.to_string())))?;

        Ok(Self {
            pool,
            _metrics: metrics.clone(),
        })
    }
}

/// BigTable Pool Manager. This contains everything needed to create a new connection.
pub struct BigtableClientManager {
    settings: BigTableDbSettings,
    dsn: Option<String>,
    connection: String,
}

impl BigtableClientManager {
    fn new(
        settings: &BigTableDbSettings,
        dsn: Option<String>,
        connection: String,
    ) -> Result<Self, DbError> {
        Ok(Self {
            settings: settings.clone(),
            dsn,
            connection,
        })
    }
}

impl fmt::Debug for BigtableClientManager {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("deadpool::BtClientManager")
            .field("settings", &self.settings.clone())
            .finish()
    }
}

#[async_trait]
impl Manager for BigtableClientManager {
    type Error = DbError;
    type Type = BigtableDb;

    /// Create a new Bigtable Client with it's own channel.
    /// `BigtableClient` is the most atomic we can go.
    async fn create(&self) -> Result<BigtableDb, DbError> {
        debug!("🏊 Create a new pool entry.");
        let entry = BigtableDb::new(self.get_channel()?, &self.settings.metadata()?);
        debug!("🏊 Bigtable connection acquired");
        Ok(entry)
    }

    /// Recycle if the connection has outlived it's lifespan.
    async fn recycle(
        &self,
        client: &mut Self::Type,
        metrics: &deadpool::managed::Metrics,
    ) -> deadpool::managed::RecycleResult<Self::Error> {
        if !self.settings.database_pool_connection_ttl.is_zero()
            && Instant::now() - metrics.created > self.settings.database_pool_connection_ttl
        {
            debug!("🏊 Recycle requested (old).");
            return Err(DbError::BTError(BigTableError::Recycle).into());
        }
        if !self.settings.database_pool_max_idle.is_zero() {
            if let Some(recycled) = metrics.recycled {
                if Instant::now() - recycled > self.settings.database_pool_max_idle {
                    debug!("🏊 Recycle requested (idle).");
                    return Err(DbError::BTError(BigTableError::Recycle).into());
                }
            }
        }

        // Clippy 0.1.73 complains about the `.map_err` being hard to read.
        // note, this changes to `blocks_in_conditions` for 1.76+
        #[allow(clippy::blocks_in_conditions)]
        if !client
            .health_check(&self.settings.table_name)
            .await
            .map_err(|e| {
                debug!("🏊 Recycle requested (health). {:?}", e);
                DbError::BTError(BigTableError::Recycle)
            })?
        {
            debug!("🏊 Health check failed");
            return Err(DbError::BTError(BigTableError::Recycle).into());
        }

        // Bigtable does not offer a simple health check. A read or write operation would
        // need to be performed.

        Ok(())
    }
}

impl BigtableClientManager {
    /// Get a new Channel, based on the application settings.
    pub fn get_channel(&self) -> Result<Channel, BigTableError> {
        Ok(Self::create_channel(self.dsn.clone())?.connect(self.connection.as_str()))
    }
    /// Channels are GRPCIO constructs that contain the actual command data paths.
    /// Channels seem to be fairly light weight.
    pub fn create_channel(dsn: Option<String>) -> Result<ChannelBuilder, BigTableError> {
        debug!("🏊 Creating new channel...");
        let mut chan = ChannelBuilder::new(Arc::new(EnvBuilder::new().build()))
            .max_send_message_len(MAX_MESSAGE_LEN)
            .max_receive_message_len(MAX_MESSAGE_LEN);
        // Don't get the credentials if we are running in the emulator
        if dsn.map(|v| v.contains("localhost")).unwrap_or(false)
            || std::env::var("BIGTABLE_EMULATOR_HOST").is_ok()
        {
            debug!("🉑 Using emulator");
        } else {
            chan = chan.set_credentials(ChannelCredentials::google_default_credentials().map_err(
                |e| {
                    BigTableError::Admin(
                        "Could not set credentials".to_owned(),
                        Some(e.to_string()),
                    )
                },
            )?);
            debug!("🉑 Using real");
        }
        Ok(chan)
    }
}
