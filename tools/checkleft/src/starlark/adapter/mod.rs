use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use starlark::values::{Heap, Value};

use crate::input::{ChangeSet, SourceTree};
use crate::starlark::adapter::proto::{ProtoAdapter, ProtoAdapterOutput};
use crate::starlark::adapter::text::{TextAdapter, TextAdapterOutput};

pub(crate) mod proto;
pub(crate) mod text;

pub(crate) struct AdapterInput<'a> {
    pub changeset: &'a ChangeSet,
    pub tree: &'a dyn SourceTree,
    pub applies_to: &'a [String],
    pub package_scope: Option<&'a Path>,
}

pub(crate) trait FormatAdapter: Send + Sync {
    fn kind(&self) -> &'static str;

    fn prepare(&self, input: AdapterInput<'_>) -> Result<AdapterPreparedOutput>;
}

#[derive(Debug)]
pub(crate) enum AdapterPreparedOutput {
    Proto(ProtoAdapterOutput),
    Text(TextAdapterOutput),
}

impl AdapterPreparedOutput {
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Proto(output) => output.is_empty(),
            Self::Text(output) => output.is_empty(),
        }
    }

    pub fn alloc_context<'v>(&self, heap: Heap<'v>) -> Value<'v> {
        match self {
            Self::Proto(output) => output.alloc_context(heap),
            Self::Text(output) => output.alloc_context(heap),
        }
    }
}

#[derive(Default)]
pub(crate) struct AdapterRegistry {
    adapters: BTreeMap<&'static str, Arc<dyn FormatAdapter>>,
}

impl AdapterRegistry {
    pub fn with_builtin_adapters() -> Self {
        let mut registry = Self::default();
        registry.register(ProtoAdapter);
        registry.register(TextAdapter);
        registry
    }

    pub fn register<A>(&mut self, adapter: A)
    where
        A: FormatAdapter + 'static,
    {
        self.adapters.insert(adapter.kind(), Arc::new(adapter));
    }

    pub fn get(&self, kind: &str) -> Option<Arc<dyn FormatAdapter>> {
        self.adapters.get(kind).cloned()
    }

    pub fn require(&self, kind: &str) -> Result<Arc<dyn FormatAdapter>> {
        self.get(kind)
            .ok_or_else(|| anyhow!("unknown Starlark adapter `{kind}`"))
    }
}
