pub mod availability_info;
pub mod available_modules;
pub(crate) mod chunking_context;
pub(crate) mod containment_tree;
pub(crate) mod data;
pub(crate) mod evaluate;
pub mod optimize;
pub(crate) mod passthrough_asset;

use std::{
    collections::HashSet,
    fmt::{Debug, Display},
    future::Future,
    hash::Hash,
    marker::PhantomData,
};

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use tracing::{info_span, Span};
use turbo_tasks::{
    debug::ValueDebugFormat,
    graph::{AdjacencyMap, GraphTraversal, GraphTraversalResult, Visit, VisitControlFlow},
    trace::TraceRawVcs,
    ReadRef, TryJoinIterExt, Upcast, Value, ValueToString, Vc,
};
use turbo_tasks_fs::FileSystemPath;
use turbo_tasks_hash::DeterministicHash;

use self::availability_info::AvailabilityInfo;
pub use self::{
    chunking_context::{ChunkingContext, ChunkingContextExt},
    data::{ChunkData, ChunkDataOption, ChunksData},
    evaluate::{EvaluatableAsset, EvaluatableAssetExt, EvaluatableAssets},
    passthrough_asset::PassthroughModule,
};
use crate::{
    asset::Asset,
    ident::AssetIdent,
    module::{Module, Modules},
    output::OutputAssets,
    reference::{ModuleReference, ModuleReferences},
};

/// A module id, which can be a number or string
#[turbo_tasks::value(shared)]
#[derive(Debug, Clone, Hash, Ord, PartialOrd, DeterministicHash)]
#[serde(untagged)]
pub enum ModuleId {
    Number(u32),
    String(String),
}

impl Display for ModuleId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ModuleId::Number(i) => write!(f, "{}", i),
            ModuleId::String(s) => write!(f, "{}", s),
        }
    }
}

#[turbo_tasks::value_impl]
impl ValueToString for ModuleId {
    #[turbo_tasks::function]
    fn to_string(&self) -> Vc<String> {
        Vc::cell(self.to_string())
    }
}

impl ModuleId {
    pub fn parse(id: &str) -> Result<ModuleId> {
        Ok(match id.parse::<u32>() {
            Ok(i) => ModuleId::Number(i),
            Err(_) => ModuleId::String(id.to_string()),
        })
    }
}

/// A list of module ids.
#[turbo_tasks::value(transparent, shared)]
pub struct ModuleIds(Vec<Vc<ModuleId>>);

/// A [Module] that can be converted into a [Chunk].
#[turbo_tasks::value_trait]
pub trait ChunkableModule: Module + Asset {
    fn as_chunk_item(
        self: Vc<Self>,
        chunking_context: Vc<Box<dyn ChunkingContext>>,
    ) -> Vc<Box<dyn ChunkItem>>;
}

#[turbo_tasks::value(transparent)]
pub struct Chunks(Vec<Vc<Box<dyn Chunk>>>);

#[turbo_tasks::value_impl]
impl Chunks {
    /// Creates a new empty [Vc<Chunks>].
    #[turbo_tasks::function]
    pub fn empty() -> Vc<Self> {
        Vc::cell(vec![])
    }
}

/// A chunk is one type of asset.
/// It usually contains multiple chunk items.
#[turbo_tasks::value_trait]
pub trait Chunk: Asset {
    fn ident(self: Vc<Self>) -> Vc<AssetIdent>;
    fn chunking_context(self: Vc<Self>) -> Vc<Box<dyn ChunkingContext>>;
    // TODO Once output assets have their own trait, this path() method will move
    // into that trait and ident() will be removed from that. Assets on the
    // output-level only have a path and no complex ident.
    /// The path of the chunk.
    fn path(self: Vc<Self>) -> Vc<FileSystemPath> {
        self.ident().path()
    }

    /// Returns a list of chunks that should be loaded in parallel to this
    /// chunk.
    fn parallel_chunks(self: Vc<Self>) -> Vc<Chunks> {
        Chunks::empty()
    }

    /// Other [OutputAsset]s referenced from this [Chunk].
    fn references(self: Vc<Self>) -> Vc<OutputAssets> {
        OutputAssets::empty()
    }
}

