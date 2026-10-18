//! Keeping item vectors current (KAIROS-A-0021 rules 3 and 4, KAIROS-T-0190).
//!
//! The piece that joins the three halves: [`kairos_core`] composes and chunks
//! the text, [`kairos_embed`] turns it into vectors, and
//! [`kairos_db::embeddings`] stores it. This module decides **what actually
//! needs doing**, which is almost always far less than "everything".
//!
//! # Skipping is the point
//!
//! Every text — the composed primary and each chunk — is hashed, and the hash is
//! compared with what is stored. Unchanged text is not sent to a model at all.
//!
//! That is not an optimisation bolted on afterwards; it is why chunking exists.
//! Metis instructs agents to append to a Status Updates section every few tool
//! calls, and the measured corpus has a median of nine sections per document, so
//! the difference between hashing per chunk and re-embedding per document is
//! roughly nine to one on the commonest write there is.
//!
//! # Batching
//!
//! Whatever does need embedding goes to the provider in **one call**, measured
//! at 42 texts/s batched against 4,927 documents. A per-chunk call would turn a
//! document refresh into nine round trips, which for the remote provider is nine
//! HTTP requests.
//!
//! # Blocking
//!
//! [`kairos_embed::EmbeddingProvider`] is synchronous and the remote
//! implementation **panics** if called from an async worker thread. Everything
//! here therefore runs under [`crate::blocking::BlockingTenantPool`], which is
//! already a `spawn_blocking` context. Nothing in this module may be awaited
//! directly from a handler.

use std::collections::HashMap;
use std::sync::Arc;

use kairos_core::chunk::chunk;
use kairos_core::embed_text::{chunk_text, mask_import_footer};
use kairos_core::primary::{PrimaryInputs, content_hash, primary_text};
use kairos_core::short_code::ItemType;
use kairos_db::embeddings::{
    ChunkWrite, EmbeddingError, PendingItem, StoredModel, metadata_for, pending_primary,
    store_primary, stored_chunk_hashes, sync_chunks,
};
use kairos_embed::{EmbedError, Embedding, EmbeddingProvider};

/// Errors from a refresh.
#[derive(Debug, thiserror::Error)]
pub enum RefreshError {
    /// The store refused or failed.
    #[error(transparent)]
    Store(#[from] EmbeddingError),
    /// The provider refused or failed.
    #[error(transparent)]
    Provider(#[from] EmbedError),
    /// A stored row names an entity type this build does not know.
    #[error("unknown entity type {0:?}")]
    UnknownEntityType(String),
    /// The job of the batch was stopped (KAIROS-T-0375).
    #[error("the batch was stopped")]
    Stopped,
}

/// What a refresh actually did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RefreshOutcome {
    /// Whether the primary vector was recomputed.
    pub primary_embedded: bool,
    /// How many chunks were recomputed.
    pub chunks_embedded: usize,
    /// How many chunks the item has in total.
    pub chunks_total: usize,
}

impl RefreshOutcome {
    /// Whether anything at all was sent to the model.
    pub fn did_work(&self) -> bool {
        self.primary_embedded || self.chunks_embedded > 0
    }

    /// How many texts were embedded.
    pub fn embedded(&self) -> usize {
        usize::from(self.primary_embedded) + self.chunks_embedded
    }
}

/// What a batch did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BatchOutcome {
    /// Items considered.
    pub items_seen: usize,
    /// Items that needed any work.
    pub items_changed: usize,
    /// Texts sent to the model.
    pub texts_embedded: usize,
}

/// How many items a background sweep looks at per tenant per tick.
///
/// Small on purpose. The sweep runs every few seconds and is competing with real
/// request traffic for the same connection pool; a large page would turn a
/// catch-up into a latency spike on the boards. A deployment with a real backlog
/// runs `embed-backfill`, which exists to go fast under supervision.
pub const REFRESH_BATCH: i64 = 25;

/// Keeps an item's vectors in line with its text.
pub struct EmbeddingService {
    provider: Arc<dyn EmbeddingProvider>,
    model: StoredModel,
}

