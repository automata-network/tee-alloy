use crate::{
    fillers::{
        CachedNonceManager, ChainIdFiller, FillerControlFlow, GasFiller, JoinFill, NonceFiller,
        NonceManager, RecommendedFiller, SimpleNonceManager, TxFiller, WalletFiller,
    },
    provider::SendableTx,
    Provider, RootProvider,
};
use alloy_chains::NamedChain;
use alloy_network::{Ethereum, Network};
use alloy_primitives::ChainId;
use alloy_rpc_client::{BuiltInConnectionString, ClientBuilder, RpcClient};
use alloy_transport::{BoxTransport, Transport, TransportError, TransportResult};
use std::marker::PhantomData;

/// A layering abstraction in the vein of [`tower::Layer`]
///
/// [`tower::Layer`]: https://docs.rs/tower/latest/tower/trait.Layer.html
pub trait ProviderLayer<P: Provider<T, N>, T: Transport + Clone, N: Network = Ethereum> {
    /// The provider constructed by this layer.
    type Provider: Provider<T, N>;

    /// Wrap the given provider in the layer's provider.
    fn layer(&self, inner: P) -> Self::Provider;
}

/// An identity layer that does nothing.
#[derive(Clone, Copy, Debug)]
pub struct Identity;

impl<N> TxFiller<N> for Identity
where
    N: Network,
{
    type Fillable = ();

    fn status(&self, _tx: &<N as Network>::TransactionRequest) -> FillerControlFlow {
        FillerControlFlow::Finished
    }

    fn fill_sync(&self, _tx: &mut SendableTx<N>) {}

    async fn prepare<P, T>(
        &self,
        _provider: &P,
        _tx: &N::TransactionRequest,
    ) -> TransportResult<Self::Fillable> {
        Ok(())
    }

    async fn fill(
        &self,
        _to_fill: Self::Fillable,
        tx: SendableTx<N>,
    ) -> TransportResult<SendableTx<N>> {
        Ok(tx)
    }
}

impl<P, T, N> ProviderLayer<P, T, N> for Identity
where
    T: Transport + Clone,
    N: Network,
    P: Provider<T, N>,
{
    type Provider = P;

    fn layer(&self, inner: P) -> Self::Provider {
        inner
    }
}

/// A stack of two providers.
#[derive(Debug)]
pub struct Stack<Inner, Outer> {
    inner: Inner,
    outer: Outer,
}

impl<Inner, Outer> Stack<Inner, Outer> {
    /// Create a new `Stack`.
    pub const fn new(inner: Inner, outer: Outer) -> Self {
        Self { inner, outer }
    }
}

impl<P, T, N, Inner, Outer> ProviderLayer<P, T, N> for Stack<Inner, Outer>
where
    T: Transport + Clone,
    N: Network,
    P: Provider<T, N>,
    Inner: ProviderLayer<P, T, N>,
    Outer: ProviderLayer<Inner::Provider, T, N>,
{
    type Provider = Outer::Provider;

    fn layer(&self, provider: P) -> Self::Provider {
        let inner = self.inner.layer(provider);

        self.outer.layer(inner)
    }
}

/// A builder for constructing a [`Provider`] from various layers.
///
/// This type is similar to [`tower::ServiceBuilder`], with extra complication
/// around maintaining the network and transport types.
///
/// [`tower::ServiceBuilder`]: https://docs.rs/tower/latest/tower/struct.ServiceBuilder.html
#[derive(Debug)]
pub struct ProviderBuilder<L, F, N = Ethereum> {
    layer: L,
    filler: F,
    network: PhantomData<fn() -> N>,
}

impl ProviderBuilder<Identity, Identity, Ethereum> {
    /// Create a new [`ProviderBuilder`].
    pub const fn new() -> Self {
        Self { layer: Identity, filler: Identity, network: PhantomData }
    }
}

impl<N> Default for ProviderBuilder<Identity, Identity, N> {
    fn default() -> Self {
        Self { layer: Identity, filler: Identity, network: PhantomData }
    }
}