/// Aggregated information about a chunk content that can be used by the runtime
/// code to optimize chunk loading.
#[turbo_tasks::value(shared)]
#[derive(Default)]
pub struct OutputChunkRuntimeInfo {
    pub included_ids: Option<Vc<ModuleIds>>,
    pub excluded_ids: Option<Vc<ModuleIds>>,
    /// List of paths of chunks containing individual modules that are part of
    /// this chunk. This is useful for selectively loading modules from a chunk
    /// without loading the whole chunk.
    pub module_chunks: Option<Vc<OutputAssets>>,
    pub placeholder_for_future_extensions: (),
}

#[turbo_tasks::value_trait]
pub trait OutputChunk: Asset {
    fn runtime_info(self: Vc<Self>) -> Vc<OutputChunkRuntimeInfo>;
}

/// Specifies how a chunk interacts with other chunks when building a chunk
/// group
#[derive(
    Copy, Default, Clone, Hash, TraceRawVcs, Serialize, Deserialize, Eq, PartialEq, ValueDebugFormat,
)]
pub enum ChunkingType {
    /// Asset is always placed into the referencing chunk and loaded with it.
    Placed,
    /// A heuristic determines if the asset is placed into the referencing chunk
    /// or in a separate chunk that is loaded in parallel.
    #[default]
    PlacedOrParallel,
    /// Asset is always placed in a separate chunk that is loaded in parallel.
    Parallel,
    /// Asset is always placed in a separate chunk that is loaded in parallel.
    /// Referenced asset will not inherit the available modules, but form a
    /// new availability root.
    IsolatedParallel,
    /// An async loader is placed into the referencing chunk and loads the
    /// separate chunk group in which the asset is placed.
    Async,
}

#[turbo_tasks::value(transparent)]
pub struct ChunkingTypeOption(Option<ChunkingType>);

/// A [ModuleReference] implementing this trait and returning true for
/// [ChunkableModuleReference::is_chunkable] are considered as potentially
/// chunkable references. When all [Module]s of such a reference implement
/// [ChunkableModule] they are placed in [Chunk]s during chunking.
/// They are even potentially placed in the same [Chunk] when a chunk type
/// specific interface is implemented.
#[turbo_tasks::value_trait]
pub trait ChunkableModuleReference: ModuleReference + ValueToString {
    fn chunking_type(self: Vc<Self>) -> Vc<ChunkingTypeOption> {
        Vc::cell(Some(ChunkingType::default()))
    }
}

pub struct ChunkContentResult<I> {
    pub chunk_items: Vec<I>,
    pub chunks: Vec<Vc<Box<dyn Chunk>>>,
    pub external_module_references: Vec<Vc<Box<dyn ModuleReference>>>,
    pub availability_info: AvailabilityInfo,
}

#[async_trait::async_trait]
pub trait FromChunkableModule: ChunkItem {
    async fn from_asset(
        chunking_context: Vc<Box<dyn ChunkingContext>>,
        asset: Vc<Box<dyn Module>>,
    ) -> Result<Option<Vc<Self>>>;
    async fn from_async_asset(
        chunking_context: Vc<Box<dyn ChunkingContext>>,
        asset: Vc<Box<dyn ChunkableModule>>,
        availability_info: Value<AvailabilityInfo>,
    ) -> Result<Option<Vc<Self>>>;
}

pub async fn chunk_content_split<I>(
    chunking_context: Vc<Box<dyn ChunkingContext>>,
    entry: Vc<Box<dyn Module>>,
    additional_entries: Option<Vc<Modules>>,
    availability_info: Value<AvailabilityInfo>,
) -> Result<ChunkContentResult<Vc<I>>>
where
    I: FromChunkableModule,
{
    chunk_content_internal_parallel(
        chunking_context,
        entry,
        additional_entries,
        availability_info,
        true,
    )
    .await
    .map(|o| o.unwrap())
}

pub async fn chunk_content<I>(
    chunking_context: Vc<Box<dyn ChunkingContext>>,
    entry: Vc<Box<dyn Module>>,
    additional_entries: Option<Vc<Modules>>,
    availability_info: Value<AvailabilityInfo>,
) -> Result<Option<ChunkContentResult<Vc<I>>>>
where
    I: FromChunkableModule,
{
    chunk_content_internal_parallel(
        chunking_context,
        entry,
        additional_entries,
        availability_info,
        false,
    )
    .await
}

#[derive(Eq, PartialEq, Clone, Hash)]
enum ChunkContentGraphNode<I> {
    // An asset not placed in the current chunk, but whose references we will
    // follow to find more graph nodes.
    PassthroughModule { asset: Vc<Box<dyn Module>> },
    // Chunk items that are placed into the current chunk
    ChunkItem { item: I, ident: ReadRef<String> },
    // Asset that is already available and doesn't need to be included
    AvailableAsset(Vc<Box<dyn Module>>),
    // Chunks that are loaded in parallel to the current chunk
    Chunk(Vc<Box<dyn Chunk>>),
    ExternalModuleReference(Vc<Box<dyn ModuleReference>>),
}