impl std::fmt::Debug for EmbeddingService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmbeddingService")
            .field("model", &self.model)
            .finish()
    }
}

fn parse_entity_type(value: &str) -> Result<ItemType, RefreshError> {
    Ok(match value {
        "strategy" => ItemType::Strategy,
        "initiative" => ItemType::Initiative,
        "task" => ItemType::Task,
        "document" => ItemType::Document,
        "adr" => ItemType::Adr,
        other => return Err(RefreshError::UnknownEntityType(other.to_string())),
    })
}

impl EmbeddingService {
    /// Wrap a provider.
    pub fn new(provider: Arc<dyn EmbeddingProvider>) -> Self {
        let id = provider.model_id();
        let model = StoredModel {
            provider: id.provider.clone(),
            model: id.model.clone(),
            dimension: id.dimension,
        };
        Self { provider, model }
    }

    /// The provider, for a caller that makes vectors of its own (the builder
    /// of the base code index, COLLIERY-T-1853).
    pub fn provider(&self) -> Arc<dyn EmbeddingProvider> {
        Arc::clone(&self.provider)
    }

    /// The model every stored row is compared against.
    pub fn model(&self) -> &StoredModel {
        &self.model
    }

    /// The model, for a log line an operator will read.
    pub fn model_display(&self) -> String {
        format!(
            "{}/{} ({}d)",
            self.model.provider, self.model.model, self.model.dimension
        )
    }

    /// Bring one item's vectors up to date on one connection:
    /// [`Self::plan_item`], [`Self::embed_plan`], [`Self::store_plan`].
    ///
    /// `metadata` is the item's stamped `(label, value)` pairs, which the caller
    /// fetches for a whole batch at once — see [`metadata_for`].
    pub fn refresh_item(
        &self,
        conn: &mut diesel::pg::PgConnection,
        item: &PendingItem,
        metadata: &[(String, String)],
    ) -> Result<RefreshOutcome, RefreshError> {
        let plan = self.plan_item(conn, item, metadata)?;
        let vectors = self.embed_plan(&plan)?;
        self.store_plan(conn, &plan, &vectors)
    }

    /// The reads of one item (KAIROS-T-0614): what its vectors would be made
    /// of now, against what is stored. The plan holds all that
    /// [`Self::store_plan`] writes, so the connection can go back to the
    /// pool while the provider runs.
    pub fn plan_item(
        &self,
        conn: &mut diesel::pg::PgConnection,
        item: &PendingItem,
        metadata: &[(String, String)],
    ) -> Result<ItemPlan, RefreshError> {
        let item_type = parse_entity_type(&item.entity_type)?;

        // ---- the primary vector -------------------------------------------
        let inputs = PrimaryInputs {
            title: &item.title,
            repository: item.repository.as_deref(),
            parent_title: item.parent_title.as_deref(),
            metadata,
            content: &item.content,
        };
        let text = primary_text(item_type, &inputs);
        let hash = content_hash(&text);
        let primary_stale = item.stored_hash.as_deref() != Some(hash.as_str());

        // ---- the chunks ---------------------------------------------------
        // The importer footer is masked before chunking (COLLIERY-T-1840): it
        // says where an item came from, and it made the last chunk of every
        // item of one repository alike. The mask keeps each character offset,
        // so the stored offsets still point into the stored content.
        let content = mask_import_footer(&item.content);
        let chunks = chunk(&content);
        let stored: HashMap<i32, String> = stored_chunk_hashes(conn, item.id, &self.model)?
            .into_iter()
            .collect();
        // What is embedded for a chunk is its item's title, its heading and its
        // body (`kairos_core::embed_text::chunk_text`), and the hash is of that
        // text: a new title re-embeds the chunks, as it must.
        let texts: Vec<String> = chunks
            .iter()
            .map(|c| chunk_text(&item.title, c.heading.as_deref(), &c.text))
            .collect();
        let hashes: Vec<String> = texts.iter().map(|t| content_hash(t)).collect();

        // Everything that needs the model, gathered before anything is sent, so
        // one item costs one call however many of its sections moved.
        let mut to_embed: Vec<String> = Vec::new();
        if primary_stale {
            to_embed.push(text);
        }
        let stale_chunks: Vec<usize> = (0..chunks.len())
            .filter(|&i| stored.get(&(i as i32)) != Some(&hashes[i]))
            .collect();
        to_embed.extend(stale_chunks.iter().map(|&i| texts[i].clone()));

        Ok(ItemPlan {
            id: item.id,
            entity_type: item.entity_type.clone(),
            primary: primary_stale.then_some(hash),
            chunks,
            hashes,
            stale_chunks,
            to_embed,
        })
    }