impl<L, N> ProviderBuilder<L, Identity, N> {
    /// Add preconfigured set of layers handling gas estimation, nonce
    /// management, and chain-id fetching.
    pub fn with_recommended_fillers(self) -> ProviderBuilder<L, RecommendedFiller, N> {
        self.filler(GasFiller).filler(NonceFiller::default()).filler(ChainIdFiller::default())
    }

    /// Add gas estimation to the stack being built.
    ///
    /// See [`GasFiller`]
    pub fn with_gas_estimation(self) -> ProviderBuilder<L, JoinFill<Identity, GasFiller>, N> {
        self.filler(GasFiller)
    }

    /// Add nonce management to the stack being built.
    ///
    /// See [`NonceFiller`]
    pub fn with_nonce_management<M: NonceManager>(
        self,
        nonce_manager: M,
    ) -> ProviderBuilder<L, JoinFill<Identity, NonceFiller<M>>, N> {
        self.filler(NonceFiller::new(nonce_manager))
    }

    /// Add simple nonce management to the stack being built.
    ///
    /// See [`SimpleNonceManager`]
    pub fn with_simple_nonce_management(
        self,
    ) -> ProviderBuilder<L, JoinFill<Identity, NonceFiller>, N> {
        self.with_nonce_management(SimpleNonceManager::default())
    }

    /// Add cached nonce management to the stack being built.
    ///
    /// See [`CachedNonceManager`]
    pub fn with_cached_nonce_management(
        self,
    ) -> ProviderBuilder<L, JoinFill<Identity, NonceFiller<CachedNonceManager>>, N> {
        self.with_nonce_management(CachedNonceManager::default())
    }

    /// Add a chain ID filler to the stack being built. The filler will attempt
    /// to fetch the chain ID from the provider using
    /// [`Provider::get_chain_id`]. the first time a transaction is prepared,
    /// and will cache it for future transactions.
    pub fn fetch_chain_id(self) -> ProviderBuilder<L, JoinFill<Identity, ChainIdFiller>, N> {
        self.filler(ChainIdFiller::default())
    }

    /// Add a specific chain ID to the stack being built. The filler will
    /// fill transactions with the provided chain ID, regardless of the chain ID
    /// that the provider reports via [`Provider::get_chain_id`].
    pub fn with_chain_id(
        self,
        chain_id: ChainId,
    ) -> ProviderBuilder<L, JoinFill<Identity, ChainIdFiller>, N> {
        self.filler(ChainIdFiller::new(Some(chain_id)))
    }
}

impl<L, F, N> ProviderBuilder<L, F, N> {
    /// Add a layer to the stack being built. This is similar to
    /// [`tower::ServiceBuilder::layer`].
    ///
    /// ## Note:
    ///
    /// Layers are added in outer-to-inner order, as in
    /// [`tower::ServiceBuilder`]. The first layer added will be the first to
    /// see the request.
    ///
    /// [`tower::ServiceBuilder::layer`]: https://docs.rs/tower/latest/tower/struct.ServiceBuilder.html#method.layer
    /// [`tower::ServiceBuilder`]: https://docs.rs/tower/latest/tower/struct.ServiceBuilder.html
    pub fn layer<Inner>(self, layer: Inner) -> ProviderBuilder<Stack<Inner, L>, F, N> {
        ProviderBuilder {
            layer: Stack::new(layer, self.layer),
            filler: self.filler,
            network: PhantomData,
        }
    }

    /// Add a transaction filler to the stack being built. Transaction fillers
    /// are used to fill in missing fields on transactions before they are sent,
    /// and are all joined to form the outermost layer of the stack.
    pub fn filler<F2>(self, filler: F2) -> ProviderBuilder<L, JoinFill<F, F2>, N> {
        ProviderBuilder {
            layer: self.layer,
            filler: JoinFill::new(self.filler, filler),
            network: PhantomData,
        }
    }

    /// Add a wallet layer to the stack being built.
    ///
    /// See [`WalletFiller`].
    pub fn wallet<W>(self, wallet: W) -> ProviderBuilder<L, JoinFill<F, WalletFiller<W>>, N> {
        self.filler(WalletFiller::new(wallet))
    }

