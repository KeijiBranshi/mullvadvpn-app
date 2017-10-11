use error_chain;

use error_chain::ChainedError;
use jsonrpc_client_core;
use jsonrpc_core::{Error, ErrorCode, Metadata};
use jsonrpc_core::futures::{future, sync, Future};
use jsonrpc_core::futures::sync::oneshot::Sender as OneshotSender;
use jsonrpc_macros::pubsub;
use jsonrpc_pubsub::{PubSubHandler, PubSubMetadata, Session, SubscriptionId};
use jsonrpc_ws_server;
use mullvad_types::account::{AccountData, AccountToken};
use mullvad_types::location::{CountryCode, Location};
use mullvad_types::relay_endpoint::RelayEndpoint;
use mullvad_types::states::{DaemonState, TargetState};

use serde;

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex, RwLock};
use std::sync::atomic::{AtomicBool, Ordering};

use talpid_core::mpsc::IntoSender;
use talpid_ipc;
use uuid;

/// FIXME(linus): This is here just because the futures crate has deprecated it and jsonrpc_core
/// did not introduce their own yet (https://github.com/paritytech/jsonrpc/pull/196).
/// Remove this and use the one in jsonrpc_core when that is released.
pub type BoxFuture<T, E> = Box<Future<Item = T, Error = E> + Send>;

build_rpc_trait! {
    pub trait ManagementInterfaceApi {
        type Metadata;

        #[rpc(meta, name = "auth")]
        fn auth(&self, Self::Metadata, String) -> BoxFuture<(), Error>;

        /// Fetches and returns metadata about an account. Returns an error on non-existing
        /// accounts.
        #[rpc(meta, name = "get_account_data")]
        fn get_account_data(&self, Self::Metadata, AccountToken) -> BoxFuture<AccountData, Error>;

        /// Returns available countries.
        #[rpc(name = "get_countries")]
        fn get_countries(&self) -> Result<HashMap<CountryCode, String>, Error>;

        /// Set which account to connect with.
        #[rpc(meta, name = "set_account")]
        fn set_account(&self, Self::Metadata, Option<AccountToken>) -> BoxFuture<(), Error>;

        /// Get which account is configured.
        #[rpc(meta, name = "get_account")]
        fn get_account(&self, Self::Metadata) -> BoxFuture<Option<AccountToken>, Error>;

        /// Set which relay to connect to
        #[rpc(meta, name = "set_custom_relay")]
        fn set_custom_relay(&self, Self::Metadata, RelayEndpoint) -> BoxFuture<(), Error>;

        /// Unset the custom relay, reverting to the default relay listing
        #[rpc(meta, name = "remove_custom_relay")]
        fn remove_custom_relay(&self, Self::Metadata) -> BoxFuture<(), Error>;

        /// Set if the client should automatically establish a tunnel on start or not.
        #[rpc(meta, name = "set_autoconnect")]
        fn set_autoconnect(&self, Self::Metadata, bool) -> BoxFuture<(), Error>;

        /// Try to connect if disconnected, or do nothing if already connecting/connected.
        #[rpc(meta, name = "connect")]
        fn connect(&self, Self::Metadata) -> BoxFuture<(), Error>;

        /// Disconnect the VPN tunnel if it is connecting/connected. Does nothing if already
        /// disconnected.
        #[rpc(meta, name = "disconnect")]
        fn disconnect(&self, Self::Metadata) -> BoxFuture<(), Error>;

        /// Returns the current state of the Mullvad client. Changes to this state will
        /// be announced to subscribers of `new_state`.
        #[rpc(meta, name = "get_state")]
        fn get_state(&self, Self::Metadata) -> BoxFuture<DaemonState, Error>;

        /// Returns the current public IP of this computer.
        #[rpc(name = "get_ip")]
        fn get_ip(&self) -> Result<IpAddr, Error>;

        /// Performs a geoIP lookup and returns the current location as perceived by the public
        /// internet.
        #[rpc(name = "get_location")]
        fn get_location(&self) -> Result<Location, Error>;

        #[pubsub(name = "new_state")] {
            /// Subscribes to the `new_state` event notifications.
            #[rpc(name = "new_state_subscribe")]
            fn new_state_subscribe(&self, Self::Metadata, pubsub::Subscriber<DaemonState>);

            /// Unsubscribes from the `new_state` event notifications.
            #[rpc(name = "new_state_unsubscribe")]
            fn new_state_unsubscribe(&self, SubscriptionId) -> BoxFuture<(), Error>;
        }

        #[pubsub(name = "error")] {
            /// Subscribes to the `error` event notifications.
            #[rpc(name = "error_subscribe")]
            fn error_subscribe(&self, Self::Metadata, pubsub::Subscriber<Vec<String>>);

            /// Unsubscribes from the `error` event notifications.
            #[rpc(name = "error_unsubscribe")]
            fn error_unsubscribe(&self, SubscriptionId) -> BoxFuture<(), Error>;
        }
    }
}