    /// The vectors of a plan, from the provider (KAIROS-T-0614): one call
    /// for the item however many of its sections moved, and no call when
    /// none did. It needs no connection: a job runs it with none checked
    /// out.
    pub fn embed_plan(&self, plan: &ItemPlan) -> Result<Vec<Embedding>, RefreshError> {
        if plan.to_embed.is_empty() {
            return Ok(Vec::new());
        }
        let vectors = self.provider.embed(&plan.to_embed)?;
        if vectors.len() != plan.to_embed.len() {
            return Err(EmbedError::Failed(format!(
                "the provider gave {} vectors for {} texts",
                vectors.len(),
                plan.to_embed.len()
            ))
            .into());
        }
        Ok(vectors)
    }

    /// The writes of one item (KAIROS-T-0614): the primary vector and the
    /// chunks of `plan`, with the `vectors` of [`Self::embed_plan`].
    pub fn store_plan(
        &self,
        conn: &mut diesel::pg::PgConnection,
        plan: &ItemPlan,
        vectors: &[Embedding],
    ) -> Result<RefreshOutcome, RefreshError> {
        let mut outcome = RefreshOutcome {
            chunks_total: plan.chunks.len(),
            ..RefreshOutcome::default()
        };
        if plan.to_embed.is_empty() {
            return Ok(outcome);
        }
        if vectors.len() != plan.to_embed.len() {
            return Err(EmbedError::Failed(format!(
                "{} vectors for {} texts",
                vectors.len(),
                plan.to_embed.len()
            ))
            .into());
        }

        let mut next = 0usize;
        if let Some(hash) = &plan.primary {
            store_primary(
                conn,
                plan.id,
                &plan.entity_type,
                &vectors[next],
                &self.model,
                hash,
            )?;
            outcome.primary_embedded = true;
            next += 1;
        }

        // `None` for every chunk that did not move: the store leaves those rows
        // untouched rather than rewriting them with identical content.
        let mut fresh: HashMap<usize, &[f32]> = HashMap::new();
        for &i in &plan.stale_chunks {
            fresh.insert(i, vectors[next].as_slice());
            next += 1;
        }
        outcome.chunks_embedded = plan.stale_chunks.len();

        let writes: Vec<ChunkWrite<'_>> = plan
            .chunks
            .iter()
            .enumerate()
            .map(|(i, c)| ChunkWrite {
                ordinal: i as i32,
                heading: c.heading.as_deref(),
                char_start: c.char_start as i32,
                char_end: c.char_end as i32,
                text: &c.text,
                content_hash: &plan.hashes[i],
                vector: fresh.get(&i).copied(),
            })
            .collect();
        sync_chunks(conn, plan.id, &plan.entity_type, &writes, &self.model)?;

        Ok(outcome)
    }

