use core::any::Any;
use core::iter::{repeat, zip};
use std::collections::HashMap;
use std::ops::Deref;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context as _};
use async_nats::header::{IntoHeaderName as _, IntoHeaderValue as _};
use async_trait::async_trait;
use bytes::Bytes;
use secrecy::Secret;
use tokio::sync::RwLock;
use tracing::{error, instrument, warn};
use wasmcloud_runtime::capability::logging::logging;
use wasmcloud_runtime::capability::secrets::store::SecretValue;
use wasmcloud_runtime::capability::{
    self, messaging0_2_0, messaging0_3_0, secrets, CallTargetInterface,
};
use wasmcloud_runtime::component::{
    Bus, Bus1_0_0, Config, InvocationErrorIntrospect, InvocationErrorKind, Logging, Messaging0_2,
    Messaging0_3, MessagingClient0_3, MessagingGuestMessage0_3, MessagingHostMessage0_3,
    ReplacedInstanceTarget, Secrets,
};
use wasmcloud_tracing::context::TraceContextInjector;
use wrpc_transport::InvokeExt as _;
use wrpc_transport_nats::ParamWriter;

use super::config::ConfigBundle;
use super::{injector_to_headers, Features};

// Added for in-host invocation
use wasmcloud_core::ComponentId; // For ComponentId
use crate::wasmbus::Component; // For Component from crates/host/src/wasmbus/mod.rs

use wrpc_transport::Index;
use wrpc_transport_nats::Reader;

use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf};

#[derive(Clone, Debug)]
pub struct Handler {
    pub components: Arc<RwLock<HashMap<ComponentId, Arc<Component>>>>,
    pub nats: Arc<async_nats::Client>,
    // ConfigBundle is perfectly safe to pass around, but in order to update it on the fly, we need
    // to have it behind a lock since it can be cloned and because the `Actor` struct this gets
    // placed into is also inside of an Arc
    pub config_data: Arc<RwLock<ConfigBundle>>,
    /// Secrets are cached per-[`Handler`] so they can be used at runtime without consulting the secrets
    /// backend for each request. The [`SecretValue`] is wrapped in the [`Secret`] type from the `secrecy`
    /// crate to ensure that it is not accidentally logged or exposed in error messages.
    pub secrets: Arc<RwLock<HashMap<String, Secret<SecretValue>>>>,
    /// The lattice this handler will use for RPC
    pub lattice: Arc<str>,
    /// The identifier of the component that this handler is associated with
    pub component_id: Arc<str>,
    /// The current link targets. `instance` -> `link-name`
    /// Instance specification does not include a version
    pub targets: Arc<RwLock<HashMap<Box<str>, Arc<str>>>>,

    /// Map of link names -> instance -> Target
    ///
    /// While a target may often be a component ID, it is not guaranteed to be one, and could be
    /// some other identifier of where to send invocations, representing one or more lattice entities.
    ///
    /// Lattice entities could be:
    /// - A (single) Component ID
    /// - A routing group
    /// - Some other opaque string
    #[allow(clippy::type_complexity)]
    pub instance_links: Arc<RwLock<HashMap<Box<str>, HashMap<Box<str>, Box<str>>>>>,
    /// Link name -> messaging client
    pub messaging_links: Arc<RwLock<HashMap<Box<str>, async_nats::Client>>>,

    pub invocation_timeout: Duration,
    /// Experimental features enabled in the host for gating handler functionality
    pub experimental_features: Features,
}

impl Handler {
    /// Used for creating a new handler from an existing one. This is different than clone because
    /// some fields shouldn't be copied between component instances such as link targets.
    pub fn copy_for_new(&self) -> Self {
        Handler {
            components: self.components.clone(),
            nats: self.nats.clone(),
            config_data: self.config_data.clone(),
            secrets: self.secrets.clone(),
            lattice: self.lattice.clone(),
            component_id: self.component_id.clone(),
            targets: Arc::default(),
            instance_links: self.instance_links.clone(),
            messaging_links: self.messaging_links.clone(),
            invocation_timeout: self.invocation_timeout,
            experimental_features: self.experimental_features,
        }
    }
}

#[async_trait]
impl Bus1_0_0 for Handler {
    /// Set the current link name in use by the handler, which is otherwise "default".
    ///
    /// Link names are important to set to differentiate similar operations (ex. `wasi:keyvalue/store.get`)
    /// that should go to different targets (ex. a capability provider like `kv-redis` vs `kv-vault`)
    #[instrument(level = "debug", skip(self))]
    async fn set_link_name(&self, link_name: String, interfaces: Vec<Arc<CallTargetInterface>>) {
        let interfaces = interfaces.iter().map(Deref::deref);
        let mut targets = self.targets.write().await;
        if link_name == "default" {
            for CallTargetInterface {
                namespace,
                package,
                interface,
            } in interfaces
            {
                targets.remove(&format!("{namespace}:{package}/{interface}").into_boxed_str());
            }
        } else {
            let link_name = Arc::from(link_name);
            for CallTargetInterface {
                namespace,
                package,
                interface,
            } in interfaces
            {
                targets.insert(
                    format!("{namespace}:{package}/{interface}").into_boxed_str(),
                    Arc::clone(&link_name),
                );
            }
        }
    }
}