    /// Change the network.
    ///
    /// By default, the network is `Ethereum`. This method must be called to configure a different
    /// network.
    ///
    /// ```ignore
    /// builder.network::<Arbitrum>()
    /// ```
    pub fn network<Net: Network>(self) -> ProviderBuilder<L, F, Net> {
        ProviderBuilder { layer: self.layer, filler: self.filler, network: PhantomData }
    }

    /// Add a chain layer to the stack being built. The layer will set
    /// the client's poll interval based on the average block time for this chain.
    ///
    /// Does nothing to the client with a local transport.
    pub fn with_chain(
        self,
        chain: NamedChain,
    ) -> ProviderBuilder<Stack<crate::layers::ChainLayer, L>, F, N> {
        let chain_layer = crate::layers::ChainLayer::from(chain);
        self.layer(chain_layer)
    }

    /// Finish the layer stack by providing a root [`Provider`], outputting
    /// the final [`Provider`] type with all stack components.
    pub fn on_provider<P, T>(self, provider: P) -> F::Provider
    where
        L: ProviderLayer<P, T, N>,
        F: TxFiller<N> + ProviderLayer<L::Provider, T, N>,
        P: Provider<T, N>,
        T: Transport + Clone,
        N: Network,
    {
        let Self { layer, filler, .. } = self;
        let stack = Stack::new(layer, filler);
        stack.layer(provider)
    }

    /// Finish the layer stack by providing a root [`RpcClient`], outputting
    /// the final [`Provider`] type with all stack components.
    ///
    /// This is a convenience function for
    /// `ProviderBuilder::provider<RpcClient>`.
    pub fn on_client<T>(self, client: RpcClient<T>) -> F::Provider
    where
        L: ProviderLayer<RootProvider<T, N>, T, N>,
        F: TxFiller<N> + ProviderLayer<L::Provider, T, N>,
        T: Transport + Clone,
        N: Network,
    {
        self.on_provider(RootProvider::new(client))
    }

    /// Finish the layer stack by providing a connection string for a built-in
    /// transport type, outputting the final [`Provider`] type with all stack
    /// components.
    pub async fn on_builtin(self, s: &str) -> Result<F::Provider, TransportError>
    where
        L: ProviderLayer<RootProvider<BoxTransport, N>, BoxTransport, N>,
        F: TxFiller<N> + ProviderLayer<L::Provider, BoxTransport, N>,
        N: Network,
    {
        let connect: BuiltInConnectionString = s.parse()?;
        let client = ClientBuilder::default().connect_boxed(connect).await?;
        Ok(self.on_client(client))
    }

    /// Build this provider with a websocket connection.
    #[cfg(feature = "ws")]
    pub async fn on_ws(
        self,
        connect: alloy_transport_ws::WsConnect,
    ) -> Result<F::Provider, TransportError>
    where
        L: ProviderLayer<
            RootProvider<alloy_pubsub::PubSubFrontend, N>,
            alloy_pubsub::PubSubFrontend,
            N,
        >,
        F: TxFiller<N> + ProviderLayer<L::Provider, alloy_pubsub::PubSubFrontend, N>,
        N: Network,
    {
        let client = ClientBuilder::default().ws(connect).await?;
        Ok(self.on_client(client))
    }

    /// Build this provider with an IPC connection.
    #[cfg(feature = "ipc")]
    pub async fn on_ipc<T>(
        self,
        connect: alloy_transport_ipc::IpcConnect<T>,
    ) -> Result<F::Provider, TransportError>
    where
        alloy_transport_ipc::IpcConnect<T>: alloy_pubsub::PubSubConnect,
        L: ProviderLayer<
            RootProvider<alloy_pubsub::PubSubFrontend, N>,
            alloy_pubsub::PubSubFrontend,
            N,
        >,
        F: TxFiller<N> + ProviderLayer<L::Provider, alloy_pubsub::PubSubFrontend, N>,
        N: Network,
    {
        let client = ClientBuilder::default().ipc(connect).await?;
        Ok(self.on_client(client))
    }

