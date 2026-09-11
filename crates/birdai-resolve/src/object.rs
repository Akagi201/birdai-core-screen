//! Object, checkpoint and dynamic-field access.
//!
//! The only backend shipped here is gRPC v2, because it returns a native
//! [`sui_types::object::Object`] — the same type the checkpoint stream carries — so the crate that
//! consumes objects does not care whether they came from the wire or from a checkpoint. Everything
//! else (GraphQL, fixtures, a validator's object store) is a matter of implementing
//! [`ObjectSource`].

use async_trait::async_trait;
use bytes::Bytes;
use sui_rpc_api::{Client, client::HeadersInterceptor};
use sui_types::{
    base_types::{ObjectID, SequenceNumber},
    full_checkpoint_content::Checkpoint,
    object::Object,
};
use tonic::Code;

use crate::error::ResolveError;

/// How many dynamic fields to request per page.
pub const DYNAMIC_FIELD_PAGE_SIZE: u32 = 50;

/// The UID a dynamic field is attached to.
///
/// Sui sets a dynamic field's `Owner::ObjectOwner` to the **UID the field was added to**, which for
/// a container inlined in another object is an inner UID rather than the containing object. That is
/// exactly the Cetus tick skip list's case, and it is why `object(address: POOL) { dynamicFields }`
/// returns nothing for it.
#[must_use]
pub fn dynamic_field_parent(object: &Object) -> Option<ObjectID> {
    match object.owner() {
        sui_types::object::Owner::ObjectOwner(parent) => Some(ObjectID::from(*parent)),
        _ => None,
    }
}

/// A child object of a dynamic-field container.
///
/// `parent` is the id of the **UID the field was attached to**, which for a container inlined in
/// another object is an inner UID rather than the containing object. The tick skip list inside a
/// Cetus pool is exactly this case: `object(address: POOL) { dynamicFields }` is empty and the
/// nodes are only reachable through the inner UID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DynamicFieldRef {
    /// The UID the field is attached to.
    pub parent: ObjectID,
    /// The id of the field object itself.
    pub field_id: ObjectID,
}

/// One page of dynamic fields.
#[derive(Debug, Clone)]
pub struct DynamicFieldPage {
    /// The fields in this page.
    pub entries: Vec<DynamicFieldRef>,
    /// Token to pass back for the next page.
    pub next: Option<Bytes>,
}

/// Where raw objects come from.
#[async_trait]
pub trait ObjectSource: Send + Sync {
    /// Fetch an object, optionally pinned to a version.
    async fn object(&self, id: ObjectID, version: Option<u64>) -> Result<Object, ResolveError>;

    /// Fetch a whole checkpoint, including its deduplicated object set.
    async fn checkpoint(&self, sequence_number: u64) -> Result<Checkpoint, ResolveError>;

    /// List the dynamic fields attached to `parent`, one page at a time.
    async fn dynamic_fields(
        &self,
        parent: ObjectID,
        cursor: Option<Bytes>,
    ) -> Result<DynamicFieldPage, ResolveError>;

    /// The chain the node is serving.
    async fn chain_id(&self) -> Result<String, ResolveError>;

    /// The node's latest checkpoint height.
    async fn latest_checkpoint(&self) -> Result<u64, ResolveError>;

    /// Fetch several objects at their latest versions.
    ///
    /// The default implementation is sequential; backends should override it when they can do
    /// better. Loading a Cetus pool's 650-odd tick nodes one round trip at a time is the difference
    /// between a second and a minute.
    async fn objects(&self, ids: &[ObjectID]) -> Result<Vec<Object>, ResolveError> {
        let mut objects = Vec::with_capacity(ids.len());
        for id in ids {
            objects.push(self.object(*id, None).await?);
        }
        Ok(objects)
    }
}

/// A [`ObjectSource`] backed by a Sui fullnode's gRPC v2 API.
#[derive(Clone)]
pub struct GrpcObjectSource {
    client: Client,
    /// Endpoint used only for `Checkpoint` reads, when one is configured.
    checkpoint_client: Option<Client>,
}

