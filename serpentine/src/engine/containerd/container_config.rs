//! Holds the runtime container and service types.

use std::collections::BTreeMap;
use std::hash::Hash;
use std::sync::Arc;
use std::time::Duration;

use serpentine_internal::network;
use typed_path::{UnixPath, UnixPathBuf};

/// Configuration for the container
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[cfg_attr(test, derive(bolero::TypeGenerator))]
pub struct ContainerConfig {
    /// Environment
    #[cfg_attr(test, generator(fuzz::env_map()))]
    pub(super) env: BTreeMap<Arc<str>, Arc<str>>,
    /// The working directory
    #[cfg_attr(test, generator(fuzz::unix_path()))]
    #[serde(with = "serpentine_internal::TypedPathBufRemote")]
    pub(super) working_dir: UnixPathBuf,
    /// The user to spawn the process as.
    ///
    /// This is stored in the same format as the oci image spec for linux.
    /// > `user`, `uid`, `user:group`, `uid:gid`, `uid:group`, `user:gid`
    #[cfg_attr(test, generator(fuzz::opt_arc_str()))]
    pub(super) user: Option<Arc<str>>,
    /// The services to attach to this container.
    // TODO: Should this generate services?
    #[cfg_attr(test, generator(bolero::constant(BTreeMap::new())))]
    pub(super) services: BTreeMap<Arc<str>, ServiceState>,
}

impl ContainerConfig {
    /// Get an environment variable in the container
    pub fn get_env_var(&self, env: &str) -> Option<&Arc<str>> {
        self.env.get(env)
    }

    /// Resolve `dir` against the current working directory of the container.
    ///
    /// Joining rather than replacing is what makes `WorkingDir("./a") > WorkingDir("./b")` land in
    /// `a/b`; an absolute `dir` still replaces the whole path.
    pub fn set_working_dir(&mut self, dir: &UnixPath) {
        self.working_dir = self.working_dir.join(dir);
    }

    /// Set an environment variable in the container
    pub fn set_env_var(&mut self, env: Arc<str>, value: Arc<str>) {
        self.env.insert(env, value);
    }

    /// Update the user config for the container
    pub fn set_user(&mut self, user: Arc<str>) {
        self.user = Some(user);
    }

    /// Attach service to this container
    pub fn with_service(&mut self, service: ServiceState, hostname: Arc<str>) {
        self.services.insert(hostname, service);
    }
}

impl From<oci_client::config::Config> for ContainerConfig {
    fn from(config: oci_client::config::Config) -> Self {
        let env = config
            .env
            .unwrap_or_default()
            .into_iter()
            .filter_map(|env| {
                env.split_once('=')
                    .map(|(key, value)| (Arc::from(key), Arc::from(value)))
            })
            .collect();

        Self {
            env,
            working_dir: config.working_dir.map_or_else(
                || UnixPath::new("/").to_path_buf(),
                |dir| UnixPath::new(&dir).to_path_buf(),
            ),
            user: config.user.map(Arc::from),
            services: BTreeMap::new(),
        }
    }
}

/// Extra config values for services
#[derive(Clone, Eq, PartialEq, Debug, Hash, serde::Serialize, serde::Deserialize)]
#[cfg_attr(test, derive(bolero::TypeGenerator))]
pub struct ServiceConfig {
    /// The service entry point
    #[cfg_attr(test, generator(fuzz::arc_str()))]
    pub(super) entrypoint: Arc<str>,
    /// Command to run in the same container as the service and which should return a 0 exit code
    /// before spawning parents.
    #[cfg_attr(test, generator(fuzz::healthcheck()))]
    pub(super) healthcheck: (Arc<str>, Duration),
}

impl From<oci_client::config::Config> for ServiceConfig {
    fn from(config: oci_client::config::Config) -> Self {
        let entrypoint = config
            .entrypoint
            .unwrap_or_default()
            .into_iter()
            .chain(config.cmd.unwrap_or_default())
            .collect::<Vec<_>>();
        let entrypoint = shell_words::join(entrypoint);

        Self {
            entrypoint: format!("exec {entrypoint}").into(),
            healthcheck: ("exit 0".into(), Duration::from_secs(1)),
        }
    }
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            entrypoint: "while true; do sleep 1; done".into(),
            healthcheck: ("exit 0".into(), Duration::from_secs(1)),
        }
    }
}