    /// The plans of one page of the sweep (KAIROS-T-0614): the reads of
    /// [`Self::refresh_page_with`], with no call of the provider.
    pub fn plan_page(
        &self,
        conn: &mut diesel::pg::PgConnection,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<ItemPlan>, RefreshError> {
        let items = pending_primary(conn, &self.model, limit, offset)?;
        let ids: Vec<uuid::Uuid> = items.iter().map(|i| i.id).collect();

        // One metadata query for the page, not one per item.
        let mut by_item: HashMap<uuid::Uuid, Vec<(String, String)>> = HashMap::new();
        for (id, label, value) in metadata_for(conn, &ids)? {
            by_item.entry(id).or_default().push((label, value));
        }

        items
            .iter()
            .map(|item| {
                let metadata = by_item.remove(&item.id).unwrap_or_default();
                self.plan_item(conn, item, &metadata)
            })
            .collect()
    }

    /// Refresh up to `limit` items, oldest short code first.
    ///
    /// The unit a backfill walks. It is deliberately a *page* rather than a loop
    /// over everything: a backfill has to be interruptible and rate-limitable,
    /// and the caller is what decides how fast to go.
    pub fn refresh_batch(
        &self,
        conn: &mut diesel::pg::PgConnection,
        limit: i64,
    ) -> Result<BatchOutcome, RefreshError> {
        self.refresh_page(conn, limit, 0)
    }

    /// One page of a sweep, starting at `offset`.
    ///
    /// The offset is what makes a repeating sweep converge. Without it, a
    /// background pass took the same first page every tick, did no work once
    /// those were current, and never reached anything after them — a tenant's
    /// twenty-sixth item stayed unembedded for ever. Found by running the whole
    /// UAT suite, which creates enough work to get past one page; no smaller
    /// test could have shown it.
    pub fn refresh_page(
        &self,
        conn: &mut diesel::pg::PgConnection,
        limit: i64,
        offset: i64,
    ) -> Result<BatchOutcome, RefreshError> {
        self.refresh_page_with(conn, limit, offset, &mut || true)
    }

    /// [`Self::refresh_page`], with a call of `progress` before each item. A
    /// batch job (KAIROS-T-0375) reports its progress there; when `progress`
    /// returns false (the job was stopped), the page stops with
    /// [`RefreshError::Stopped`].
    pub fn refresh_page_with(
        &self,
        conn: &mut diesel::pg::PgConnection,
        limit: i64,
        offset: i64,
        progress: &mut dyn FnMut() -> bool,
    ) -> Result<BatchOutcome, RefreshError> {
        let plans = self.plan_page(conn, limit, offset)?;
        let mut outcome = BatchOutcome {
            items_seen: plans.len(),
            ..Default::default()
        };
        for plan in &plans {
            if !progress() {
                return Err(RefreshError::Stopped);
            }
            let vectors = self.embed_plan(plan)?;
            let one = self.store_plan(conn, plan, &vectors)?;
            outcome.count(&one);
        }
        progress();
        Ok(outcome)
    }
}

impl BatchOutcome {
    /// Count one item's outcome.
    fn count(&mut self, one: &RefreshOutcome) {
        if one.did_work() {
            self.items_changed += 1;
            self.texts_embedded += one.embedded();
        }
    }
}

/// The work of one item (KAIROS-T-0614): what [`EmbeddingService::plan_item`]
/// read, the texts for the provider, and what
/// [`EmbeddingService::store_plan`] writes. It holds no connection, so a
/// job keeps it while the provider runs with none checked out.
#[derive(Debug)]
pub struct ItemPlan {
    id: uuid::Uuid,
    entity_type: String,
    /// The hash of the primary text, when the primary vector is stale.
    primary: Option<String>,
    chunks: Vec<kairos_core::chunk::Chunk>,
    /// The hash of each chunk's text.
    hashes: Vec<String>,
    /// The chunks whose stored hash differs.
    stale_chunks: Vec<usize>,
    /// The primary text when stale, then the stale chunks: the provider's
    /// input, in the order of the vectors.
    to_embed: Vec<String>,
}

impl ItemPlan {
    /// True: the item needs the provider.
    pub fn needs_model(&self) -> bool {
        !self.to_embed.is_empty()
    }

    /// The texts that go to the provider.
    pub fn texts(&self) -> &[String] {
        &self.to_embed
    }
}

/// The dedupe key of the embedding batches of `tenant` (KAIROS-T-0375): one
/// open batch for each organization.
pub fn batch_key(tenant: &str) -> String {
    format!("embedding-batch:{tenant}")
}

/// The payload of an embedding batch job: where its page starts, and, when
/// it ended well, where the next page starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BatchPayload {
    pub offset: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<i64>,
}