#[derive(Clone, Copy)]
struct ChunkContentContext {
    chunking_context: Vc<Box<dyn ChunkingContext>>,
    entry: Vc<Box<dyn Module>>,
    availability_info: Value<AvailabilityInfo>,
    split: bool,
}

async fn reference_to_graph_nodes<I>(
    chunk_content_context: ChunkContentContext,
    reference: Vc<Box<dyn ModuleReference>>,
) -> Result<
    Vec<(
        Option<(Vc<Box<dyn Module>>, ChunkingType)>,
        ChunkContentGraphNode<Vc<I>>,
    )>,
>
where
    I: Send + FromChunkableModule,
{
    let Some(chunkable_module_reference) =
        Vc::try_resolve_downcast::<Box<dyn ChunkableModuleReference>>(reference).await?
    else {
        return Ok(vec![(
            None,
            ChunkContentGraphNode::ExternalModuleReference(reference),
        )]);
    };

    let Some(chunking_type) = *chunkable_module_reference.chunking_type().await? else {
        return Ok(vec![(
            None,
            ChunkContentGraphNode::ExternalModuleReference(reference),
        )]);
    };

    let modules = reference.resolve_reference().primary_modules().await?;

    let mut graph_nodes = vec![];

    for &module in &modules {
        let module = module.resolve().await?;
        if let Some(available_modules) = chunk_content_context.availability_info.available_modules()
        {
            if *available_modules.includes(module).await? {
                graph_nodes.push((
                    Some((module, chunking_type)),
                    ChunkContentGraphNode::AvailableAsset(module),
                ));
                continue;
            }
        }

        if Vc::try_resolve_sidecast::<Box<dyn PassthroughModule>>(module)
            .await?
            .is_some()
        {
            graph_nodes.push((
                None,
                ChunkContentGraphNode::PassthroughModule { asset: module },
            ));
            continue;
        }

        let chunkable_module =
            match Vc::try_resolve_sidecast::<Box<dyn ChunkableModule>>(module).await? {
                Some(chunkable_module) => chunkable_module,
                _ => {
                    return Ok(vec![(
                        None,
                        ChunkContentGraphNode::ExternalModuleReference(reference),
                    )]);
                }
            };

        match chunking_type {
            ChunkingType::Placed => {
                if let Some(chunk_item) =
                    I::from_asset(chunk_content_context.chunking_context, module).await?
                {
                    graph_nodes.push((
                        Some((module, chunking_type)),
                        ChunkContentGraphNode::ChunkItem {
                            item: chunk_item,
                            ident: module.ident().to_string().await?,
                        },
                    ));
                } else {
                    return Err(anyhow!(
                        "Module {} was requested to be placed into the same chunk, but this \
                         wasn't possible",
                        module.ident().to_string().await?
                    ));
                }
            }
            ChunkingType::Parallel => {
                let chunk_item =
                    chunkable_module.as_chunk_item(chunk_content_context.chunking_context);
                let chunk = chunk_item
                    .ty()
                    .as_chunk(chunk_item, chunk_content_context.availability_info);
                graph_nodes.push((
                    Some((module, chunking_type)),
                    ChunkContentGraphNode::Chunk(chunk),
                ));
            }
            ChunkingType::IsolatedParallel => {
                let chunk_item =
                    chunkable_module.as_chunk_item(chunk_content_context.chunking_context);
                let chunk = chunk_item.ty().as_chunk(
                    chunk_item,
                    Value::new(AvailabilityInfo::Root {
                        current_availability_root: Vc::upcast(chunkable_module),
                    }),
                );
                graph_nodes.push((
                    Some((module, chunking_type)),
                    ChunkContentGraphNode::Chunk(chunk),
                ));
            }
            ChunkingType::PlacedOrParallel => {
                // heuristic for being in the same chunk
                if !chunk_content_context.split
                    && *chunk_content_context
                        .chunking_context
                        .can_be_in_same_chunk(chunk_content_context.entry, module)
                        .await?
                {
                    // chunk item, chunk or other asset?
                    if let Some(chunk_item) =
                        I::from_asset(chunk_content_context.chunking_context, module).await?
                    {
                        graph_nodes.push((
                            Some((module, chunking_type)),
                            ChunkContentGraphNode::ChunkItem {
                                item: chunk_item,
                                ident: module.ident().to_string().await?,
                            },
                        ));
                        continue;
                    }
                }

                let chunk_item =
                    chunkable_module.as_chunk_item(chunk_content_context.chunking_context);
                let chunk = chunk_item
                    .ty()
                    .as_chunk(chunk_item, chunk_content_context.availability_info);
                graph_nodes.push((
                    Some((module, chunking_type)),
                    ChunkContentGraphNode::Chunk(chunk),
                ));
            }
            ChunkingType::Async => {
                if let Some(manifest_loader_item) = I::from_async_asset(
                    chunk_content_context.chunking_context,
                    chunkable_module,
                    chunk_content_context.availability_info,
                )
                .await?
                {
                    graph_nodes.push((
                        Some((module, chunking_type)),
                        ChunkContentGraphNode::ChunkItem {
                            item: manifest_loader_item,
                            ident: module.ident().to_string().await?,
                        },
                    ));
                } else {
                    return Ok(vec![(
                        None,
                        ChunkContentGraphNode::ExternalModuleReference(reference),
                    )]);
                }
            }
        }
    }

    Ok(graph_nodes)
}