#[async_trait]
impl Bus for Handler {
    /// Set the current link name in use by the handler, which is otherwise "default".
    ///
    /// Link names are important to set to differentiate similar operations (ex. `wasi:keyvalue/store.get`)
    /// that should go to different targets (ex. a capability provider like `kv-redis` vs `kv-vault`)
    #[instrument(level = "debug", skip(self))]
    async fn set_link_name(
        &self,
        link_name: String,
        interfaces: Vec<Arc<CallTargetInterface>>,
    ) -> anyhow::Result<Result<(), String>> {
        let links = self.instance_links.read().await;
        // Ensure that all interfaces have an established link with the given name.
        if let Some(interface_missing_link) = interfaces.iter().find_map(|i| {
            let instance = i.as_instance();
            // This could be expressed in one line as a `!(bool).then_some`, but the negation makes it confusing
            if links
                .get(link_name.as_str())
                .and_then(|l| l.get(instance.as_str()))
                .is_none()
            {
                Some(instance)
            } else {
                None
            }
        }) {
            return Ok(Err(format!(
                "interface `{interface_missing_link}` does not have an existing link with name `{link_name}`"
            )));
        }
        // Explicitly drop the lock before calling `set_link_name` just to avoid holding the lock for longer than needed
        drop(links);

        Bus1_0_0::set_link_name(self, link_name, interfaces).await;
        Ok(Ok(()))
    }
}

#[derive(Debug)]
pub struct MyDuplexStream(pub tokio::io::DuplexStream);

impl AsyncRead for MyDuplexStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl AsyncWrite for MyDuplexStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, data)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

impl Index<MyDuplexStream> for MyDuplexStream {
    fn index(&self, _path: &[usize]) -> std::result::Result<MyDuplexStream, anyhow::Error> {
        Err(anyhow!("Index not implemented for MyDuplexStream"))
    }
}

pub enum EitherOutgoing {
    Local(MyDuplexStream),
    Remote(ParamWriter),
}

impl AsyncWrite for EitherOutgoing {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            EitherOutgoing::Local(ref mut local) => {
                let pinned = Pin::new(local);
                pinned.poll_write(cx, data)
            }
            EitherOutgoing::Remote(ref mut remote) => {
                let pinned = Pin::new(remote);
                pinned.poll_write(cx, data)
            }
        }
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            EitherOutgoing::Local(ref mut local) => {
                let pinned = Pin::new(local);
                pinned.poll_flush(cx)
            }
            EitherOutgoing::Remote(ref mut remote) => {
                let pinned = Pin::new(remote);
                pinned.poll_flush(cx)
            }
        }
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            EitherOutgoing::Local(ref mut local) => {
                let pinned = Pin::new(local);
                pinned.poll_shutdown(cx)
            }
            EitherOutgoing::Remote(ref mut remote) => {
                let pinned = Pin::new(remote);
                pinned.poll_shutdown(cx)
            }
        }
    }
}

impl Index<EitherOutgoing> for EitherOutgoing {
    fn index(
        &self,
        path: &[usize],
    ) -> std::result::Result<EitherOutgoing, anyhow::Error> {
        match self {
            EitherOutgoing::Local(local) => {
                let new_local = local.index(path)?;
                Ok(EitherOutgoing::Local(new_local))
            }
            EitherOutgoing::Remote(remote) => {
                let new_remote = remote.index(path)?;
                Ok(EitherOutgoing::Remote(new_remote))
            }
        }
    }
}

pub enum EitherIncoming {
    Local(MyDuplexStream),
    Remote(Reader),
}

impl AsyncRead for EitherIncoming {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            EitherIncoming::Local(ref mut local) => {
                let pinned = Pin::new(local);
                pinned.poll_read(cx, buf)
            }
            EitherIncoming::Remote(ref mut remote) => {
                let pinned = Pin::new(remote);
                pinned.poll_read(cx, buf)
            }
        }
    }
}

impl Index<EitherIncoming> for EitherIncoming {
    fn index(
        &self,
        path: &[usize],
    ) -> std::result::Result<EitherIncoming, anyhow::Error> {
        match self {
            EitherIncoming::Local(local) => {
                let new_local = local.index(path)?;
                Ok(EitherIncoming::Local(new_local))
            }
            EitherIncoming::Remote(remote) => {
                let new_remote = remote.index(path)?;
                Ok(EitherIncoming::Remote(new_remote))
            }
        }
    }
}

impl wrpc_transport::Invoke for Handler {
    type Context = Option<ReplacedInstanceTarget>;
    // type Outgoing = <wrpc_transport_nats::Client as wrpc_transport::Invoke>::Outgoing;
    // type Incoming = <wrpc_transport_nats::Client as wrpc_transport::Invoke>::Incoming;
    type Outgoing = EitherOutgoing;
    type Incoming = EitherIncoming;