/// Add the next embedding batch of `tenant`, on a connection of the public
/// schema (KAIROS-T-0375). Its page starts where the last batch that ended
/// well stopped, so a failed batch leaves its items for the next one. It
/// runs `interval` after that batch ended, on whichever server: a
/// deployment with 2 servers runs one batch of an organization for each
/// interval, not 2. The older ended batches of the organization are
/// removed: one row of each organization keeps the cursor. `None`: a batch
/// of the organization is open, and nothing was added.
pub fn enqueue_batch(
    conn: &mut diesel::pg::PgConnection,
    tenant: &str,
    interval: std::time::Duration,
    max_tries: i32,
) -> diesel::QueryResult<Option<kairos_db::jobs::Job>> {
    use diesel::Connection;
    use kairos_db::jobs as queue;
    let key = batch_key(tenant);
    conn.transaction(|conn| {
        let last = queue::last_done(conn, &key)?;
        let offset = last
            .as_ref()
            .and_then(|job| serde_json::from_str::<BatchPayload>(&job.payload).ok())
            .and_then(|payload| payload.next)
            .unwrap_or(0);
        let run_after = last.as_ref().and_then(|job| job.finished_at).map(|at| {
            at + chrono::Duration::from_std(interval).unwrap_or(chrono::Duration::zero())
        });
        let added = queue::enqueue(
            conn,
            &queue::NewJob {
                kind: crate::jobs::kinds::EMBEDDING_BATCH.to_string(),
                tenant: Some(tenant.to_string()),
                payload: serde_json::json!(BatchPayload { offset, next: None }),
                priority: crate::jobs::priority::EMBEDDING_BATCH,
                dedupe_key: Some(key.clone()),
                run_after,
                max_tries,
            },
        )?;
        if let (Some(_), Some(last)) = (&added, &last) {
            queue::remove_done_except(conn, &key, last.id)?;
        }
        Ok(added)
    })
}

/// Add the next embedding batch of each organization
/// ([`enqueue_batch`]), and wake the worker. The number added.
pub async fn enqueue_batches(
    blocking: &crate::blocking::BlockingTenantPool,
    worker: &crate::jobs::Worker,
    interval: std::time::Duration,
) -> usize {
    use crate::error::ApiError;
    let tenants = match blocking
        .run_public(|conn| kairos_db::list_tenants(conn).map_err(ApiError::internal))
        .await
    {
        Ok(tenants) => tenants,
        Err(e) => {
            tracing::warn!(error = ?e, "embedding refresher could not list tenants");
            return 0;
        }
    };
    let max_tries = worker.settings().max_tries;
    let mut added = 0;
    for tenant in tenants.into_iter().filter(|t| t.schema_exists) {
        let slug = tenant.slug.clone();
        match blocking
            .run_public(move |conn| {
                enqueue_batch(conn, &slug, interval, max_tries).map_err(ApiError::internal)
            })
            .await
        {
            Ok(Some(_)) => added += 1,
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(tenant = %tenant.slug, error = ?e, "embedding batch not added")
            }
        }
    }
    if added > 0 {
        worker.wake();
    }
    added
}

/// The handler of the embedding batch jobs (KAIROS-T-0375): one page of
/// [`EmbeddingService::refresh_page_with`] in the organization of the job.
#[derive(Clone)]
pub struct BatchHandler {
    blocking: crate::blocking::BlockingTenantPool,
    service: Arc<EmbeddingService>,
    batch: i64,
}

impl BatchHandler {
    /// A handler of pages of `batch` items.
    pub fn new(
        blocking: crate::blocking::BlockingTenantPool,
        service: Arc<EmbeddingService>,
        batch: i64,
    ) -> Self {
        BatchHandler {
            blocking,
            service,
            batch,
        }
    }

    /// Run the embedding batches on `worker`.
    pub fn register(self, worker: &crate::jobs::Worker) {
        worker.register(crate::jobs::kinds::EMBEDDING_BATCH, Arc::new(self));
    }

