//! Utilities for launching a Geth dev-mode instance.

use crate::{
    extract_endpoint, extract_value, unused_port, DevOptions, NodeError, NodeInstanceError,
    NodeMode, PrivateNetOptions, NODE_DIAL_LOOP_TIMEOUT, NODE_STARTUP_TIMEOUT,
};
use alloy_genesis::{CliqueConfig, Genesis};
use alloy_primitives::Address;
use k256::ecdsa::SigningKey;
use std::{
    fs::{create_dir, File},
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{Child, ChildStderr, Command, Stdio},
    time::Instant,
};
use tempfile::tempdir;
use url::Url;

/// The exposed APIs
const API: &str = "eth,net,web3,txpool,admin,personal,miner,debug";

/// The geth command
const GETH: &str = "geth";

/// A geth instance. Will close the instance when dropped.
///
/// Construct this using [`Geth`].
#[derive(Debug)]
pub struct GethInstance {
    pid: Child,
    port: u16,
    p2p_port: Option<u16>,
    auth_port: Option<u16>,
    ipc: Option<PathBuf>,
    data_dir: Option<PathBuf>,
    genesis: Option<Genesis>,
    clique_private_key: Option<SigningKey>,
}

impl GethInstance {
    /// Returns the port of this instance
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// Returns the p2p port of this instance
    pub const fn p2p_port(&self) -> Option<u16> {
        self.p2p_port
    }

    /// Returns the auth port of this instance
    pub const fn auth_port(&self) -> Option<u16> {
        self.auth_port
    }

    /// Returns the HTTP endpoint of this instance
    #[doc(alias = "http_endpoint")]
    pub fn endpoint(&self) -> String {
        format!("http://localhost:{}", self.port)
    }

    /// Returns the Websocket endpoint of this instance
    pub fn ws_endpoint(&self) -> String {
        format!("ws://localhost:{}", self.port)
    }

    /// Returns the IPC endpoint of this instance
    pub fn ipc_endpoint(&self) -> String {
        self.ipc.clone().map_or_else(|| "geth.ipc".to_string(), |ipc| ipc.display().to_string())
    }

    /// Returns the HTTP endpoint url of this instance
    #[doc(alias = "http_endpoint_url")]
    pub fn endpoint_url(&self) -> Url {
        Url::parse(&self.endpoint()).unwrap()
    }

    /// Returns the Websocket endpoint url of this instance
    pub fn ws_endpoint_url(&self) -> Url {
        Url::parse(&self.ws_endpoint()).unwrap()
    }

    /// Returns the path to this instances' data directory
    pub const fn data_dir(&self) -> &Option<PathBuf> {
        &self.data_dir
    }

    /// Returns the genesis configuration used to configure this instance
    pub const fn genesis(&self) -> &Option<Genesis> {
        &self.genesis
    }

    /// Returns the private key used to configure clique on this instance
    #[deprecated = "clique support was removed in geth >=1.14"]
    pub const fn clique_private_key(&self) -> &Option<SigningKey> {
        &self.clique_private_key
    }

    /// Takes the stderr contained in the child process.
    ///
    /// This leaves a `None` in its place, so calling methods that require a stderr to be present
    /// will fail if called after this.
    pub fn stderr(&mut self) -> Result<ChildStderr, NodeInstanceError> {
        self.pid.stderr.take().ok_or(NodeInstanceError::NoStderr)
    }

    /// Blocks until geth adds the specified peer, using 20s as the timeout.
    ///
    /// Requires the stderr to be present in the `GethInstance`.
    pub fn wait_to_add_peer(&mut self, id: &str) -> Result<(), NodeInstanceError> {
        let mut stderr = self.pid.stderr.as_mut().ok_or(NodeInstanceError::NoStderr)?;
        let mut err_reader = BufReader::new(&mut stderr);
        let mut line = String::new();
        let start = Instant::now();

        while start.elapsed() < NODE_DIAL_LOOP_TIMEOUT {
            line.clear();
            err_reader.read_line(&mut line).map_err(NodeInstanceError::ReadLineError)?;

            // geth ids are truncated
            let truncated_id = if id.len() > 16 { &id[..16] } else { id };
            if line.contains("Adding p2p peer") && line.contains(truncated_id) {
                return Ok(());
            }
        }
        Err(NodeInstanceError::Timeout("Timed out waiting for geth to add a peer".into()))
    }
}