/// Enum representing commands coming in on the management interface.
pub enum TunnelCommand {
    /// Change target state.
    SetTargetState(TargetState),
    /// Request the current state.
    GetState(OneshotSender<DaemonState>),
    /// Request the metadata for an account.
    GetAccountData(
        OneshotSender<BoxFuture<AccountData, jsonrpc_client_core::Error>>,
        AccountToken,
    ),
    /// Set which account token to use for subsequent connection attempts.
    SetAccount(OneshotSender<()>, Option<AccountToken>),
    /// Request the current account token being used.
    GetAccount(OneshotSender<Option<AccountToken>>),
    /// Set a custom relay instead of the default list of relays
    SetCustomRelay(OneshotSender<()>, Option<RelayEndpoint>),
}

#[derive(Default)]
struct ActiveSubscriptions {
    new_state_subscriptions: RwLock<HashMap<SubscriptionId, pubsub::Sink<DaemonState>>>,
    error_subscriptions: RwLock<HashMap<SubscriptionId, pubsub::Sink<Vec<String>>>>,
}

pub struct ManagementInterfaceServer {
    server: talpid_ipc::IpcServer,
    subscriptions: Arc<ActiveSubscriptions>,
}

impl ManagementInterfaceServer {
    pub fn start<T>(
        tunnel_tx: IntoSender<TunnelCommand, T>,
        shared_secret: String,
    ) -> talpid_ipc::Result<Self>
    where
        T: From<TunnelCommand> + 'static + Send,
    {
        let rpc = ManagementInterface::new(tunnel_tx, shared_secret);
        let subscriptions = rpc.subscriptions.clone();

        let mut io = PubSubHandler::default();
        io.extend_with(rpc.to_delegate());
        let server = talpid_ipc::IpcServer::start_with_metadata(io.into(), meta_extractor)?;
        Ok(ManagementInterfaceServer {
            server,
            subscriptions,
        })
    }

    pub fn address(&self) -> &str {
        self.server.address()
    }

    pub fn event_broadcaster(&self) -> EventBroadcaster {
        EventBroadcaster {
            subscriptions: self.subscriptions.clone(),
        }
    }

    /// Consumes the server and waits for it to finish. Returns an error if the server exited
    /// due to an error.
    pub fn wait(self) -> talpid_ipc::Result<()> {
        self.server.wait()
    }
}


/// A handle that allows broadcasting messages to all subscribers of the management interface.
pub struct EventBroadcaster {
    subscriptions: Arc<ActiveSubscriptions>,
}

impl EventBroadcaster {
    /// Sends a new state update to all `new_state` subscribers of the management interface.
    pub fn notify_new_state(&self, new_state: DaemonState) {
        self.notify(&self.subscriptions.new_state_subscriptions, new_state);
    }

    /// Sends an error to all `error` subscribers of the management interface.
    pub fn notify_error<E>(&self, error: &E)
    where
        E: error_chain::ChainedError,
    {
        let error_strings = error.iter().map(|e| e.to_string()).collect();
        self.notify(&self.subscriptions.error_subscriptions, error_strings);
    }

    fn notify<T>(
        &self,
        subscriptions_lock: &RwLock<HashMap<SubscriptionId, pubsub::Sink<T>>>,
        value: T,
    ) where
        T: serde::Serialize + Clone,
    {
        let subscriptions = subscriptions_lock.read().unwrap();
        for sink in subscriptions.values() {
            let _ = sink.notify(Ok(value.clone())).wait();
        }
    }
}

struct ManagementInterface<T: From<TunnelCommand> + 'static + Send> {
    subscriptions: Arc<ActiveSubscriptions>,
    tx: Mutex<IntoSender<TunnelCommand, T>>,
    shared_secret: String,
}