    #[instrument(level = "debug", skip_all)]
    async fn invoke<P>(
        &self,
        target_instance: Self::Context,
        instance: &str,
        func: &str,
        params: Bytes,
        paths: impl AsRef<[P]> + Send,
    ) -> anyhow::Result<(Self::Outgoing, Self::Incoming)>
    where
        P: AsRef<[Option<usize>]> + Send + Sync,
    {
        let components = self.components.read().await;

        let links = self.instance_links.read().await;
        let targets = self.targets.read().await;
        

        let target_instance = match target_instance {
            Some(
                ReplacedInstanceTarget::BlobstoreBlobstore
                | ReplacedInstanceTarget::BlobstoreContainer,
            ) => "wasi:blobstore/blobstore",
            Some(ReplacedInstanceTarget::KeyvalueAtomics) => "wasi:keyvalue/atomics",
            Some(ReplacedInstanceTarget::KeyvalueStore) => "wasi:keyvalue/store",
            Some(ReplacedInstanceTarget::KeyvalueBatch) => "wasi:keyvalue/batch",
            Some(ReplacedInstanceTarget::HttpIncomingHandler) => "wasi:http/incoming-handler",
            Some(ReplacedInstanceTarget::HttpOutgoingHandler) => "wasi:http/outgoing-handler",
            None => instance.split_once('@').map_or(instance, |(l, _)| l),
        };

        let link_name = targets
            .get(target_instance)
            .map_or("default", AsRef::as_ref);

        let instances = links.get(link_name).with_context(|| {
            warn!(
                instance,
                link_name,
                ?target_instance,
                ?self.component_id,
                "no links with link name found for instance"
            );
            format!("link `{link_name}` not found for instance `{target_instance}`")
        })?;


        // Determine the lattice target ID we should be sending to
        let id = instances.get(target_instance).with_context(||{
            warn!(
                instance,
                ?target_instance,
                ?self.component_id,
                "component is not linked to a lattice target for the given instance"
            );
            format!("failed to call `{func}` in instance `{instance}` (failed to find a configured link with name `{link_name}` from component `{id}`, please check your configuration)", id = self.component_id)
        })?;


        if let Some(component) = components.get(id.as_ref()) {

            // Create two pairs of DuplexStreams
            // Use tokio::io::duplex instead of std::sync::mpsc::channel
            // To improve
            let (func_in_rx, func_in_tx) = tokio::io::duplex(1024);   // For the function's input
            let (func_out_rx, func_out_tx) = tokio::io::duplex(1024); // For the function's output

            // Wrap them with custom DuplexStream with Index
            // From the host’s perspective
            let mut outgoing_stream = MyDuplexStream(func_in_tx); // We write here, function reads
            let incoming_stream = MyDuplexStream(func_out_rx);    // Function writes here, we read

            // Write request bytes to the `outgoing_stream` so the function sees them
            outgoing_stream.write_all(&params).await?;
            
            // Shutdown the writer to indicate EOF to the callee
            outgoing_stream.shutdown().await?;

            // Call the function with (rx, tx) = (func_in_rx, func_out_tx)
            // The instantiate methods is a high-level constructor, which is different from the instantiate_async used in Instance::call.
            // The instantiate method doesn't do any Wasmtime store allocation or imports hooking up
            component
                .instantiate(component.handler.copy_for_new(), component.events.clone())
                .call(
                    instance,
                    func,
                    MyDuplexStream(func_in_rx),  // Function is reading from func_in_rx
                    MyDuplexStream(func_out_tx), // Function is writing to func_out_tx
                )
                .await?;

            // `outgoing_stream` is the host’s writer => EitherOutgoing::Local
            // `incoming_stream` is the host’s reader => EitherIncoming::Local
            return Ok((
                EitherOutgoing::Local(outgoing_stream),
                EitherIncoming::Local(incoming_stream),
            )); 
        }

        let mut headers = injector_to_headers(&TraceContextInjector::default_with_span());
        headers.insert("source-id", &*self.component_id);
        headers.insert("link-name", link_name);
        let nats = wrpc_transport_nats::Client::new(
            Arc::clone(&self.nats),
            format!("{}.{id}", &self.lattice),
            None,
        )
        .await?;
        let (remote_outgoing, remote_incoming) = nats
        .timeout(self.invocation_timeout)
        .invoke(Some(headers), instance, func, params, paths)
        .await?;

        // Wrap them in the `Remote` variant of the enums
        Ok((
            EitherOutgoing::Remote(remote_outgoing),
            EitherIncoming::Remote(remote_incoming),
        ))
    }
}


#[async_trait]
impl Config for Handler {
    #[instrument(level = "debug", skip_all)]
    async fn get(
        &self,
        key: &str,
    ) -> anyhow::Result<Result<Option<String>, capability::config::store::Error>> {
        let lock = self.config_data.read().await;
        let conf = lock.get_config().await;
        let data = conf.get(key).cloned();
        Ok(Ok(data))
    }

    #[instrument(level = "debug", skip_all)]
    async fn get_all(
        &self,
    ) -> anyhow::Result<Result<Vec<(String, String)>, capability::config::store::Error>> {
        Ok(Ok(self
            .config_data
            .read()
            .await
            .get_config()
            .await
            .clone()
            .into_iter()
            .collect()))
    }
}

