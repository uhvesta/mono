use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use starlark::values::{Heap, Value};

use crate::input::{ChangeSet, SourceTree};
use crate::starlark::adapter::java::{JavaAdapter, JavaAdapterOutput};
use crate::starlark::adapter::module_json::{ModuleJsonAdapter, ModuleJsonAdapterOutput};
use crate::starlark::adapter::proto::{ProtoAdapter, ProtoAdapterOutput};
use crate::starlark::adapter::text::{TextAdapter, TextAdapterOutput};

pub(crate) mod java;
pub(crate) mod module_json;
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

    fn file_selectors(&self) -> &'static [AdapterFileSelector];

    fn prepare(&self, input: AdapterInput<'_>) -> Result<AdapterPreparedOutput>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum AdapterFileSelector {
    Ext(&'static str),
    Name(&'static str),
}

impl AdapterFileSelector {
    pub fn is_match(&self, path: &Path) -> bool {
        match self {
            Self::Ext(ext) => path.extension().and_then(|value| value.to_str()) == Some(*ext),
            Self::Name(name) => path.file_name().and_then(|value| value.to_str()) == Some(*name),
        }
    }
}

#[derive(Debug)]
pub(crate) enum AdapterPreparedOutput {
    Java(JavaAdapterOutput),
    ModuleJson(ModuleJsonAdapterOutput),
    Proto(ProtoAdapterOutput),
    Text(TextAdapterOutput),
}

impl AdapterPreparedOutput {
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Java(output) => output.is_empty(),
            Self::ModuleJson(output) => output.is_empty(),
            Self::Proto(output) => output.is_empty(),
            Self::Text(output) => output.is_empty(),
        }
    }

    pub fn alloc_context<'v>(&self, heap: Heap<'v>) -> Value<'v> {
        match self {
            Self::Java(output) => output.alloc_context(heap),
            Self::ModuleJson(output) => output.alloc_context(heap),
            Self::Proto(output) => output.alloc_context(heap),
            Self::Text(output) => output.alloc_context(heap),
        }
    }
}

#[derive(Default)]
pub(crate) struct AdapterRegistry {
    adapters: BTreeMap<&'static str, Arc<dyn FormatAdapter>>,
    selector_owners: BTreeMap<AdapterFileSelector, &'static str>,
}

impl AdapterRegistry {
    pub fn with_builtin_adapters() -> Self {
        let mut registry = Self::default();
        registry.register(JavaAdapter);
        registry.register(ModuleJsonAdapter);
        registry.register(ProtoAdapter);
        registry.register(TextAdapter);
        registry
    }

    pub fn register<A>(&mut self, adapter: A)
    where
        A: FormatAdapter + 'static,
    {
        let kind = adapter.kind();
        for selector in adapter.file_selectors() {
            if let Some(existing) = self.selector_owners.get(selector) {
                panic!("Starlark adapters `{existing}` and `{kind}` both claim selector {selector:?}");
            }
        }
        for selector in adapter.file_selectors() {
            self.selector_owners.insert(*selector, kind);
        }
        self.adapters.insert(kind, Arc::new(adapter));
    }

    pub fn get(&self, kind: &str) -> Option<Arc<dyn FormatAdapter>> {
        self.adapters.get(kind).cloned()
    }

    pub fn require(&self, kind: &str) -> Result<Arc<dyn FormatAdapter>> {
        self.get(kind)
            .ok_or_else(|| anyhow!("unknown Starlark adapter `{kind}`"))
    }
}

pub(crate) fn adapter_matches_changed_file(adapter: &dyn FormatAdapter, path: &Path, old_path: Option<&Path>) -> bool {
    adapter.file_selectors().iter().any(|selector| selector.is_match(path))
        || old_path.is_some_and(|old_path| {
            adapter
                .file_selectors()
                .iter()
                .any(|selector| selector.is_match(old_path))
        })
}

#[cfg(test)]
mod tests {
    use anyhow::Result;

    use super::*;

    struct TestAdapter {
        kind: &'static str,
        selectors: &'static [AdapterFileSelector],
    }

    impl FormatAdapter for TestAdapter {
        fn kind(&self) -> &'static str {
            self.kind
        }

        fn file_selectors(&self) -> &'static [AdapterFileSelector] {
            self.selectors
        }

        fn prepare(&self, _input: AdapterInput<'_>) -> Result<AdapterPreparedOutput> {
            unreachable!("selector uniqueness test does not prepare adapters")
        }
    }

    #[test]
    #[should_panic(expected = "both claim selector")]
    fn registry_rejects_duplicate_adapter_selectors() {
        static SELECTORS: &[AdapterFileSelector] = &[AdapterFileSelector::Ext("proto")];
        let mut registry = AdapterRegistry::default();
        registry.register(TestAdapter {
            kind: "first",
            selectors: SELECTORS,
        });
        registry.register(TestAdapter {
            kind: "second",
            selectors: SELECTORS,
        });
    }

    #[test]
    fn file_selector_matches_extension_or_file_name() {
        assert!(AdapterFileSelector::Ext("proto").is_match(Path::new("api/user.proto")));
        assert!(AdapterFileSelector::Name("module-info.json").is_match(Path::new("a/b/module-info.json")));
        assert!(!AdapterFileSelector::Name("module-info.json").is_match(Path::new("a/b/module.json")));
    }
}