impl ServiceConfig {
    /// Set the healthcheck command and timeout for this service.
    pub fn set_healthcheck(&mut self, command: Arc<str>, timeout: Duration) {
        self.healthcheck = (command, timeout);
    }
}

/// A services state
#[derive(Clone, Eq, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
#[cfg_attr(test, derive(bolero::TypeGenerator))]
pub struct ServiceState {
    /// The underlying container
    pub(super) container: ContainerState,
    /// The service-specific config.
    pub(super) service_config: ServiceConfig,
}

impl ServiceState {
    /// Get a reference to the service config.
    pub fn get_service_config(&self) -> &ServiceConfig {
        &self.service_config
    }

    /// Update this states service config using a closure.
    ///
    /// This does not change the input state but instead returns a new one.
    pub fn update_service_config(&self, update: impl FnOnce(&mut ServiceConfig)) -> Self {
        let mut service_config = self.service_config.clone();
        update(&mut service_config);
        ServiceState {
            container: self.container.clone(),
            service_config,
        }
    }

    /// Convert this service into a container topology
    fn into_topology(mut self, hostname: Arc<str>) -> network::Topology<ContainerTopologyNode> {
        let mut services = Vec::with_capacity(self.container.config.services.len());

        self.container = self.container.update_config(|config| {
            for (service_hostname, service) in &config.services {
                services.push(service.clone().into_topology(Arc::clone(service_hostname)));
            }
        });

        let this = ContainerTopologyNode {
            state: ContainerLike::Service(self),
            hostname: Some(hostname),
            cmd: None,
        };

        network::Topology::with_children(this, services)
    }
}

impl std::ops::Deref for ServiceState {
    type Target = ContainerState;
    fn deref(&self) -> &ContainerState {
        &self.container
    }
}

impl std::ops::DerefMut for ServiceState {
    fn deref_mut(&mut self) -> &mut ContainerState {
        &mut self.container
    }
}

/// A reference to a specific state of a container.
#[derive(Clone, Eq, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
#[cfg_attr(test, derive(bolero::TypeGenerator))]
pub struct ContainerState {
    /// The snapshot to use for the container
    #[cfg_attr(test, generator(fuzz::arc_str()))]
    pub(super) snapshot: Arc<str>,
    /// The container config
    pub(super) config: ContainerConfig,
}

impl ContainerState {
    /// Get a reference to the config.
    pub fn get_config(&self) -> &ContainerConfig {
        &self.config
    }

    /// Update this states config using a closure.
    ///
    /// This does not change the input state but instead returns a new one.
    pub fn update_config(&self, update: impl FnOnce(&mut ContainerConfig)) -> Self {
        let mut config = self.config.clone();
        update(&mut config);
        ContainerState {
            snapshot: Arc::clone(&self.snapshot),
            config,
        }
    }

    /// Convert this container into a service
    pub fn into_service(self, entrypoint: Arc<str>) -> ServiceState {
        ServiceState {
            container: self,
            service_config: ServiceConfig {
                entrypoint,
                ..ServiceConfig::default()
            },
        }
    }

    /// Convert this into a container topology
    pub(super) fn into_topology(
        mut self,
        cmd: Box<str>,
    ) -> network::Topology<ContainerTopologyNode> {
        let mut services = Vec::with_capacity(self.config.services.len());

        self = self.update_config(|config| {
            for (service_hostname, service) in &config.services {
                services.push(service.clone().into_topology(Arc::clone(service_hostname)));
            }
        });

        let this = ContainerTopologyNode {
            state: ContainerLike::Container(self),
            hostname: None,
            cmd: Some(cmd),
        };

        network::Topology::with_children(this, services)
    }
}

#[cfg(test)]
impl ContainerState {
    /// Build a state from exact field values, for tests that need a fixed constant rather than a
    /// generated one.
    pub(crate) fn from_parts(snapshot: Arc<str>, config: ContainerConfig) -> Self {
        Self { snapshot, config }
    }
}