#[async_trait]
impl Logging for Handler {
    #[instrument(level = "trace", skip(self))]
    async fn log(
        &self,
        level: logging::Level,
        context: String,
        message: String,
    ) -> anyhow::Result<()> {
        match level {
            logging::Level::Trace => {
                tracing::event!(
                    tracing::Level::TRACE,
                    component_id = ?self.component_id,
                    level = level.to_string(),
                    context,
                    "{message}"
                );
            }
            logging::Level::Debug => {
                tracing::event!(
                    tracing::Level::DEBUG,
                    component_id = ?self.component_id,
                    level = level.to_string(),
                    context,
                    "{message}"
                );
            }
            logging::Level::Info => {
                tracing::event!(
                    tracing::Level::INFO,
                    component_id = ?self.component_id,
                    level = level.to_string(),
                    context,
                    "{message}"
                );
            }
            logging::Level::Warn => {
                tracing::event!(
                    tracing::Level::WARN,
                    component_id = ?self.component_id,
                    level = level.to_string(),
                    context,
                    "{message}"
                );
            }
            logging::Level::Error => {
                tracing::event!(
                    tracing::Level::ERROR,
                    component_id = ?self.component_id,
                    level = level.to_string(),
                    context,
                    "{message}"
                );
            }
            logging::Level::Critical => {
                tracing::event!(
                    tracing::Level::ERROR,
                    component_id = ?self.component_id,
                    level = level.to_string(),
                    context,
                    "{message}"
                );
            }
        };
        Ok(())
    }
}

#[async_trait]
impl Secrets for Handler {
    #[instrument(level = "debug", skip_all)]
    async fn get(
        &self,
        key: &str,
    ) -> anyhow::Result<Result<secrets::store::Secret, secrets::store::SecretsError>> {
        if self.secrets.read().await.get(key).is_some() {
            Ok(Ok(Arc::new(key.to_string())))
        } else {
            Ok(Err(secrets::store::SecretsError::NotFound))
        }
    }

    async fn reveal(
        &self,
        secret: secrets::store::Secret,
    ) -> anyhow::Result<secrets::store::SecretValue> {
        let read_lock = self.secrets.read().await;
        let Some(secret_val) = read_lock.get(secret.as_str()) else {
            // NOTE(brooksmtownsend): This error case should never happen, since we check for existence during `get` and
            // fail to start the component if the secret is missing. We might hit this during wRPC testing with resources.
            const ERROR_MSG: &str = "secret not found to reveal, ensure the secret is declared and associated with this component at startup";
            // NOTE: This "secret" is just the name of the key, not the actual secret value. Regardless the secret itself
            // both wasn't found and is wrapped by `secrecy` so it won't be logged.
            error!(?secret, ERROR_MSG);
            bail!(ERROR_MSG)
        };
        use secrecy::ExposeSecret;
        Ok(secret_val.expose_secret().clone())
    }
}

impl Messaging0_2 for Handler {
    #[instrument(level = "debug", skip_all)]
    async fn request(
        &self,
        subject: String,
        body: Vec<u8>,
        timeout_ms: u32,
    ) -> anyhow::Result<Result<messaging0_2_0::types::BrokerMessage, String>> {
        use wasmcloud_runtime::capability::wrpc::wasmcloud::messaging0_2_0 as messaging;

        {
            let targets = self.targets.read().await;
            let target = targets
                .get("wasmcloud:messaging/consumer")
                .map(AsRef::as_ref)
                .unwrap_or("default");
            if let Some(nats) = self.messaging_links.read().await.get(target) {
                match nats.request(subject, body.into()).await {
                    Ok(async_nats::Message {
                        subject,
                        payload,
                        reply,
                        ..
                    }) => {
                        return Ok(Ok(messaging0_2_0::types::BrokerMessage {
                            subject: subject.into_string(),
                            body: payload.into(),
                            reply_to: reply.map(async_nats::Subject::into_string),
                        }))
                    }
                    Err(err) => return Ok(Err(err.to_string())),
                }
            }
        }

        match messaging::consumer::request(self, None, &subject, &Bytes::from(body), timeout_ms)
            .await?
        {
            Ok(messaging::types::BrokerMessage {
                subject,
                body,
                reply_to,
            }) => Ok(Ok(messaging0_2_0::types::BrokerMessage {
                subject,
                body: body.into(),
                reply_to,
            })),
            Err(err) => Ok(Err(err)),
        }
    }

    #[instrument(level = "debug", skip_all)]
    async fn publish(
        &self,
        messaging0_2_0::types::BrokerMessage {
            subject,
            body,
            reply_to,
        }: messaging0_2_0::types::BrokerMessage,
    ) -> anyhow::Result<Result<(), String>> {
        use wasmcloud_runtime::capability::wrpc::wasmcloud::messaging0_2_0 as messaging;

        {
            let targets = self.targets.read().await;
            let target = targets
                .get("wasmcloud:messaging/consumer")
                .map(AsRef::as_ref)
                .unwrap_or("default");
            if let Some(nats) = self.messaging_links.read().await.get(target) {
                if let Some(reply_to) = reply_to {
                    match nats
                        .publish_with_reply(subject, reply_to, body.into())
                        .await
                    {
                        Ok(()) => return Ok(Ok(())),
                        Err(err) => return Ok(Err(err.to_string())),
                    }
                }
                match nats.publish(subject, body.into()).await {
                    Ok(()) => return Ok(Ok(())),
                    Err(err) => return Ok(Err(err.to_string())),
                }
            }
        }

        messaging::consumer::publish(
            self,
            None,
            &messaging::types::BrokerMessage {
                subject,
                body: body.into(),
                reply_to,
            },
        )
        .await
    }
}