/// The maximum number of chunk items that can be in a chunk before we split it
/// into multiple chunks.
const MAX_CHUNK_ITEMS_COUNT: usize = 5000;

struct ChunkContentVisit<I> {
    chunk_content_context: ChunkContentContext,
    chunk_items_count: usize,
    processed_assets: HashSet<(ChunkingType, Vc<Box<dyn Module>>)>,
    _phantom: PhantomData<I>,
}

type ChunkItemToGraphNodesEdges<I: Send> = impl Iterator<
    Item = (
        Option<(Vc<Box<dyn Module>>, ChunkingType)>,
        ChunkContentGraphNode<Vc<I>>,
    ),
>;

type ChunkItemToGraphNodesFuture<I: FromChunkableModule> =
    impl Future<Output = Result<ChunkItemToGraphNodesEdges<I>>>;

impl<I> Visit<ChunkContentGraphNode<Vc<I>>, ()> for ChunkContentVisit<Vc<I>>
where
    I: Send + FromChunkableModule,
{
    type Edge = (
        Option<(Vc<Box<dyn Module>>, ChunkingType)>,
        ChunkContentGraphNode<Vc<I>>,
    );
    type EdgesIntoIter = ChunkItemToGraphNodesEdges<I>;
    type EdgesFuture = ChunkItemToGraphNodesFuture<I>;

    fn visit(
        &mut self,
        (option_key, node): (
            Option<(Vc<Box<dyn Module>>, ChunkingType)>,
            ChunkContentGraphNode<Vc<I>>,
        ),
    ) -> VisitControlFlow<ChunkContentGraphNode<Vc<I>>, ()> {
        let Some((asset, chunking_type)) = option_key else {
            return VisitControlFlow::Continue(node);
        };

        if !self.processed_assets.insert((chunking_type, asset)) {
            return VisitControlFlow::Skip(node);
        }

        if let ChunkContentGraphNode::ChunkItem { .. } = &node {
            self.chunk_items_count += 1;

            // Make sure the chunk doesn't become too large.
            // This will hurt performance in many aspects.
            if !self.chunk_content_context.split && self.chunk_items_count >= MAX_CHUNK_ITEMS_COUNT
            {
                // Chunk is too large, cancel this algorithm and restart with splitting from the
                // start.
                return VisitControlFlow::Abort(());
            }
        }

        VisitControlFlow::Continue(node)
    }

    fn edges(&mut self, node: &ChunkContentGraphNode<Vc<I>>) -> Self::EdgesFuture {
        let node = node.clone();

        let chunk_content_context = self.chunk_content_context;

        async move {
            let references = match node {
                ChunkContentGraphNode::PassthroughModule { asset } => asset.references(),
                ChunkContentGraphNode::ChunkItem { item, .. } => item.references(),
                _ => {
                    return Ok(vec![].into_iter().flatten());
                }
            };

            Ok(references
                .await?
                .into_iter()
                .map(|reference| reference_to_graph_nodes::<I>(chunk_content_context, *reference))
                .try_join()
                .await?
                .into_iter()
                .flatten())
        }
    }

    fn span(&mut self, node: &ChunkContentGraphNode<Vc<I>>) -> Span {
        if let ChunkContentGraphNode::ChunkItem { ident, .. } = node {
            info_span!("module", name = display(ident))
        } else {
            Span::current()
        }
    }
}