    async fn refresh(self, handle: crate::jobs::JobHandle) -> Result<(), crate::jobs::JobError> {
        use crate::error::ApiError;
        use crate::jobs::JobError;
        let job = handle.job().clone();
        let tenant = job
            .tenant
            .clone()
            .ok_or_else(|| JobError::fail("The embedding batch has no tenant."))?;
        let payload: BatchPayload = serde_json::from_str(&job.payload).map_err(|e| {
            JobError::fail(format!("The payload of the embedding batch is wrong: {e}"))
        })?;
        let (service, batch, offset) = (Arc::clone(&self.service), self.batch, payload.offset);
        let failed = |reason: String| {
            tracing::warn!(tenant = %tenant, job = %job.id, %reason, "embedding batch failed");
            JobError::retry(format!(
                "The embedding batch at item {offset} failed: {reason}"
            ))
        };
        let no_database = |e: ApiError| {
            JobError::retry(format!(
                "The embedding batch at item {offset} could not reach the database: {}",
                e.message
            ))
        };

        // KAIROS-T-0614: three steps. One checkout reads the page; the
        // provider runs with no connection checked out; a second checkout
        // writes. A remote provider that takes seconds pins no connection
        // of the pool while it runs.
        let planner = Arc::clone(&service);
        let (total, plans) = self
            .blocking
            .run(&tenant, move |conn| {
                let total = kairos_db::embeddings::item_count(conn).map_err(ApiError::internal)?;
                // The reason stays a text: the job keeps it in `last_error`.
                Ok(planner
                    .plan_page(conn, batch, offset)
                    .map(|plans| (total, plans))
                    .map_err(|e| e.to_string()))
            })
            .await
            .map_err(no_database)?
            .map_err(failed)?;

        let embedder = Arc::clone(&service);
        let progress = handle.clone();
        let embedded = tokio::task::spawn_blocking(move || {
            plans
                .into_iter()
                .map(|plan| {
                    if !progress.progress() {
                        return Err(RefreshError::Stopped);
                    }
                    let vectors = embedder.embed_plan(&plan)?;
                    Ok((plan, vectors))
                })
                .collect::<Result<Vec<_>, RefreshError>>()
        })
        .await
        .map_err(|e| JobError::retry(format!("The embedding batch at item {offset} stopped: {e}")))?
        .map_err(|e| failed(e.to_string()))?;

        let writer = Arc::clone(&service);
        let progress = handle.clone();
        let outcome = self
            .blocking
            .run(&tenant, move |conn| {
                let mut outcome = BatchOutcome {
                    items_seen: embedded.len(),
                    ..Default::default()
                };
                for (plan, vectors) in &embedded {
                    match writer.store_plan(conn, plan, vectors) {
                        Ok(one) => outcome.count(&one),
                        Err(e) => return Ok(Err(e.to_string())),
                    }
                }
                progress.progress();
                Ok(Ok(outcome))
            })
            .await
            .map_err(no_database)?
            .map_err(failed)?;
        // Wrap at the end, so the sweep keeps cycling: an edit to an item
        // anywhere in the organization is seen on the next time round.
        let next = offset + batch;
        let next = if next >= total { 0 } else { next };
        if outcome.items_changed > 0 {
            tracing::info!(
                tenant = %tenant,
                items = outcome.items_changed,
                texts = outcome.texts_embedded,
                "embedded"
            );
        }
        // The cursor of the next batch.
        let (id, instance) = (job.id, job.leased_by.unwrap_or_default());
        let patch = serde_json::json!({ "next": next });
        self.blocking
            .run_public(move |conn| {
                kairos_db::jobs::merge_payload(conn, id, instance, &patch)
                    .map_err(ApiError::internal)
            })
            .await
            .map_err(|e| JobError::retry(format!("The cursor was not written: {}", e.message)))?;
        Ok(())
    }
}

impl crate::jobs::JobHandler for BatchHandler {
    fn run(&self, handle: crate::jobs::JobHandle) -> crate::jobs::JobFuture {
        Box::pin(self.clone().refresh(handle))
    }
}