impl std::fmt::Debug for GrpcObjectSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GrpcObjectSource").finish_non_exhaustive()
    }
}

impl GrpcObjectSource {
    /// Connect to a fullnode, e.g. `https://fullnode.mainnet.sui.io:443`.
    pub fn new(url: &str) -> Result<Self, ResolveError> {
        Self::with_api_key(url, None)
    }

    /// Connect to a fullnode that wants an API key in an `x-api-key` header.
    ///
    /// Some hosted providers put the key in the URL path, which a gRPC client cannot use — tonic
    /// builds `/<service>/<method>` request paths — so the key has to travel as metadata.
    pub fn with_api_key(url: &str, api_key: Option<&str>) -> Result<Self, ResolveError> {
        Self::with_endpoints(url, api_key, None)
    }

    /// Connect, and read `Checkpoint`s from a separate archival endpoint.
    ///
    /// See the type's documentation for why the two cannot be merged.
    pub fn with_endpoints(
        url: &str,
        api_key: Option<&str>,
        checkpoint_url: Option<&str>,
    ) -> Result<Self, ResolveError> {
        let client = Self::connect(url, api_key)?;
        // An empty value means "no separate endpoint", so `--archive-url ""` works as a way to opt
        // out. Pointing it at the same host would only double the work.
        let checkpoint_client = match checkpoint_url {
            Some(checkpoint_url) if !checkpoint_url.is_empty() && checkpoint_url != url => {
                Some(Self::connect(checkpoint_url, api_key)?)
            }
            _ => None,
        };
        Ok(Self { client, checkpoint_client })
    }

    fn connect(url: &str, api_key: Option<&str>) -> Result<Client, ResolveError> {
        let mut client = Client::new(url)
            .map_err(|error| ResolveError::Rpc { call: "connect", message: error.to_string() })?;
        if let Some(api_key) = api_key {
            let value = tonic::metadata::MetadataValue::try_from(api_key).map_err(|error| {
                ResolveError::Rpc { call: "api key", message: error.to_string() }
            })?;
            let mut headers = HeadersInterceptor::new();
            // Providers disagree on where the key goes. Sending all three is harmless — an unknown
            // header is ignored — and avoids a per-provider flag.
            for name in [API_KEY_HEADER, "x-token-id"] {
                headers.headers_mut().insert(name, value.clone());
            }
            headers.bearer_auth(api_key);
            client = client.with_headers(headers);
        }
        Ok(client)
    }

    /// The client, for callers that need a Sui API this crate does not wrap.
    #[must_use]
    pub const fn client(&self) -> &Client {
        &self.client
    }

    /// Whether checkpoint reads are routed to a separate endpoint.
    #[must_use]
    pub const fn has_checkpoint_endpoint(&self) -> bool {
        self.checkpoint_client.is_some()
    }
}

/// The header hosted providers conventionally use for Sui gRPC keys.
pub const API_KEY_HEADER: &str = "x-api-key";

fn rpc_error(call: &'static str, status: tonic::Status) -> ResolveError {
    ResolveError::Rpc { call, message: status.to_string() }
}

#[async_trait]
impl ObjectSource for GrpcObjectSource {
    async fn object(&self, id: ObjectID, version: Option<u64>) -> Result<Object, ResolveError> {
        // `Client` methods that mutate take `&mut self`; cloning the handle is cheap and keeps the
        // source usable from many threads without a lock.
        let mut client = self.client.clone();
        let result = match version {
            Some(version) => {
                client.get_object_with_version(id, SequenceNumber::from_u64(version)).await
            }
            None => client.get_object(id).await,
        };
        match result {
            Ok(object) => Ok(object),
            Err(status) if status.code() == Code::NotFound => {
                Err(ResolveError::ObjectNotFound { id: id.to_canonical_string(true), version })
            }
            Err(status) => Err(rpc_error("get_object", status)),
        }
    }