/// Either a container or a service.
///
/// Many operations (env, working dir, user, etc.) apply equally to both containers and services.
/// This type allows those operations to be written once and work on either.
#[derive(Clone, Eq, PartialEq, Debug)]
pub enum ContainerLike {
    /// A container
    Container(ContainerState),
    /// A service
    Service(ServiceState),
}

impl ContainerLike {
    /// Update this states config using a closure.
    ///
    /// This does not change the input state but instead returns a new one.
    pub fn update_config(&self, update: impl FnOnce(&mut ContainerConfig)) -> Self {
        match self {
            Self::Container(container) => Self::Container(container.update_config(update)),
            Self::Service(service) => Self::Service(ServiceState {
                container: service.container.update_config(update),
                service_config: service.service_config.clone(),
            }),
        }
    }
}

impl std::ops::Deref for ContainerLike {
    type Target = ContainerState;
    fn deref(&self) -> &ContainerState {
        match self {
            Self::Container(container) => container,
            Self::Service(service) => service,
        }
    }
}

impl std::ops::DerefMut for ContainerLike {
    fn deref_mut(&mut self) -> &mut ContainerState {
        match self {
            Self::Container(container) => container,
            Self::Service(service) => &mut *service,
        }
    }
}

/// A node in a container topology.
///
/// The root node will be a `ContainerState` and the children will be `ServiceState`s.
/// This is used to represent the full state of a container with all its attached services.
pub(super) struct ContainerTopologyNode {
    /// The service state
    pub(super) state: ContainerLike,
    /// The hostname of this service.
    pub(super) hostname: Option<Arc<str>>,
    /// The command provided to the root node.
    pub(super) cmd: Option<Box<str>>,
}

impl ContainerTopologyNode {
    /// Get the hostname to use for this container.
    pub(super) fn get_hostname(&self) -> Arc<str> {
        self.hostname.clone().unwrap_or_else(|| "step".into())
    }

    /// Get the command to execute
    pub(super) fn get_cmd(&self) -> &str {
        match (&self.state, &self.cmd) {
            (ContainerLike::Container(_), Some(cmd)) => cmd.as_ref(),
            (ContainerLike::Service(service), None) => &service.service_config.entrypoint,
            _ => {
                debug_assert!(
                    false,
                    "Only the root node should have a cmd, and it should be set"
                );
                log::error!("Invalid container topology: root node has no cmd");
                "/bin/sh"
            }
        }
    }
}

/// Field generators for fuzzing the container config types.
#[cfg(test)]
mod fuzz {
    use bolero::ValueGenerator as _;

    use super::*;
    pub(super) use crate::engine::data_model::fuzz::arc_str;

    /// Generator for an optional user string.
    pub(super) fn opt_arc_str() -> impl bolero::ValueGenerator<Output = Option<Arc<str>>> {
        bolero::produce::<Option<String>>().map_gen(|value| value.map(Arc::from))
    }

    /// Generator for small environment maps.
    pub(super) fn env_map() -> impl bolero::ValueGenerator<Output = BTreeMap<Arc<str>, Arc<str>>> {
        bolero::produce::<Vec<(String, String)>>()
            .with()
            .len(0..10_usize)
            .map_gen(|entries| {
                entries
                    .into_iter()
                    .map(|(key, value)| (Arc::from(key), Arc::from(value)))
                    .collect()
            })
    }

    /// Generator for arbitrary unix paths.
    pub(super) fn unix_path() -> impl bolero::ValueGenerator<Output = UnixPathBuf> {
        bolero::produce::<Vec<u8>>().map_gen(|bytes| UnixPath::new(&bytes).to_path_buf())
    }

    /// Generator for healthchecks, using whole seconds so the cache roundtrip is lossless.
    pub(super) fn healthcheck() -> impl bolero::ValueGenerator<Output = (Arc<str>, Duration)> {
        (
            arc_str(),
            bolero::produce::<u64>().map_gen(Duration::from_secs),
        )
    }
}