async fn chunk_content_internal_parallel<I>(
    chunking_context: Vc<Box<dyn ChunkingContext>>,
    entry: Vc<Box<dyn Module>>,
    additional_entries: Option<Vc<Modules>>,
    availability_info: Value<AvailabilityInfo>,
    split: bool,
) -> Result<Option<ChunkContentResult<Vc<I>>>>
where
    I: FromChunkableModule,
{
    let additional_entries = if let Some(additional_entries) = additional_entries {
        additional_entries.await?.clone_value().into_iter()
    } else {
        vec![].into_iter()
    };

    let root_edges = [entry]
        .into_iter()
        .chain(additional_entries)
        .map(|entry| async move {
            Ok((
                Some((entry, ChunkingType::Placed)),
                ChunkContentGraphNode::ChunkItem {
                    item: I::from_asset(chunking_context, entry).await?.unwrap(),
                    ident: entry.ident().to_string().await?,
                },
            ))
        })
        .try_join()
        .await?;

    let chunk_content_context = ChunkContentContext {
        chunking_context,
        entry,
        split,
        availability_info,
    };

    let visit = ChunkContentVisit {
        chunk_content_context,
        chunk_items_count: 0,
        processed_assets: Default::default(),
        _phantom: PhantomData,
    };

    let GraphTraversalResult::Completed(traversal_result) =
        AdjacencyMap::new().visit(root_edges, visit).await
    else {
        return Ok(None);
    };

    let graph_nodes: Vec<_> = traversal_result?.into_reverse_topological().collect();

    let mut chunk_items = Vec::new();
    let mut chunks = Vec::new();
    let mut external_module_references = Vec::new();

    for graph_node in graph_nodes {
        match graph_node {
            ChunkContentGraphNode::AvailableAsset(_)
            | ChunkContentGraphNode::PassthroughModule { .. } => {}
            ChunkContentGraphNode::ChunkItem { item, .. } => {
                chunk_items.push(item);
            }
            ChunkContentGraphNode::Chunk(chunk) => {
                chunks.push(chunk);
            }
            ChunkContentGraphNode::ExternalModuleReference(reference) => {
                external_module_references.push(reference);
            }
        }
    }

    Ok(Some(ChunkContentResult {
        chunk_items,
        chunks,
        external_module_references,
        availability_info: availability_info.into_value(),
    }))
}

#[turbo_tasks::value_trait]
pub trait ChunkItem {
    /// The [AssetIdent] of the [Module] that this [ChunkItem] was created from.
    /// For most chunk types this must uniquely identify the asset as it's the
    /// source of the module id used at runtime.
    fn asset_ident(self: Vc<Self>) -> Vc<AssetIdent>;
    /// A [ChunkItem] can describe different `references` than its original
    /// [Module].
    /// TODO(alexkirsz) This should have a default impl that returns empty
    /// references.
    fn references(self: Vc<Self>) -> Vc<ModuleReferences>;

    /// The type of chunk this item should be assembled into.
    fn ty(self: Vc<Self>) -> Vc<Box<dyn ChunkType>>;

    /// A temporary method to retrieve the module associated with this
    /// ChunkItem. TODO: Remove this as part of the chunk refactoring.
    fn module(self: Vc<Self>) -> Vc<Box<dyn Module>>;

    fn chunking_context(self: Vc<Self>) -> Vc<Box<dyn ChunkingContext>>;
}

#[turbo_tasks::value_trait]
pub trait ChunkType {
    /// Create a new chunk for the given subgraph.
    fn as_chunk(
        &self,
        chunk_item: Vc<Box<dyn ChunkItem>>,
        availability_info: Value<AvailabilityInfo>,
    ) -> Vc<Box<dyn Chunk>>;
}

#[turbo_tasks::value(transparent)]
pub struct ChunkItems(Vec<Vc<Box<dyn ChunkItem>>>);

pub trait ChunkItemExt: Send {
    /// Returns the module id of this chunk item.
    fn id(self: Vc<Self>) -> Vc<ModuleId>;
}

impl<T> ChunkItemExt for T
where
    T: Upcast<Box<dyn ChunkItem>>,
{
    /// Returns the module id of this chunk item.
    fn id(self: Vc<Self>) -> Vc<ModuleId> {
        let chunk_item = Vc::upcast(self);
        chunk_item.chunking_context().chunk_item_id(chunk_item)
    }
}