    async fn checkpoint(&self, sequence_number: u64) -> Result<Checkpoint, ResolveError> {
        // Prefer the archival endpoint when one is configured: full nodes prune checkpoint data, so
        // an old checkpoint on a fullnode is either missing or, under load, transiently
        // unavailable.
        async fn fetch(client: &Client, sequence_number: u64) -> Result<Checkpoint, ResolveError> {
            let mut client = client.clone();
            client.get_full_checkpoint(sequence_number).await.map_err(|status| {
                if status.code() == Code::NotFound {
                    ResolveError::ObjectNotFound {
                        id: format!("checkpoint {sequence_number}"),
                        version: None,
                    }
                } else {
                    rpc_error("get_full_checkpoint", status)
                }
            })
        }

        let Some(archival) = &self.checkpoint_client else {
            return fetch(&self.client, sequence_number).await;
        };
        match fetch(archival, sequence_number).await {
            Ok(checkpoint) => Ok(checkpoint),
            Err(error) => {
                // The archival endpoint failed; try the full node before giving up.
                tracing::debug!(
                    checkpoint = sequence_number,
                    %error,
                    "archival checkpoint read failed; retrying on the full node"
                );
                fetch(&self.client, sequence_number).await
            }
        }
    }

    async fn dynamic_fields(
        &self,
        parent: ObjectID,
        cursor: Option<Bytes>,
    ) -> Result<DynamicFieldPage, ResolveError> {
        let response = self
            .client
            .get_dynamic_fields(parent, Some(DYNAMIC_FIELD_PAGE_SIZE), cursor)
            .await
            .map_err(|status| rpc_error("get_dynamic_fields", status))?;

        let mut entries = Vec::with_capacity(response.dynamic_fields.len());
        for field in &response.dynamic_fields {
            let Some(text) = field.field_id.as_deref() else {
                continue;
            };
            let field_id = text.parse::<ObjectID>().map_err(|_| ResolveError::Unparsable {
                what: "dynamic field id",
                value: text.to_owned(),
            })?;
            entries.push(DynamicFieldRef { parent, field_id });
        }

        Ok(DynamicFieldPage { entries, next: response.next_page_token })
    }

    async fn chain_id(&self) -> Result<String, ResolveError> {
        let identifier = self
            .client
            .get_chain_identifier()
            .await
            .map_err(|status| rpc_error("get_chain_identifier", status))?;
        Ok(format!("{identifier:?}"))
    }

    async fn latest_checkpoint(&self) -> Result<u64, ResolveError> {
        let mut client = self.client.clone();
        let summary = client
            .get_latest_checkpoint()
            .await
            .map_err(|status| rpc_error("get_latest_checkpoint", status))?;
        Ok(summary.sequence_number)
    }

    async fn objects(&self, ids: &[ObjectID]) -> Result<Vec<Object>, ResolveError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }

        // Batching is far fewer round trips, but `batch_get_objects` collapses a single missing
        // object into a wholesale error — and a missing object is not exotic: enumerating a pool's
        // 650-odd tick nodes and then fetching them takes long enough that a tick can be removed in
        // between. So a batch failure is retried object by object, skipping anything that is gone.
        match self.client.batch_get_objects(ids).await {
            Ok(objects) => Ok(objects),
            Err(_) => self.objects_individually(ids).await,
        }
    }
}

impl GrpcObjectSource {
    async fn objects_individually(&self, ids: &[ObjectID]) -> Result<Vec<Object>, ResolveError> {
        let mut objects = Vec::with_capacity(ids.len());
        for id in ids {
            match self.client.clone().get_object(*id).await {
                Ok(object) => objects.push(object),
                Err(status) if status.code() == Code::NotFound => {
                    tracing::debug!(object = %id.to_canonical_string(true), "object vanished before it could be fetched");
                }
                Err(status) => return Err(rpc_error("get_object", status)),
            }
        }
        Ok(objects)
    }
}