struct MessagingClient {
    name: Box<str>,
}

#[async_trait]
impl MessagingClient0_3 for MessagingClient {
    async fn disconnect(&mut self) -> anyhow::Result<Result<(), messaging0_3_0::types::Error>> {
        Ok(Ok(()))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Concrete implementation of a message originating directly from the host, i.e. not received via
/// wRPC.
enum Message {
    Nats(async_nats::Message),
}

#[async_trait]
impl MessagingHostMessage0_3 for Message {
    async fn topic(&self) -> anyhow::Result<Option<messaging0_3_0::types::Topic>> {
        match self {
            Message::Nats(async_nats::Message { subject, .. }) => Ok(Some(subject.to_string())),
        }
    }
    async fn content_type(&self) -> anyhow::Result<Option<String>> {
        Ok(None)
    }
    async fn set_content_type(&mut self, _content_type: String) -> anyhow::Result<()> {
        bail!("`content-type` not supported")
    }
    async fn data(&self) -> anyhow::Result<Vec<u8>> {
        match self {
            Message::Nats(async_nats::Message { payload, .. }) => Ok(payload.to_vec()),
        }
    }
    async fn set_data(&mut self, buf: Vec<u8>) -> anyhow::Result<()> {
        match self {
            Message::Nats(msg) => {
                msg.payload = buf.into();
            }
        }
        Ok(())
    }
    async fn metadata(&self) -> anyhow::Result<Option<messaging0_3_0::types::Metadata>> {
        match self {
            Message::Nats(async_nats::Message { headers: None, .. }) => Ok(None),
            Message::Nats(async_nats::Message {
                headers: Some(headers),
                ..
            }) => Ok(Some(headers.iter().fold(
                // TODO: Initialize vector with capacity, once `async-nats` is updated to 0.37,
                // where `len` method is introduced:
                // https://docs.rs/async-nats/0.37.0/async_nats/header/struct.HeaderMap.html#method.len
                //Vec::with_capacity(headers.len()),
                Vec::default(),
                |mut headers, (k, vs)| {
                    for v in vs {
                        headers.push((k.to_string(), v.to_string()))
                    }
                    headers
                },
            ))),
        }
    }
    async fn add_metadata(&mut self, key: String, value: String) -> anyhow::Result<()> {
        match self {
            Message::Nats(async_nats::Message {
                headers: Some(headers),
                ..
            }) => {
                headers.append(key, value);
                Ok(())
            }
            Message::Nats(async_nats::Message { headers, .. }) => {
                *headers = Some(async_nats::HeaderMap::from_iter([(
                    key.into_header_name(),
                    value.into_header_value(),
                )]));
                Ok(())
            }
        }
    }
    async fn set_metadata(&mut self, meta: messaging0_3_0::types::Metadata) -> anyhow::Result<()> {
        match self {
            Message::Nats(async_nats::Message { headers, .. }) => {
                *headers = Some(
                    meta.into_iter()
                        .map(|(k, v)| (k.into_header_name(), v.into_header_value()))
                        .collect(),
                );
                Ok(())
            }
        }
    }
    async fn remove_metadata(&mut self, key: String) -> anyhow::Result<()> {
        match self {
            Message::Nats(async_nats::Message {
                headers: Some(headers),
                ..
            }) => {
                *headers = headers
                    .iter()
                    .filter(|(k, ..)| (k.as_ref() != key))
                    .flat_map(|(k, vs)| zip(repeat(k.clone()), vs.iter().cloned()))
                    .collect();
                Ok(())
            }
            Message::Nats(..) => Ok(()),
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

impl Messaging0_3 for Handler {
    #[instrument(level = "debug", skip_all)]
    async fn connect(
        &self,
        name: String,
    ) -> anyhow::Result<
        Result<Box<dyn MessagingClient0_3 + Send + Sync>, messaging0_3_0::types::Error>,
    > {
        Ok(Ok(Box::new(MessagingClient {
            name: name.into_boxed_str(),
        })))
    }

    #[instrument(level = "debug", skip_all)]
    async fn send(
        &self,
        client: &(dyn MessagingClient0_3 + Send + Sync),
        topic: messaging0_3_0::types::Topic,
        message: messaging0_3_0::types::Message,
    ) -> anyhow::Result<Result<(), messaging0_3_0::types::Error>> {
        use wasmcloud_runtime::capability::wrpc::wasmcloud::messaging0_2_0 as messaging;

        let MessagingClient { name } = client
            .as_any()
            .downcast_ref()
            .context("unknown client type")?;
        {
            let targets = self.targets.read().await;
            let target = targets
                .get("wasmcloud:messaging/producer")
                .map(AsRef::as_ref)
                .unwrap_or("default");
            let name = if name.is_empty() {
                "default"
            } else {
                name.as_ref()
            };
            if name != target {
                return Ok(Err(messaging0_3_0::types::Error::Other(format!(
                    "mismatch between link name and client connection name, `{name}` != `{target}`"
                ))));
            }
            if let Some(nats) = self.messaging_links.read().await.get(target) {
                match match message {
                    messaging0_3_0::types::Message::Host(message) => {
                        let message = message
                            .into_any()
                            .downcast::<Message>()
                            .map_err(|_| anyhow!("unknown message type"))?;
                        match *message {
                            Message::Nats(async_nats::Message {
                                payload,
                                headers: Some(headers),
                                ..
                            }) => nats.publish_with_headers(topic, headers, payload).await,
                            Message::Nats(async_nats::Message { payload, .. }) => {
                                nats.publish(topic, payload).await
                            }
                        }
                    }
                    messaging0_3_0::types::Message::Wrpc(messaging::types::BrokerMessage {
                        body,
                        ..
                    }) => nats.publish(topic, body).await,
                    messaging0_3_0::types::Message::Guest(MessagingGuestMessage0_3 {
                        content_type,
                        data,
                        metadata,
                    }) => {
                        if let Some(content_type) = content_type {
                            warn!(
                                content_type,
                                "`content-type` not supported by NATS.io, value is ignored"
                            );
                        }
                        if let Some(metadata) = metadata {
                            nats.publish_with_headers(
                                topic,
                                metadata
                                    .into_iter()
                                    .map(|(k, v)| (k.into_header_name(), v.into_header_value()))
                                    .collect(),
                                data.into(),
                            )
                            .await
                        } else {
                            nats.publish(topic, data.into()).await
                        }
                    }
                } {
                    Ok(()) => return Ok(Ok(())),
                    Err(err) => {
                        // TODO: Correctly handle error kind
                        return Ok(Err(messaging0_3_0::types::Error::Other(err.to_string())));
                    }
                }
            }
            let body = match message {
                messaging0_3_0::types::Message::Host(message) => {
                    let message = message
                        .into_any()
                        .downcast::<Message>()
                        .map_err(|_| anyhow!("unknown message type"))?;
                    match *message {
                        Message::Nats(async_nats::Message {
                            headers: Some(..), ..
                        }) => {
                            return Ok(Err(messaging0_3_0::types::Error::Other(
                                "headers not currently supported by wRPC targets".into(),
                            )));
                        }
                        Message::Nats(async_nats::Message { payload, .. }) => payload,
                    }
                }
                messaging0_3_0::types::Message::Wrpc(messaging::types::BrokerMessage {
                    body,
                    ..
                }) => body,
                messaging0_3_0::types::Message::Guest(MessagingGuestMessage0_3 {
                    metadata: Some(..),
                    ..
                }) => {
                    return Ok(Err(messaging0_3_0::types::Error::Other(
                        "`metadata` not currently supported by wRPC targets".into(),
                    )));
                }
                messaging0_3_0::types::Message::Guest(MessagingGuestMessage0_3 {
                    content_type,
                    data,
                    ..
                }) => {
                    if let Some(content_type) = content_type {
                        warn!(
                            content_type,
                            "`content-type` not currently supported by wRPC targets, value is ignored",
                        );
                    }
                    data.into()
                }
            };
            match messaging::consumer::publish(
                self,
                None,
                &messaging::types::BrokerMessage {
                    subject: topic,
                    body,
                    reply_to: None,
                },
            )
            .await
            {
                Ok(Ok(())) => Ok(Ok(())),
                Ok(Err(err)) => Ok(Err(messaging0_3_0::types::Error::Other(err))),
                // TODO: Correctly handle error kind
                Err(err) => Ok(Err(messaging0_3_0::types::Error::Other(err.to_string()))),
            }
        }
    }

    #[instrument(level = "debug", skip_all)]
    async fn request(
        &self,
        client: &(dyn MessagingClient0_3 + Send + Sync),
        topic: messaging0_3_0::types::Topic,
        message: &messaging0_3_0::types::Message,
        options: Option<messaging0_3_0::request_reply::RequestOptions>,
    ) -> anyhow::Result<
        Result<Vec<Box<dyn MessagingHostMessage0_3 + Send + Sync>>, messaging0_3_0::types::Error>,
    > {
        if options.is_some() {
            return Ok(Err(messaging0_3_0::types::Error::Other(
                "`options` not currently supported".into(),
            )));
        }

        use wasmcloud_runtime::capability::wrpc::wasmcloud::messaging0_2_0 as messaging;

        let MessagingClient { name } = client
            .as_any()
            .downcast_ref()
            .context("unknown client type")?;
        {
            let targets = self.targets.read().await;
            let target = targets
                .get("wasmcloud:messaging/request-reply")
                .map(AsRef::as_ref)
                .unwrap_or("default");
            let name = if name.is_empty() {
                "default"
            } else {
                name.as_ref()
            };
            if name != target {
                return Ok(Err(messaging0_3_0::types::Error::Other(format!(
                    "mismatch between link name and client connection name, `{name}` != `{target}`"
                ))));
            }
            if let Some(nats) = self.messaging_links.read().await.get(target) {
                match match message {
                    messaging0_3_0::types::Message::Host(message) => {
                        let message = message
                            .as_any()
                            .downcast_ref::<Message>()
                            .context("unknown message type")?;
                        match message {
                            Message::Nats(async_nats::Message {
                                payload,
                                headers: Some(headers),
                                ..
                            }) => {
                                nats.request_with_headers(topic, headers.clone(), payload.clone())
                                    .await
                            }
                            Message::Nats(async_nats::Message { payload, .. }) => {
                                nats.request(topic, payload.clone()).await
                            }
                        }
                    }
                    messaging0_3_0::types::Message::Wrpc(messaging::types::BrokerMessage {
                        body,
                        ..
                    }) => nats.request(topic, body.clone()).await,
                    messaging0_3_0::types::Message::Guest(MessagingGuestMessage0_3 {
                        content_type,
                        data,
                        metadata,
                    }) => {
                        if let Some(content_type) = content_type {
                            warn!(
                                content_type,
                                "`content-type` not supported by NATS.io, value is ignored"
                            );
                        }
                        if let Some(metadata) = metadata {
                            nats.request_with_headers(
                                topic,
                                metadata
                                    .iter()
                                    .map(|(k, v)| {
                                        (
                                            k.as_str().into_header_name(),
                                            v.as_str().into_header_value(),
                                        )
                                    })
                                    .collect(),
                                Bytes::copy_from_slice(data),
                            )
                            .await
                        } else {
                            nats.request(topic, Bytes::copy_from_slice(data)).await
                        }
                    }
                } {
                    Ok(msg) => return Ok(Ok(vec![Box::new(Message::Nats(msg))])),
                    Err(err) => {
                        // TODO: Correctly handle error kind
                        return Ok(Err(messaging0_3_0::types::Error::Other(err.to_string())));
                    }
                }
            }
            let body = match message {
                messaging0_3_0::types::Message::Host(message) => {
                    let message = message
                        .as_any()
                        .downcast_ref::<Message>()
                        .context("unknown message type")?;
                    match message {
                        Message::Nats(async_nats::Message {
                            headers: Some(..), ..
                        }) => {
                            return Ok(Err(messaging0_3_0::types::Error::Other(
                                "headers not currently supported by wRPC targets".into(),
                            )));
                        }
                        Message::Nats(async_nats::Message { payload, .. }) => payload.clone(),
                    }
                }
                messaging0_3_0::types::Message::Wrpc(messaging::types::BrokerMessage {
                    body,
                    ..
                }) => body.clone(),
                messaging0_3_0::types::Message::Guest(MessagingGuestMessage0_3 {
                    metadata: Some(..),
                    ..
                }) => {
                    return Ok(Err(messaging0_3_0::types::Error::Other(
                        "`metadata` not currently supported by wRPC targets".into(),
                    )));
                }
                messaging0_3_0::types::Message::Guest(MessagingGuestMessage0_3 {
                    content_type,
                    data,
                    ..
                }) => {
                    if let Some(content_type) = content_type {
                        warn!(
                            content_type,
                            "`content-type` not currently supported by wRPC targets, value is ignored",
                        );
                    }
                    Bytes::copy_from_slice(data)
                }
            };

            match messaging::consumer::publish(
                self,
                None,
                &messaging::types::BrokerMessage {
                    subject: topic,
                    body,
                    reply_to: None,
                },
            )
            .await
            {
                Ok(Ok(())) => Ok(Err(messaging0_3_0::types::Error::Other(
                    "message sent, but returning responses is not currently supported by wRPC targets".into(),
                ))),
                Ok(Err(err)) => Ok(Err(messaging0_3_0::types::Error::Other(err))),
                // TODO: Correctly handle error kind
                Err(err) => Ok(Err(messaging0_3_0::types::Error::Other(err.to_string()))),
            }
        }
    }

    #[instrument(level = "debug", skip_all)]
    async fn reply(
        &self,
        reply_to: &messaging0_3_0::types::Message,
        message: messaging0_3_0::types::Message,
    ) -> anyhow::Result<Result<(), messaging0_3_0::types::Error>> {
        use wasmcloud_runtime::capability::wrpc::wasmcloud::messaging0_2_0 as messaging;

        {
            let targets = self.targets.read().await;
            let target = targets
                .get("wasmcloud:messaging/request-reply")
                .map(AsRef::as_ref)
                .unwrap_or("default");
            if let Some(nats) = self.messaging_links.read().await.get(target) {
                let subject = match reply_to {
                    messaging0_3_0::types::Message::Host(reply_to) => {
                        match reply_to
                            .as_any()
                            .downcast_ref::<Message>()
                            .context("unknown message type")?
                        {
                            Message::Nats(async_nats::Message {
                                reply: Some(reply), ..
                            }) => reply.clone(),
                            Message::Nats(async_nats::Message { reply: None, .. }) => {
                                return Ok(Err(messaging0_3_0::types::Error::Other(
                                    "reply not set in incoming NATS.io message".into(),
                                )))
                            }
                        }
                    }
                    messaging0_3_0::types::Message::Wrpc(messaging::types::BrokerMessage {
                        reply_to: Some(reply_to),
                        ..
                    }) => reply_to.as_str().into(),
                    messaging0_3_0::types::Message::Wrpc(messaging::types::BrokerMessage {
                        reply_to: None,
                        ..
                    }) => {
                        return Ok(Err(messaging0_3_0::types::Error::Other(
                            "reply not set in incoming wRPC message".into(),
                        )))
                    }
                    messaging0_3_0::types::Message::Guest(..) => {
                        return Ok(Err(messaging0_3_0::types::Error::Other(
                            "cannot reply to guest message".into(),
                        )))
                    }
                };
                match match message {
                    messaging0_3_0::types::Message::Host(message) => {
                        let message = message
                            .into_any()
                            .downcast::<Message>()
                            .map_err(|_| anyhow!("unknown message type"))?;
                        match *message {
                            Message::Nats(async_nats::Message {
                                payload,
                                headers: Some(headers),
                                ..
                            }) => nats.publish_with_headers(subject, headers, payload).await,
                            Message::Nats(async_nats::Message { payload, .. }) => {
                                nats.publish(subject, payload).await
                            }
                        }
                    }
                    messaging0_3_0::types::Message::Wrpc(messaging::types::BrokerMessage {
                        body,
                        ..
                    }) => nats.publish(subject, body).await,
                    messaging0_3_0::types::Message::Guest(MessagingGuestMessage0_3 {
                        content_type,
                        data,
                        metadata,
                    }) => {
                        if let Some(content_type) = content_type {
                            warn!(
                                content_type,
                                "`content-type` not supported by NATS.io, value is ignored"
                            );
                        }
                        if let Some(metadata) = metadata {
                            nats.publish_with_headers(
                                subject,
                                metadata
                                    .into_iter()
                                    .map(|(k, v)| (k.into_header_name(), v.into_header_value()))
                                    .collect(),
                                data.into(),
                            )
                            .await
                        } else {
                            nats.publish(subject, data.into()).await
                        }
                    }
                } {
                    Ok(()) => return Ok(Ok(())),
                    Err(err) => {
                        // TODO: Correctly handle error kind
                        return Ok(Err(messaging0_3_0::types::Error::Other(err.to_string())));
                    }
                }
            }
            let body = match message {
                messaging0_3_0::types::Message::Host(message) => {
                    let message = message
                        .into_any()
                        .downcast::<Message>()
                        .map_err(|_| anyhow!("unknown message type"))?;
                    match *message {
                        Message::Nats(async_nats::Message {
                            headers: Some(..), ..
                        }) => {
                            return Ok(Err(messaging0_3_0::types::Error::Other(
                                "headers not currently supported by wRPC targets".into(),
                            )));
                        }
                        Message::Nats(async_nats::Message { payload, .. }) => payload,
                    }
                }
                messaging0_3_0::types::Message::Wrpc(messaging::types::BrokerMessage {
                    body,
                    ..
                }) => body,
                messaging0_3_0::types::Message::Guest(MessagingGuestMessage0_3 {
                    metadata: Some(..),
                    ..
                }) => {
                    return Ok(Err(messaging0_3_0::types::Error::Other(
                        "`metadata` not currently supported by wRPC targets".into(),
                    )));
                }
                messaging0_3_0::types::Message::Guest(MessagingGuestMessage0_3 {
                    content_type,
                    data,
                    ..
                }) => {
                    if let Some(content_type) = content_type {
                        warn!(
                            content_type,
                            "`content-type` not currently supported by wRPC targets, value is ignored",
                        );
                    }
                    data.into()
                }
            };
            let subject = match reply_to {
                messaging0_3_0::types::Message::Host(reply_to) => {
                    match reply_to
                        .as_any()
                        .downcast_ref::<Message>()
                        .context("unknown message type")?
                    {
                        Message::Nats(async_nats::Message {
                            reply: Some(reply), ..
                        }) => reply.to_string(),
                        Message::Nats(async_nats::Message { reply: None, .. }) => {
                            return Ok(Err(messaging0_3_0::types::Error::Other(
                                "reply not set in incoming NATS.io message".into(),
                            )))
                        }
                    }
                }
                messaging0_3_0::types::Message::Wrpc(messaging::types::BrokerMessage {
                    reply_to: Some(reply_to),
                    ..
                }) => reply_to.clone(),
                messaging0_3_0::types::Message::Wrpc(messaging::types::BrokerMessage {
                    reply_to: None,
                    ..
                }) => {
                    return Ok(Err(messaging0_3_0::types::Error::Other(
                        "reply not set in incoming wRPC message".into(),
                    )))
                }
                messaging0_3_0::types::Message::Guest(..) => {
                    return Ok(Err(messaging0_3_0::types::Error::Other(
                        "cannot reply to guest message".into(),
                    )))
                }
            };
            match messaging::consumer::publish(
                self,
                None,
                &messaging::types::BrokerMessage {
                    subject,
                    body,
                    reply_to: None,
                },
            )
            .await
            {
                Ok(Ok(())) => Ok(Ok(())),
                Ok(Err(err)) => Ok(Err(messaging0_3_0::types::Error::Other(err))),
                // TODO: Correctly handle error kind
                Err(err) => Ok(Err(messaging0_3_0::types::Error::Other(err.to_string()))),
            }
        }
    }
}

impl InvocationErrorIntrospect for Handler {
    fn invocation_error_kind(&self, err: &anyhow::Error) -> InvocationErrorKind {
        if let Some(err) = err.root_cause().downcast_ref::<std::io::Error>() {
            if err.kind() == std::io::ErrorKind::NotConnected {
                return InvocationErrorKind::NotFound;
            }
        }
        InvocationErrorKind::Trap
    }
}