impl Drop for GethInstance {
    fn drop(&mut self) {
        self.pid.kill().expect("could not kill geth");
    }
}

/// Builder for launching `geth`.
///
/// # Panics
///
/// If `spawn` is called without `geth` being available in the user's $PATH
///
/// # Example
///
/// ```no_run
/// use alloy_node_bindings::Geth;
///
/// let port = 8545u16;
/// let url = format!("http://localhost:{}", port).to_string();
///
/// let geth = Geth::new().port(port).block_time(5000u64).spawn();
///
/// drop(geth); // this will kill the instance
/// ```
#[derive(Clone, Debug, Default)]
#[must_use = "This Builder struct does nothing unless it is `spawn`ed"]
pub struct Geth {
    program: Option<PathBuf>,
    port: Option<u16>,
    authrpc_port: Option<u16>,
    ipc_path: Option<PathBuf>,
    ipc_enabled: bool,
    data_dir: Option<PathBuf>,
    chain_id: Option<u64>,
    insecure_unlock: bool,
    genesis: Option<Genesis>,
    mode: NodeMode,
    clique_private_key: Option<SigningKey>,
}

impl Geth {
    /// Creates an empty Geth builder.
    ///
    /// The mnemonic is chosen randomly.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a Geth builder which will execute `geth` at the given path.
    ///
    /// # Example
    ///
    /// ```
    /// use alloy_node_bindings::Geth;
    /// # fn a() {
    /// let geth = Geth::at("../go-ethereum/build/bin/geth").spawn();
    ///
    /// println!("Geth running at `{}`", geth.endpoint());
    /// # }
    /// ```
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self::new().path(path)
    }

    /// Sets the `path` to the `geth` executable
    ///
    /// By default, it's expected that `geth` is in `$PATH`, see also
    /// [`std::process::Command::new()`]
    pub fn path<T: Into<PathBuf>>(mut self, path: T) -> Self {
        self.program = Some(path.into());
        self
    }

    /// Puts the `geth` instance in `dev` mode.
    pub fn dev(mut self) -> Self {
        self.mode = NodeMode::Dev(Default::default());
        self
    }

    /// Returns whether the node is launched in Clique consensus mode.
    pub const fn is_clique(&self) -> bool {
        self.clique_private_key.is_some()
    }

    /// Calculates the address of the Clique consensus address.
    pub fn clique_address(&self) -> Option<Address> {
        self.clique_private_key.as_ref().map(|pk| Address::from_public_key(pk.verifying_key()))
    }

    /// Sets the Clique Private Key to the `geth` executable, which will be later
    /// loaded on the node.
    ///
    /// The address derived from this private key will be used to set the `miner.etherbase` field
    /// on the node.
    #[deprecated = "clique support was removed in geth >=1.14"]
    pub fn set_clique_private_key<T: Into<SigningKey>>(mut self, private_key: T) -> Self {
        self.clique_private_key = Some(private_key.into());
        self
    }

    /// Sets the port which will be used when the `geth-cli` instance is launched.
    ///
    /// If port is 0 then the OS will choose a random port.
    /// [GethInstance::port] will return the port that was chosen.
    pub fn port<T: Into<u16>>(mut self, port: T) -> Self {
        self.port = Some(port.into());
        self
    }

    /// Sets the port which will be used for incoming p2p connections.
    ///
    /// This will put the geth instance into non-dev mode, discarding any previously set dev-mode
    /// options.
    pub fn p2p_port(mut self, port: u16) -> Self {
        match &mut self.mode {
            NodeMode::Dev(_) => {
                self.mode = NodeMode::NonDev(PrivateNetOptions {
                    p2p_port: Some(port),
                    ..Default::default()
                })
            }
            NodeMode::NonDev(opts) => opts.p2p_port = Some(port),
        }
        self
    }

    /// Sets the block-time which will be used when the `geth-cli` instance is launched.
    ///
    /// This will put the geth instance in `dev` mode, discarding any previously set options that
    /// cannot be used in dev mode.
    pub const fn block_time(mut self, block_time: u64) -> Self {
        self.mode = NodeMode::Dev(DevOptions { block_time: Some(block_time) });
        self
    }

    /// Sets the chain id for the geth instance.
    pub const fn chain_id(mut self, chain_id: u64) -> Self {
        self.chain_id = Some(chain_id);
        self
    }

    /// Allow geth to unlock accounts when rpc apis are open.
    pub const fn insecure_unlock(mut self) -> Self {
        self.insecure_unlock = true;
        self
    }

    /// Enable IPC for the geth instance.
    pub const fn enable_ipc(mut self) -> Self {
        self.ipc_enabled = true;
        self
    }

    /// Disable discovery for the geth instance.
    ///
    /// This will put the geth instance into non-dev mode, discarding any previously set dev-mode
    /// options.
    pub fn disable_discovery(mut self) -> Self {
        self.inner_disable_discovery();
        self
    }

    fn inner_disable_discovery(&mut self) {
        match &mut self.mode {
            NodeMode::Dev(_) => {
                self.mode =
                    NodeMode::NonDev(PrivateNetOptions { discovery: false, ..Default::default() })
            }
            NodeMode::NonDev(opts) => opts.discovery = false,
        }
    }

    /// Sets the IPC path for the socket.
    pub fn ipc_path<T: Into<PathBuf>>(mut self, path: T) -> Self {
        self.ipc_path = Some(path.into());
        self
    }

    /// Sets the data directory for geth.
    pub fn data_dir<T: Into<PathBuf>>(mut self, path: T) -> Self {
        self.data_dir = Some(path.into());
        self
    }

    /// Sets the `genesis.json` for the geth instance.
    ///
    /// If this is set, geth will be initialized with `geth init` and the `--datadir` option will be
    /// set to the same value as `data_dir`.
    ///
    /// This is destructive and will overwrite any existing data in the data directory.
    pub fn genesis(mut self, genesis: Genesis) -> Self {
        self.genesis = Some(genesis);
        self
    }

    /// Sets the port for authenticated RPC connections.
    pub const fn authrpc_port(mut self, port: u16) -> Self {
        self.authrpc_port = Some(port);
        self
    }

    /// Consumes the builder and spawns `geth`.
    ///
    /// # Panics
    ///
    /// If spawning the instance fails at any point.
    #[track_caller]
    pub fn spawn(self) -> GethInstance {
        self.try_spawn().unwrap()
    }

    /// Consumes the builder and spawns `geth`. If spawning fails, returns an error.
    pub fn try_spawn(mut self) -> Result<GethInstance, NodeError> {
        let bin_path = self
            .program
            .as_ref()
            .map_or_else(|| GETH.as_ref(), |bin| bin.as_os_str())
            .to_os_string();
        let mut cmd = Command::new(&bin_path);
        // `geth` uses stderr for its logs
        cmd.stderr(Stdio::piped());

        // If no port provided, let the os chose it for us
        let mut port = self.port.unwrap_or(0);
        let port_s = port.to_string();

        // If IPC is not enabled on the builder, disable it.
        if !self.ipc_enabled {
            cmd.arg("--ipcdisable");
        }

        // Open the HTTP API
        cmd.arg("--http");
        cmd.arg("--http.port").arg(&port_s);
        cmd.arg("--http.api").arg(API);

        // Open the WS API
        cmd.arg("--ws");
        cmd.arg("--ws.port").arg(port_s);
        cmd.arg("--ws.api").arg(API);

        // pass insecure unlock flag if set
        let is_clique = self.is_clique();
        if self.insecure_unlock || is_clique {
            cmd.arg("--allow-insecure-unlock");
        }

        if is_clique {
            self.inner_disable_discovery();
        }

        // Set the port for authenticated APIs
        let authrpc_port = self.authrpc_port.unwrap_or_else(&mut unused_port);
        cmd.arg("--authrpc.port").arg(authrpc_port.to_string());

        // use geth init to initialize the datadir if the genesis exists
        if is_clique {
            let clique_addr = self.clique_address();
            if let Some(genesis) = &mut self.genesis {
                // set up a clique config with an instant sealing period and short (8 block) epoch
                let clique_config = CliqueConfig { period: Some(0), epoch: Some(8) };
                genesis.config.clique = Some(clique_config);

                let clique_addr = clique_addr.ok_or(NodeError::CliqueAddressError(
                    "could not calculates the address of the Clique consensus address.".to_string(),
                ))?;

                // set the extraData field
                let extra_data_bytes =
                    [&[0u8; 32][..], clique_addr.as_ref(), &[0u8; 65][..]].concat();
                genesis.extra_data = extra_data_bytes.into();

                // we must set the etherbase if using clique
                // need to use format! / Debug here because the Address Display impl doesn't show
                // the entire address
                cmd.arg("--miner.etherbase").arg(format!("{clique_addr:?}"));
            }

            let clique_addr = self.clique_address().ok_or(NodeError::CliqueAddressError(
                "could not calculates the address of the Clique consensus address.".to_string(),
            ))?;

            self.genesis = Some(Genesis::clique_genesis(
                self.chain_id.ok_or(NodeError::ChainIdNotSet)?,
                clique_addr,
            ));

            // we must set the etherbase if using clique
            // need to use format! / Debug here because the Address Display impl doesn't show the
            // entire address
            cmd.arg("--miner.etherbase").arg(format!("{clique_addr:?}"));
        }

        if let Some(genesis) = &self.genesis {
            // create a temp dir to store the genesis file
            let temp_genesis_dir_path = tempdir().map_err(NodeError::CreateDirError)?.into_path();

            // create a temp dir to store the genesis file
            let temp_genesis_path = temp_genesis_dir_path.join("genesis.json");

            // create the genesis file
            let mut file = File::create(&temp_genesis_path).map_err(|_| {
                NodeError::GenesisError("could not create genesis file".to_string())
            })?;

            // serialize genesis and write to file
            serde_json::to_writer_pretty(&mut file, &genesis).map_err(|_| {
                NodeError::GenesisError("could not write genesis to file".to_string())
            })?;

            let mut init_cmd = Command::new(bin_path);
            if let Some(data_dir) = &self.data_dir {
                init_cmd.arg("--datadir").arg(data_dir);
            }

            // set the stderr to null so we don't pollute the test output
            init_cmd.stderr(Stdio::null());

            init_cmd.arg("init").arg(temp_genesis_path);
            let res = init_cmd
                .spawn()
                .map_err(NodeError::SpawnError)?
                .wait()
                .map_err(NodeError::WaitError)?;
            // .expect("failed to wait for geth init to exit");
            if !res.success() {
                return Err(NodeError::InitError);
            }

            // clean up the temp dir which is now persisted
            std::fs::remove_dir_all(temp_genesis_dir_path).map_err(|_| {
                NodeError::GenesisError("could not remove genesis temp dir".to_string())
            })?;
        }

        if let Some(data_dir) = &self.data_dir {
            cmd.arg("--datadir").arg(data_dir);

            // create the directory if it doesn't exist
            if !data_dir.exists() {
                create_dir(data_dir).map_err(NodeError::CreateDirError)?;
            }
        }

        // Dev mode with custom block time
        let mut p2p_port = match self.mode {
            NodeMode::Dev(DevOptions { block_time }) => {
                cmd.arg("--dev");
                if let Some(block_time) = block_time {
                    cmd.arg("--dev.period").arg(block_time.to_string());
                }
                None
            }
            NodeMode::NonDev(PrivateNetOptions { p2p_port, discovery }) => {
                // if no port provided, let the os chose it for us
                let port = p2p_port.unwrap_or(0);
                cmd.arg("--port").arg(port.to_string());

                // disable discovery if the flag is set
                if !discovery {
                    cmd.arg("--nodiscover");
                }
                Some(port)
            }
        };

        if let Some(chain_id) = self.chain_id {
            cmd.arg("--networkid").arg(chain_id.to_string());
        }

        // debug verbosity is needed to check when peers are added
        cmd.arg("--verbosity").arg("4");

        if let Some(ipc) = &self.ipc_path {
            cmd.arg("--ipcpath").arg(ipc);
        }

        let mut child = cmd.spawn().map_err(NodeError::SpawnError)?;

        let stderr = child.stderr.ok_or(NodeError::NoStderr)?;

        let start = Instant::now();
        let mut reader = BufReader::new(stderr);

        // we shouldn't need to wait for p2p to start if geth is in dev mode - p2p is disabled in
        // dev mode
        let mut p2p_started = matches!(self.mode, NodeMode::Dev(_));
        let mut http_started = false;

        loop {
            if start + NODE_STARTUP_TIMEOUT <= Instant::now() {
                return Err(NodeError::Timeout);
            }

            let mut line = String::with_capacity(120);
            reader.read_line(&mut line).map_err(NodeError::ReadLineError)?;

            if matches!(self.mode, NodeMode::NonDev(_)) && line.contains("Started P2P networking") {
                p2p_started = true;
            }

            if !matches!(self.mode, NodeMode::Dev(_)) {
                // try to find the p2p port, if not in dev mode
                if line.contains("New local node record") {
                    if let Some(port) = extract_value("tcp=", &line) {
                        p2p_port = port.parse::<u16>().ok();
                    }
                }
            }

            // geth 1.9.23 uses "server started" while 1.9.18 uses "endpoint opened"
            // the unauthenticated api is used for regular non-engine API requests
            if line.contains("HTTP endpoint opened")
                || (line.contains("HTTP server started") && !line.contains("auth=true"))
            {
                // Extracts the address from the output
                if let Some(addr) = extract_endpoint("endpoint=", &line) {
                    // use the actual http port
                    port = addr.port();
                }

                http_started = true;
            }

            // Encountered an error such as Fatal: Error starting protocol stack: listen tcp
            // 127.0.0.1:8545: bind: address already in use
            if line.contains("Fatal:") {
                return Err(NodeError::Fatal(line));
            }

            if p2p_started && http_started {
                break;
            }
        }

        child.stderr = Some(reader.into_inner());

        Ok(GethInstance {
            pid: child,
            port,
            ipc: self.ipc_path,
            data_dir: self.data_dir,
            p2p_port,
            auth_port: self.authrpc_port,
            genesis: self.genesis,
            clique_private_key: self.clique_private_key,
        })
    }
}