impl<T: From<TunnelCommand> + 'static + Send> ManagementInterface<T> {
    pub fn new(tx: IntoSender<TunnelCommand, T>, shared_secret: String) -> Self {
        ManagementInterface {
            subscriptions: Default::default(),
            tx: Mutex::new(tx),
            shared_secret,
        }
    }

    fn subscribe<V>(
        subscriber: pubsub::Subscriber<V>,
        subscriptions_lock: &RwLock<HashMap<SubscriptionId, pubsub::Sink<V>>>,
    ) {
        let mut subscriptions = subscriptions_lock.write().unwrap();
        loop {
            let id = SubscriptionId::String(uuid::Uuid::new_v4().to_string());
            if let Entry::Vacant(entry) = subscriptions.entry(id.clone()) {
                if let Ok(sink) = subscriber.assign_id(id.clone()) {
                    debug!("Accepting new subscription with id {:?}", id);
                    entry.insert(sink);
                }
                break;
            }
        }
    }

    fn unsubscribe<V>(
        id: SubscriptionId,
        subscriptions_lock: &RwLock<HashMap<SubscriptionId, pubsub::Sink<V>>>,
    ) -> BoxFuture<(), Error> {
        let was_removed = subscriptions_lock.write().unwrap().remove(&id).is_some();
        let result = if was_removed {
            debug!("Unsubscribing id {:?}", id);
            future::ok(())
        } else {
            future::err(Error {
                code: ErrorCode::InvalidParams,
                message: "Invalid subscription".to_owned(),
                data: None,
            })
        };
        Box::new(result)
    }

    /// Sends a command to the daemon and maps the error to an RPC error.
    fn send_command_to_daemon(&self, command: TunnelCommand) -> BoxFuture<(), Error> {
        Box::new(
            future::result(self.tx.lock().unwrap().send(command))
                .map_err(|_| Error::internal_error()),
        )
    }

    /// Converts the given error to an error that can be given to the caller of the API.
    /// Will let any actual RPC error through as is, any other error is changed to an internal
    /// error.
    fn map_rpc_error(error: jsonrpc_client_core::Error) -> Error {
        match error.kind() {
            &jsonrpc_client_core::ErrorKind::JsonRpcError(ref rpc_error) => {
                // We have to manually copy the error since we have different
                // versions of the jsonrpc_core library at the moment.
                Error {
                    code: ErrorCode::from(rpc_error.code.code()),
                    message: rpc_error.message.clone(),
                    data: rpc_error.data.clone(),
                }
            }
            _ => Error::internal_error(),
        }
    }

    fn check_auth(&self, meta: &Meta) -> Result<(), Error> {
        if meta.authenticated.load(Ordering::SeqCst) {
            trace!("auth success");
            Ok(())
        } else {
            trace!("auth failed");
            Err(Error::invalid_request())
        }
    }
}

/// Evaluates a Result and early returns an error.
/// If it is `Ok(val)`, evaluates to `val`.
/// If it is `Err(e)` it early returns `Box<Future>` where the future will result in `e`.
macro_rules! try_future {
    ($result:expr) => (match $result {
        ::std::result::Result::Ok(val) => val,
        ::std::result::Result::Err(e) => return Box::new(future::err(e)),
    });
}