/// Keep the vectors of every organization current, for ever (KAIROS-T-0190,
/// KAIROS-T-0375): run the embedding batches on `worker`, and at each
/// `interval` add the next batch of each organization ([`enqueue_batches`]).
/// The worker of a server takes the batches of all servers, so 2 servers do
/// not embed one item twice.
///
/// # A cursor in a job, not a queue of items
///
/// [[KAIROS-A-0021]] rule 7 requires embedding to happen **off** the write
/// path: `create_item` must not wait on a model, and a failed embedding must
/// leave the item created and retrievable lexically.
///
/// Staleness here is **derived**, not recorded: an item needs work exactly when
/// its stored content hash no longer matches the text it would compose now. A
/// queue of items would be a second statement of the same fact, and second
/// statements drift. So a job is a page of the sweep, not a list of items: it
/// names where its page starts, and it asks the only source of truth there
/// is. A batch that fails leaves its items stale, and the next batch starts
/// at the same place.
///
/// The cost is latency rather than correctness. An item is embedded within one
/// interval rather than immediately, and that is an acceptable trade for a
/// feature whose results are proposals.
///
/// # It never takes the server down
///
/// Every error is logged and the loop continues. An embedding provider that has
/// gone away is a reason for retrieval to degrade to lexical, which is what rule
/// 7 promises; it is not a reason to stop serving boards.
pub async fn run_refresher(
    worker: Arc<crate::jobs::Worker>,
    blocking: crate::blocking::BlockingTenantPool,
    service: Arc<EmbeddingService>,
    interval: std::time::Duration,
    batch: i64,
) {
    tracing::info!(
        interval_secs = interval.as_secs(),
        batch,
        model = %service.model_display(),
        "embedding refresher started"
    );
    BatchHandler::new(blocking.clone(), Arc::clone(&service), batch).register(&worker);
    let mut ticker = tokio::time::interval(interval);
    // A slow pass must not cause a burst of catch-up ticks afterwards.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        enqueue_batches(&blocking, &worker, interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_entity_type_parses_and_nothing_else_does() {
        for (s, expected) in [
            ("strategy", ItemType::Strategy),
            ("initiative", ItemType::Initiative),
            ("task", ItemType::Task),
            ("document", ItemType::Document),
            ("adr", ItemType::Adr),
        ] {
            assert_eq!(parse_entity_type(s).unwrap(), expected);
        }
        let err = parse_entity_type("epic").expect_err("not a Kairos entity type");
        assert!(err.to_string().contains("epic"), "{err}");
    }

    #[test]
    fn an_outcome_reports_what_it_sent_to_the_model() {
        let none = RefreshOutcome {
            primary_embedded: false,
            chunks_embedded: 0,
            chunks_total: 9,
        };
        assert!(!none.did_work());
        assert_eq!(none.embedded(), 0);

        // The commonest real write: one section of nine appended to.
        let append = RefreshOutcome {
            primary_embedded: false,
            chunks_embedded: 1,
            chunks_total: 9,
        };
        assert!(append.did_work());
        assert_eq!(append.embedded(), 1, "one embedding, not nine");
    }
}

// ---------------------------------------------------------------------------
// Retrieval (KAIROS-T-0191)
// ---------------------------------------------------------------------------

/// The answer to "what is related to this?".
#[derive(Debug, Clone)]
pub struct RelatedWork {
    /// The item asked about.
    pub short_code: String,
    /// Bounded, best first.
    pub proposals: Vec<kairos_core::retrieval::Proposal>,
    /// Which sources contributed, so a caller can tell a thin answer from a
    /// complete one.
    pub sources: kairos_core::retrieval::Sources,
}

/// How many candidates each source contributes before fusion.
///
/// Wider than the handful returned, because fusion needs something to fuse: an
/// item ranked eighth by text and ninth by meaning is exactly the agreement
/// worth surfacing, and a five-deep window from each source would never see it.
const CANDIDATE_DEPTH: i64 = 25;