    /// Build this provider with an Reqwest HTTP transport.
    #[cfg(any(test, feature = "reqwest"))]
    pub fn on_http(self, url: reqwest::Url) -> F::Provider
    where
        L: ProviderLayer<crate::ReqwestProvider<N>, alloy_transport_http::Http<reqwest::Client>, N>,
        F: TxFiller<N> + ProviderLayer<L::Provider, alloy_transport_http::Http<reqwest::Client>, N>,
        N: Network,
    {
        let client = ClientBuilder::default().http(url);
        self.on_client(client)
    }

    /// Build this provider with an Hyper HTTP transport.
    #[cfg(feature = "hyper")]
    pub fn on_hyper_http(self, url: url::Url) -> F::Provider
    where
        L: ProviderLayer<
            crate::HyperProvider<N>,
            alloy_transport_http::Http<alloy_transport_http::HyperLayerTransport>,
            N,
        >,
        F: TxFiller<N>
            + ProviderLayer<
                L::Provider,
                alloy_transport_http::Http<alloy_transport_http::HyperLayerTransport>,
                N,
            >,
        N: Network,
    {
        let client = ClientBuilder::default().hyper_http(url);
        self.on_client(client)
    }
}

#[cfg(any(test, feature = "anvil-node"))]
type JoinedEthereumWalletFiller<F> = JoinFill<F, WalletFiller<alloy_network::EthereumWallet>>;

#[cfg(any(test, feature = "anvil-node"))]
type AnvilProviderResult<T> = Result<T, alloy_node_bindings::anvil::AnvilError>;

// Enabled when the `anvil` feature is enabled, or when both in test and the
// `reqwest` feature is enabled.
#[cfg(any(test, feature = "anvil-node"))]
impl<L, F> ProviderBuilder<L, F, Ethereum> {
    /// Build this provider with anvil, using an Reqwest HTTP transport.
    pub fn on_anvil(self) -> F::Provider
    where
        F: TxFiller<Ethereum>
            + ProviderLayer<L::Provider, alloy_transport_http::Http<reqwest::Client>, Ethereum>,
        L: crate::builder::ProviderLayer<
            crate::layers::AnvilProvider<
                crate::provider::RootProvider<alloy_transport_http::Http<reqwest::Client>>,
                alloy_transport_http::Http<reqwest::Client>,
            >,
            alloy_transport_http::Http<reqwest::Client>,
        >,
    {
        self.on_anvil_with_config(std::convert::identity)
    }

    /// Build this provider with anvil, using an Reqwest HTTP transport. This
    /// function configures a wallet backed by anvil keys, and is intended for
    /// use in tests.
    pub fn on_anvil_with_wallet(
        self,
    ) -> <JoinedEthereumWalletFiller<F> as ProviderLayer<
        L::Provider,
        alloy_transport_http::Http<reqwest::Client>,
    >>::Provider
    where
        F: TxFiller<Ethereum>
            + ProviderLayer<L::Provider, alloy_transport_http::Http<reqwest::Client>, Ethereum>,
        L: crate::builder::ProviderLayer<
            crate::layers::AnvilProvider<
                crate::provider::RootProvider<alloy_transport_http::Http<reqwest::Client>>,
                alloy_transport_http::Http<reqwest::Client>,
            >,
            alloy_transport_http::Http<reqwest::Client>,
        >,
    {
        self.on_anvil_with_wallet_and_config(std::convert::identity)
    }