// These tests should use a different datadir for each `Geth` spawned
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn port_0() {
        run_with_tempdir(|_| {
            let _geth = Geth::new().disable_discovery().port(0u16).spawn();
        });
    }

    /// Allows running tests with a temporary directory, which is cleaned up after the function is
    /// called.
    ///
    /// Helps with tests that spawn a helper instance, which has to be dropped before the temporary
    /// directory is cleaned up.
    #[track_caller]
    fn run_with_tempdir(f: impl Fn(&Path)) {
        let temp_dir = tempfile::tempdir().unwrap();
        let temp_dir_path = temp_dir.path();
        f(temp_dir_path);
        #[cfg(not(windows))]
        temp_dir.close().unwrap();
    }

    #[test]
    fn p2p_port() {
        run_with_tempdir(|temp_dir_path| {
            let geth = Geth::new().disable_discovery().data_dir(temp_dir_path).spawn();
            let p2p_port = geth.p2p_port();
            assert!(p2p_port.is_some());
        });
    }

    #[test]
    fn explicit_p2p_port() {
        run_with_tempdir(|temp_dir_path| {
            // if a p2p port is explicitly set, it should be used
            let geth = Geth::new().p2p_port(1234).data_dir(temp_dir_path).spawn();
            let p2p_port = geth.p2p_port();
            assert_eq!(p2p_port, Some(1234));
        });
    }

    #[test]
    fn dev_mode() {
        run_with_tempdir(|temp_dir_path| {
            // dev mode should not have a p2p port, and dev should be the default
            let geth = Geth::new().data_dir(temp_dir_path).spawn();
            let p2p_port = geth.p2p_port();
            assert!(p2p_port.is_none(), "{p2p_port:?}");
        })
    }

    #[test]
    #[ignore = "fails on geth >=1.14"]
    #[allow(deprecated)]
    fn clique_correctly_configured() {
        run_with_tempdir(|temp_dir_path| {
            let private_key = SigningKey::random(&mut rand::thread_rng());
            let geth = Geth::new()
                .set_clique_private_key(private_key)
                .chain_id(1337u64)
                .data_dir(temp_dir_path)
                .spawn();

            assert!(geth.p2p_port.is_some());
            assert!(geth.clique_private_key().is_some());
            assert!(geth.genesis().is_some());
        })
    }
}