impl EmbeddingService {
    /// Find work related to `short_code`, as bounded proposals.
    ///
    /// Runs lexical and vector search, fuses them by rank, and asks the graph
    /// what it knows — see `kairos_core::retrieval` for what is claimed and why
    /// nothing compares a similarity to a constant.
    ///
    /// Degrades rather than fails: with no usable vectors this answers from text
    /// alone, and `Sources` says so once for the response. That is KAIROS-A-0021
    /// rule 7. A text-only answer uses the text score of
    /// `kairos_db::embeddings::text_neighbours`: fewer proposals, and none
    /// under the floor (KAIROS-T-0562).
    pub fn related_work(
        &self,
        conn: &mut diesel::pg::PgConnection,
        short_code: &str,
        config: &kairos_core::retrieval::RetrievalConfig,
    ) -> Result<RelatedWork, RefreshError> {
        use kairos_core::retrieval::{Candidate, GraphRelation, Sources, propose};
        use std::collections::HashMap;

        let item = kairos_db::embeddings::item_by_short_code(conn, short_code)?;

        let vector =
            kairos_db::embeddings::vector_neighbours(conn, item.id, &self.model, CANDIDATE_DEPTH)?;
        let sources = Sources {
            lexical: true,
            vector: !vector.is_empty(),
        };

        let mut by_id: HashMap<uuid::Uuid, Candidate> = HashMap::new();
        if sources.vector {
            // Lexical uses the item's own title as the query: it is the most
            // discriminating text the item has, and a whole document as a
            // tsquery matches everything. Any word of it matches, and more
            // words rank higher (COLLIERY-T-1840); requiring every word matched
            // nothing.
            let lexical = kairos_db::embeddings::lexical_neighbours(
                conn,
                item.id,
                &item.title,
                CANDIDATE_DEPTH,
            )?;
            for (rank, n) in lexical.iter().enumerate() {
                by_id.entry(n.id).or_insert_with(|| blank(n)).lexical_rank = Some(rank);
            }
        } else {
            // No vectors: the text score decides (KAIROS-T-0562). The same
            // title words, but each weighs by how rare it is, so a match on
            // common words alone stays under the floor of `propose`.
            let text = kairos_db::embeddings::text_neighbours(
                conn,
                item.id,
                &item.title,
                CANDIDATE_DEPTH,
            )?;
            for (rank, n) in text.iter().enumerate() {
                let c = by_id
                    .entry(n.neighbour.id)
                    .or_insert_with(|| blank(&n.neighbour));
                c.lexical_rank = Some(rank);
                c.text_score = Some(n.text_score as f32);
            }
        }
        for (rank, n) in vector.iter().enumerate() {
            let c = by_id.entry(n.id).or_insert_with(|| blank(n));
            c.vector_rank = Some(rank);
            // The chunk heading only exists on the vector side, and it is the
            // citation — keep it even when lexical saw the item first. It is
            // `None` when the best match was the item's primary vector, and a
            // chunk with no real content is never the best match
            // (COLLIERY-T-1840), so a template heading is not quoted.
            if c.heading.is_none() {
                c.heading.clone_from(&n.heading);
            }
        }

        let ids: Vec<uuid::Uuid> = by_id.keys().copied().collect();
        for facts in kairos_db::embeddings::graph_facts(conn, item.id, &ids)? {
            if let Some(c) = by_id.get_mut(&facts.item_id) {
                c.graph = GraphRelation {
                    directly_linked: facts.directly_linked,
                    shared_parent: facts.shared_parent,
                    same_repository: facts.same_repository,
                    same_board: facts.same_board,
                };
            }
        }

        let candidates: Vec<Candidate> = by_id.into_values().collect();
        Ok(RelatedWork {
            short_code: item.short_code,
            proposals: propose(&candidates, sources, config),
            sources,
        })
    }
}

fn blank(n: &kairos_db::embeddings::Neighbour) -> kairos_core::retrieval::Candidate {
    kairos_core::retrieval::Candidate {
        short_code: n.short_code.clone(),
        title: n.title.clone(),
        entity_type: n.entity_type.clone(),
        lexical_rank: None,
        vector_rank: None,
        graph: kairos_core::retrieval::GraphRelation::default(),
        // Put away OR done: both mean "already been here", which is what makes
        // the prior-art claim worth making.
        finished: n.archived,
        heading: n.heading.clone(),
        text_score: None,
    }
}