impl<T: From<TunnelCommand> + 'static + Send> ManagementInterfaceApi for ManagementInterface<T> {
    type Metadata = Meta;

    fn auth(&self, meta: Self::Metadata, shared_secret: String) -> BoxFuture<(), Error> {
        let authenticated = shared_secret == self.shared_secret;
        meta.authenticated.store(authenticated, Ordering::SeqCst);
        debug!("authenticated: {}", authenticated);
        if authenticated {
            Box::new(future::ok(()))
        } else {
            Box::new(future::err(Error::internal_error()))
        }
    }

    fn get_account_data(
        &self,
        meta: Self::Metadata,
        account_token: AccountToken,
    ) -> BoxFuture<AccountData, Error> {
        trace!("get_account_data");
        try_future!(self.check_auth(&meta));
        let (tx, rx) = sync::oneshot::channel();
        let future = self.send_command_to_daemon(TunnelCommand::GetAccountData(tx, account_token))
            .and_then(|_| rx.map_err(|_| Error::internal_error()))
            .and_then(|rpc_future| {
                rpc_future.map_err(|error: jsonrpc_client_core::Error| {
                    error!(
                        "Unable to get account data from master: {}",
                        error.display_chain()
                    );
                    Self::map_rpc_error(error)
                })
            });
        Box::new(future)
    }

    fn get_countries(&self) -> Result<HashMap<CountryCode, String>, Error> {
        trace!("get_countries");
        Ok(HashMap::new())
    }

    fn set_account(
        &self,
        meta: Self::Metadata,
        account_token: Option<AccountToken>,
    ) -> BoxFuture<(), Error> {
        trace!("set_account");
        try_future!(self.check_auth(&meta));
        let (tx, rx) = sync::oneshot::channel();
        let future = self.send_command_to_daemon(TunnelCommand::SetAccount(tx, account_token))
            .and_then(|_| rx.map_err(|_| Error::internal_error()));
        Box::new(future)
    }

    fn get_account(&self, meta: Self::Metadata) -> BoxFuture<Option<AccountToken>, Error> {
        trace!("get_account");
        try_future!(self.check_auth(&meta));
        let (tx, rx) = sync::oneshot::channel();
        let future = self.send_command_to_daemon(TunnelCommand::GetAccount(tx))
            .and_then(|_| rx.map_err(|_| Error::internal_error()));
        Box::new(future)
    }

    fn set_custom_relay(
        &self,
        meta: Self::Metadata,
        custom_relay: RelayEndpoint,
    ) -> BoxFuture<(), Error> {
        trace!("set_custom_relay");
        try_future!(self.check_auth(&meta));
        let (tx, rx) = sync::oneshot::channel();

        let message = TunnelCommand::SetCustomRelay(tx, Some(custom_relay));
        let future = self.send_command_to_daemon(message)
            .and_then(|_| rx.map_err(|_| Error::internal_error()));
        Box::new(future)
    }

    fn remove_custom_relay(&self, meta: Self::Metadata) -> BoxFuture<(), Error> {
        trace!("remove_custom_relay");
        try_future!(self.check_auth(&meta));
        let (tx, rx) = sync::oneshot::channel();
        let future = self.send_command_to_daemon(TunnelCommand::SetCustomRelay(tx, None))
            .and_then(|_| rx.map_err(|_| Error::internal_error()));
        Box::new(future)
    }

    fn set_autoconnect(&self, meta: Self::Metadata, _autoconnect: bool) -> BoxFuture<(), Error> {
        trace!("set_autoconnect");
        try_future!(self.check_auth(&meta));
        Box::new(future::ok(()))
    }

    fn connect(&self, meta: Self::Metadata) -> BoxFuture<(), Error> {
        trace!("connect");
        try_future!(self.check_auth(&meta));
        self.send_command_to_daemon(TunnelCommand::SetTargetState(TargetState::Secured))
    }

    fn disconnect(&self, meta: Self::Metadata) -> BoxFuture<(), Error> {
        trace!("disconnect");
        try_future!(self.check_auth(&meta));
        self.send_command_to_daemon(TunnelCommand::SetTargetState(TargetState::Unsecured))
    }

    fn get_state(&self, meta: Self::Metadata) -> BoxFuture<DaemonState, Error> {
        trace!("get_state");
        try_future!(self.check_auth(&meta));
        let (state_tx, state_rx) = sync::oneshot::channel();
        let future = self.send_command_to_daemon(TunnelCommand::GetState(state_tx))
            .and_then(|_| state_rx.map_err(|_| Error::internal_error()));
        Box::new(future)
    }

    fn get_ip(&self) -> Result<IpAddr, Error> {
        trace!("get_ip");
        Ok(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)))
    }

    fn get_location(&self) -> Result<Location, Error> {
        trace!("get_location");
        Ok(Location {
            latlong: [1.0, 2.0],
            country: "narnia".to_owned(),
            city: "Le city".to_owned(),
        })
    }

    fn new_state_subscribe(
        &self,
        meta: Self::Metadata,
        subscriber: pubsub::Subscriber<DaemonState>,
    ) {
        trace!("new_state_subscribe");
        if self.check_auth(&meta).is_err() {
            return;
        }
        Self::subscribe(subscriber, &self.subscriptions.new_state_subscriptions);
    }

    fn new_state_unsubscribe(&self, id: SubscriptionId) -> BoxFuture<(), Error> {
        trace!("new_state_unsubscribe");
        Self::unsubscribe(id, &self.subscriptions.new_state_subscriptions)
    }

    fn error_subscribe(&self, meta: Self::Metadata, subscriber: pubsub::Subscriber<Vec<String>>) {
        trace!("error_subscribe");
        if self.check_auth(&meta).is_err() {
            return;
        }
        Self::subscribe(subscriber, &self.subscriptions.error_subscriptions);
    }

    fn error_unsubscribe(&self, id: SubscriptionId) -> BoxFuture<(), Error> {
        trace!("error_unsubscribe");
        Self::unsubscribe(id, &self.subscriptions.error_subscriptions)
    }
}


/// The metadata type. There is one instance associated with each connection. In this pubsub
/// scenario they are created by `meta_extractor` by the server on each new incoming
/// connection.
#[derive(Clone, Debug, Default)]
pub struct Meta {
    session: Option<Arc<Session>>,
    authenticated: Arc<AtomicBool>,
}

/// Make the `Meta` type possible to use as jsonrpc metadata type.
impl Metadata for Meta {}

/// Make the `Meta` type possible to use as a pubsub metadata type.
impl PubSubMetadata for Meta {
    fn session(&self) -> Option<Arc<Session>> {
        self.session.clone()
    }
}

/// Metadata extractor function for `Meta`.
fn meta_extractor(context: &jsonrpc_ws_server::RequestContext) -> Meta {
    Meta {
        session: Some(Arc::new(Session::new(context.sender()))),
        authenticated: Arc::new(AtomicBool::new(false)),
    }
}