    /// Build this provider with anvil, using an Reqwest HTTP transport. The
    /// given function is used to configure the anvil instance.
    pub fn on_anvil_with_config(
        self,
        f: impl FnOnce(alloy_node_bindings::Anvil) -> alloy_node_bindings::Anvil,
    ) -> F::Provider
    where
        F: TxFiller<Ethereum>
            + ProviderLayer<L::Provider, alloy_transport_http::Http<reqwest::Client>, Ethereum>,
        L: crate::builder::ProviderLayer<
            crate::layers::AnvilProvider<
                crate::provider::RootProvider<alloy_transport_http::Http<reqwest::Client>>,
                alloy_transport_http::Http<reqwest::Client>,
            >,
            alloy_transport_http::Http<reqwest::Client>,
        >,
    {
        let anvil_layer = crate::layers::AnvilLayer::from(f(Default::default()));
        let url = anvil_layer.endpoint_url();

        self.layer(anvil_layer).on_http(url)
    }

    /// Build this provider with anvil, using an Reqwest HTTP transport.
    /// This calls `try_on_anvil_with_wallet_and_config` and panics on error.
    pub fn on_anvil_with_wallet_and_config(
        self,
        f: impl FnOnce(alloy_node_bindings::Anvil) -> alloy_node_bindings::Anvil,
    ) -> <JoinedEthereumWalletFiller<F> as ProviderLayer<
        L::Provider,
        alloy_transport_http::Http<reqwest::Client>,
    >>::Provider
    where
        F: TxFiller<Ethereum>
            + ProviderLayer<L::Provider, alloy_transport_http::Http<reqwest::Client>, Ethereum>,
        L: crate::builder::ProviderLayer<
            crate::layers::AnvilProvider<
                crate::provider::RootProvider<alloy_transport_http::Http<reqwest::Client>>,
                alloy_transport_http::Http<reqwest::Client>,
            >,
            alloy_transport_http::Http<reqwest::Client>,
        >,
    {
        self.try_on_anvil_with_wallet_and_config(f).unwrap()
    }

    /// Build this provider with anvil, using an Reqwest HTTP transport. The
    /// given function is used to configure the anvil instance. This
    /// function configures a wallet backed by anvil keys, and is intended for
    /// use in tests.
    pub fn try_on_anvil_with_wallet_and_config(
        self,
        f: impl FnOnce(alloy_node_bindings::Anvil) -> alloy_node_bindings::Anvil,
    ) -> AnvilProviderResult<
        <JoinedEthereumWalletFiller<F> as ProviderLayer<
            L::Provider,
            alloy_transport_http::Http<reqwest::Client>,
        >>::Provider,
    >
    where
        F: TxFiller<Ethereum>
            + ProviderLayer<L::Provider, alloy_transport_http::Http<reqwest::Client>, Ethereum>,
        L: crate::builder::ProviderLayer<
            crate::layers::AnvilProvider<
                crate::provider::RootProvider<alloy_transport_http::Http<reqwest::Client>>,
                alloy_transport_http::Http<reqwest::Client>,
            >,
            alloy_transport_http::Http<reqwest::Client>,
        >,
    {
        let anvil_layer = crate::layers::AnvilLayer::from(f(Default::default()));
        let url = anvil_layer.endpoint_url();

        let default_keys = anvil_layer.instance().keys().to_vec();
        let (default_key, remaining_keys) = default_keys
            .split_first()
            .ok_or(alloy_node_bindings::anvil::AnvilError::NoKeysAvailable)?;

        let default_signer = alloy_signer_local::LocalSigner::from(default_key.clone());
        let mut wallet = alloy_network::EthereumWallet::from(default_signer);

        remaining_keys.iter().for_each(|key| {
            wallet.register_signer(alloy_signer_local::LocalSigner::from(key.clone()))
        });

        Ok(self.wallet(wallet).layer(anvil_layer).on_http(url))
    }
}

// Copyright (c) 2019 Tower Contributors

// Permission is hereby granted, free of charge, to any
// person obtaining a copy of this software and associated
// documentation files (the "Software"), to deal in the
// Software without restriction, including without
// limitation the rights to use, copy, modify, merge,
// publish, distribute, sublicense, and/or sell copies of
// the Software, and to permit persons to whom the Software
// is furnished to do so, subject to the following
// conditions:

// The above copyright notice and this permission notice
// shall be included in all copies or substantial portions
// of the Software.

// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF
// ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED
// TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A
// PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT
// SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
// CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION
// OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR
// IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.
