// RE-VERIFY NIT: `ConfigSaveGuard`'s private constructor is only a rail
// while this crate cannot spell `unsafe` — `mem::zeroed` forges any
// witness in one line with no warning. dbc-ui already forbids it; the
// witness is DEFINED here, so the forge was reachable here even though
// no dbc-ui code could trigger it.
//
// `forbid`, not `deny`: `deny` is undone by an `#[allow(unsafe_code)]` on
// the offending item, which is one line and no warning. dbc-state
// contains no `unsafe` today, so this costs nothing and makes adding any
// later a deliberate, visible act.
#![forbid(unsafe_code)]

mod config;
pub use config::{
    default_config_path, AppConfig, ConfigSaveGuard, ConfigVerdict, ConnectionConfig, Engine,
    FavouriteObject, MssqlOptions, SshTunnelConfig, StateError, ThemeMode, TreeGrouping,
};

pub mod applog;
pub mod fsutil;
pub mod schema_cache;
pub mod session;
pub mod workspace;

mod scope;
pub use scope::connection_scope_key;

mod vault;
pub use vault::{default_vault_path, Vault};

mod history;
pub use history::{default_history_path, HistoryDb, HistoryEntry};

mod view_prefs;
pub use view_prefs::{default_view_prefs_path, TableViewPrefs, ViewPrefsStore};

mod params;
pub use params::{default_param_values_path, ParamValue, ParamValuesStore};
